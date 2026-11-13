use std::path::Path;
use std::sync::Arc;

use pier_core::config::{ConnectionConfig, Provider};
use pier_core::enumerate::Connection as CoreConnection;
use pier_core::item::Item as CoreItem;
use pier_core::transfer::{
    CancelToken as CoreCancelToken, FolderUpload as CoreFolderUpload, ProgressSink,
    TransferOptions, UploadMode as CoreUploadMode,
};
use pier_core::Error as CoreError;

uniffi::setup_scaffolding!();

#[derive(Clone, Debug, uniffi::Record)]
pub struct PierConfig {
    pub endpoint: Option<String>,
    pub region: String,
    pub bucket: String,
    pub prefix: Option<String>,
    pub path_style: bool,
    pub access_key_id: String,
}

#[derive(Clone, Debug, uniffi::Record)]
pub struct PierEntry {
    pub identifier: String,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub etag: Option<String>,
    pub last_modified_ms: Option<i64>,
    pub storage_class: Option<String>,
}

#[derive(Clone, Debug, uniffi::Record)]
pub struct PierListPage {
    pub items: Vec<PierEntry>,
    pub next_cursor: Option<String>,
    pub from_cache: bool,
}

#[derive(Clone, Debug, uniffi::Record)]
pub struct PierChanges {
    pub upserted: Vec<PierEntry>,
    pub removed: Vec<String>,
}

/// 폴더 다운로드 결과.
#[derive(Clone, Debug, uniffi::Record)]
pub struct PierFolderDownload {
    /// 실제로 만든 로컬 폴더 경로(dest_dir/<폴더 이름> 또는 " (N)"이 붙은 이름).
    pub local_path: String,
    /// 다운로드한 파일 수.
    pub file_count: u64,
    /// 다운로드한 파일들의 총 바이트.
    pub total_bytes: u64,
}

/// test_connection 결과. can_write는 임시 객체 쓰기·삭제가 성공했는지다.
#[derive(Clone, Debug, uniffi::Record)]
pub struct PierTestResult {
    pub can_write: bool,
}

/// 폴더 업로드 결과.
#[derive(Clone, Debug, uniffi::Record)]
pub struct PierFolderUpload {
    /// 업로드한 파일 수(폴더 마커 제외).
    pub uploaded: u64,
    /// 제외 규칙·심볼릭 링크로 건너뛴 항목 수.
    pub ignored: u64,
    /// 충돌 모드 때문에 건너뛴 파일 수.
    pub skipped: u64,
    /// 업로드 대상 파일들의 총 바이트.
    pub total_bytes: u64,
}

/// 업로드 충돌 모드 (SPEC 3.4).
#[derive(Clone, Debug, uniffi::Enum)]
pub enum PierUploadMode {
    /// 대상이 이미 있으면 Conflict 오류.
    Fail,
    /// 덮어쓴다. if_match가 있으면 그 ETag와 현재 ETag가 같을 때만 쓴다.
    Replace { if_match: Option<String> },
    /// 비어 있는 이름(`이름 (2).확장자`…)을 골라 함께 올린다.
    KeepBoth,
}

#[derive(Debug, uniffi::Error)]
pub enum PierError {
    Authentication { message: String },
    Network { message: String },
    NotFound { message: String },
    Conflict { message: String },
    Cancelled { message: String },
    Other { message: String },
}

impl std::fmt::Display for PierError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Authentication { message } => write!(f, "authentication failed: {message}"),
            Self::Network { message } => write!(f, "network error: {message}"),
            Self::NotFound { message } => write!(f, "not found: {message}"),
            Self::Conflict { message } => write!(f, "conflict: {message}"),
            Self::Cancelled { message } => write!(f, "cancelled: {message}"),
            Self::Other { message } => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for PierError {}

impl From<CoreError> for PierError {
    fn from(err: CoreError) -> Self {
        let message = err.to_string();
        match err {
            CoreError::Authentication(_) => Self::Authentication { message },
            CoreError::Network(_) => Self::Network { message },
            CoreError::NotFound(_) => Self::NotFound { message },
            CoreError::Conflict(_) => Self::Conflict { message },
            CoreError::Cancelled(_) => Self::Cancelled { message },
            CoreError::Other(_) => Self::Other { message },
        }
    }
}

impl From<CoreItem> for PierEntry {
    fn from(item: CoreItem) -> Self {
        Self {
            identifier: item.key,
            name: item.name,
            is_dir: item.is_dir,
            size: item.size,
            etag: item.etag,
            last_modified_ms: item.last_modified_ms,
            storage_class: item.storage_class,
        }
    }
}

impl From<CoreFolderUpload> for PierFolderUpload {
    fn from(result: CoreFolderUpload) -> Self {
        Self {
            uploaded: result.uploaded,
            ignored: result.ignored,
            skipped: result.skipped,
            total_bytes: result.total_bytes,
        }
    }
}

/// PierUploadMode를 코어 UploadMode로 변환한다.
fn core_upload_mode(mode: PierUploadMode) -> CoreUploadMode {
    match mode {
        PierUploadMode::Fail => CoreUploadMode::Fail,
        PierUploadMode::Replace { if_match } => CoreUploadMode::Replace { if_match },
        PierUploadMode::KeepBoth => CoreUploadMode::KeepBoth,
    }
}

/// 다운로드 진행률 콜백. Swift 측에서 구현하는 프로토콜에 대응한다.
///
/// `on_progress`는 코어의 tokio 런타임(백그라운드 스레드)에서 호출되므로
/// Swift 구현은 스레드 안전해야 하며, UI 갱신은 반드시 메인 스레드로
/// 디스패치해야 한다.
#[uniffi::export(with_foreign)]
pub trait PierProgress: Send + Sync {
    fn on_progress(&self, transferred: u64, total: u64);
}

/// Swift의 PierProgress를 코어의 ProgressSink에 연결하는 어댑터.
struct ProgressAdapter(Arc<dyn PierProgress>);

impl ProgressSink for ProgressAdapter {
    fn on_progress(&self, transferred: u64, total: u64) {
        self.0.on_progress(transferred, total);
    }
}

/// 다운로드 취소 토큰. 여러 전송을 하나로 묶어 취소할 때 쓴다.
#[derive(uniffi::Object)]
pub struct PierCancelToken {
    inner: CoreCancelToken,
}

#[uniffi::export]
impl PierCancelToken {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: CoreCancelToken::new(),
        })
    }

    /// 취소를 요청한다. 진행 중인 전송은 가능한 빨리 멈춘다.
    pub fn cancel(&self) {
        self.inner.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }
}

#[derive(uniffi::Object)]
pub struct PierConnection {
    conn: CoreConnection,
}

#[uniffi::export(async_runtime = "tokio")]
impl PierConnection {
    /// cache_dir은 연결별 캐시 디렉터리. 내부에 metadata.sqlite를 둔다.
    #[uniffi::constructor]
    pub fn open(
        config: PierConfig,
        secret_access_key: String,
        cache_dir: String,
    ) -> Result<Arc<Self>, PierError> {
        let core_config = ConnectionConfig {
            id: String::new(),
            display_name: config.bucket.clone(),
            provider: Provider::Custom,
            endpoint: config.endpoint,
            region: config.region,
            bucket: config.bucket,
            prefix: config.prefix,
            path_style: config.path_style,
            access_key_id: config.access_key_id,
        };
        let conn = CoreConnection::open(&core_config, &secret_access_key, Path::new(&cache_dir))?;
        Ok(Arc::new(Self { conn }))
    }

    /// 원격 루트 목록 조회 + 쓰기 권한(임시 객체 쓰기/삭제) 확인.
    /// 쓰기 권한이 없으면 can_write=false, 그 외 오류는 PierError.
    pub async fn test_connection(&self) -> Result<PierTestResult, PierError> {
        let can_write = self.conn.test_connection().await?;
        Ok(PierTestResult { can_write })
    }

    /// parent 아래 names 중 이미 존재하는 항목(파일·폴더 모두,
    /// 이름은 끝 '/' 없이)을 반환한다. 없으면 결과에서 빠진다.
    pub async fn existing(
        &self,
        parent: String,
        names: Vec<String>,
    ) -> Result<Vec<PierEntry>, PierError> {
        let items = self.conn.existing(&parent, &names).await?;
        Ok(items.into_iter().map(PierEntry::from).collect())
    }

    /// src_path 파일을 parent 아래 name으로 업로드한다.
    /// 기본 전송 옵션(8MB 청크·동시성 8)을 쓴다.
    pub async fn upload(
        &self,
        src_path: String,
        parent: String,
        name: String,
        mode: PierUploadMode,
        progress: Arc<dyn PierProgress>,
        cancel: Arc<PierCancelToken>,
    ) -> Result<PierEntry, PierError> {
        let mode = core_upload_mode(mode);
        let opts = TransferOptions::default();
        let sink = ProgressAdapter(progress);
        let item = self
            .conn
            .upload(
                Path::new(&src_path),
                &parent,
                &name,
                &mode,
                &opts,
                &sink,
                &cancel.inner,
            )
            .await?;
        Ok(PierEntry::from(item))
    }

    /// src_dir를 parent 아래 폴더로 재귀 업로드한다(3.6 제외 규칙 적용).
    /// mode는 대상 폴더 생성과 모든 파일에 적용된다.
    /// 기본 전송 옵션(8MB 청크·동시성 8)을 쓴다.
    pub async fn upload_folder(
        &self,
        src_dir: String,
        parent: String,
        mode: PierUploadMode,
        progress: Arc<dyn PierProgress>,
        cancel: Arc<PierCancelToken>,
    ) -> Result<PierFolderUpload, PierError> {
        let mode = core_upload_mode(mode);
        let opts = TransferOptions::default();
        let sink = ProgressAdapter(progress);
        let result = self
            .conn
            .upload_folder(
                Path::new(&src_dir),
                &parent,
                &mode,
                &opts,
                &sink,
                &cancel.inner,
            )
            .await?;
        Ok(PierFolderUpload::from(result))
    }

    /// parent 아래 name으로 새 폴더를 만든다. 대상이 이미 있으면 Conflict.
    pub async fn create_folder(
        &self,
        parent: String,
        name: String,
    ) -> Result<PierEntry, PierError> {
        let item = self.conn.create_folder(&parent, &name).await?;
        Ok(PierEntry::from(item))
    }

    /// 이름 변경·이동 공통. 폴더는 new_name에 '/' 없이 받는다(코어가 붙임).
    /// 대상이 이미 있으면 Conflict.
    pub async fn rename(
        &self,
        key: String,
        new_parent: String,
        new_name: String,
    ) -> Result<PierEntry, PierError> {
        let item = self.conn.rename(&key, &new_parent, &new_name).await?;
        Ok(PierEntry::from(item))
    }

    /// 삭제. 폴더 키(끝 '/')면 하위 객체 전체가 삭제된다.
    pub async fn delete(&self, key: String) -> Result<(), PierError> {
        self.conn.delete(&key).await?;
        Ok(())
    }

    /// 중단된 폴더 이름 변경을 이어서 완료하고 처리 건수를 반환한다.
    pub async fn resume_pending_ops(&self) -> Result<u32, PierError> {
        Ok(self.conn.resume_pending_ops().await?)
    }

    /// 폴더 목록 한 페이지. complete 캐시면 캐시에서 즉시 응답한다.
    pub async fn list_page(
        &self,
        folder: String,
        cursor: Option<String>,
    ) -> Result<PierListPage, PierError> {
        let page = self.conn.list_folder(&folder, cursor.as_deref()).await?;
        Ok(PierListPage {
            items: page.items.into_iter().map(PierEntry::from).collect(),
            next_cursor: page.next_cursor,
            from_cache: page.from_cache,
        })
    }

    /// 폴더를 원격에서 다시 읽어 캐시를 교체하고 변경 사항을 반환한다.
    pub async fn refresh(&self, folder: String) -> Result<PierChanges, PierError> {
        let changes = self.conn.refresh(&folder).await?;
        Ok(PierChanges {
            upserted: changes.upserted.into_iter().map(PierEntry::from).collect(),
            removed: changes.removed,
        })
    }

    /// 단일 아이템 메타데이터. 캐시에 있으면 캐시, 없으면 원격.
    pub async fn item(&self, key: String) -> Result<PierEntry, PierError> {
        let item = self.conn.stat(&key).await?;
        Ok(PierEntry::from(item))
    }

    /// 객체를 dest_path 파일로 다운로드하고 쓴 바이트 수를 반환한다.
    /// 기본 전송 옵션(8MB 청크·동시성 8)을 쓴다.
    pub async fn download(
        &self,
        key: String,
        dest_path: String,
        progress: Arc<dyn PierProgress>,
        cancel: Arc<PierCancelToken>,
    ) -> Result<u64, PierError> {
        let opts = TransferOptions::default();
        let sink = ProgressAdapter(progress);
        let written = self
            .conn
            .download(&key, Path::new(&dest_path), &opts, &sink, &cancel.inner)
            .await?;
        Ok(written)
    }

    /// prefix 폴더의 하위 객체를 모두 dest_dir 아래 `<폴더 이름>`으로
    /// 다운로드하고 결과를 반환한다. prefix는 '/'로 끝나는 폴더 키이며
    /// 루트("")는 거부한다. 기본 전송 옵션(8MB 청크·동시성 8)을 쓴다.
    pub async fn download_folder(
        &self,
        prefix: String,
        dest_dir: String,
        progress: Arc<dyn PierProgress>,
        cancel: Arc<PierCancelToken>,
    ) -> Result<PierFolderDownload, PierError> {
        if prefix.is_empty() {
            return Err(PierError::Other {
                message: "루트 폴더는 다운로드할 수 없다".to_string(),
            });
        }
        let opts = TransferOptions::default();
        let sink = ProgressAdapter(progress);
        let result = self
            .conn
            .download_folder(&prefix, Path::new(&dest_dir), &opts, &sink, &cancel.inner)
            .await?;
        Ok(PierFolderDownload {
            local_path: result.local_path.to_string_lossy().into_owned(),
            file_count: result.file_count,
            total_bytes: result.total_bytes,
        })
    }
}
