use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use pier_core::config::{ConnectionConfig, Provider};
use pier_core::enumerate::Connection as CoreConnection;
use pier_core::item::Item as CoreItem;
use pier_core::queue::{
    Direction, ErrorKind, JobError, JobSnapshot, JobState, QueueListener, TransferQueue,
};
use pier_core::s3_raw::S3Raw;
use pier_core::transfer::{
    CancelToken as CoreCancelToken, FolderUpload as CoreFolderUpload, ProgressSink,
    TransferOptions, UploadMode as CoreUploadMode,
};
use pier_core::Error as CoreError;
use pier_core::{Backend, OpendalS3};

uniffi::setup_scaffolding!();

/// 전체 코어가 공유하는 멀티스레드 tokio 런타임.
/// UniFFI가 async 메서드를 감싸는 async-compat의 폴백 런타임(단일
/// 스레드)에서 코어 코드를 직접 실행하면 tokio::fs·spawn·JoinSet이
/// 그 폴백 위에서 돌아 전송이 병목이 되므로, 모든 코어 호출은 이
/// 런타임에 spawn해서 실행한다. 전송 대기열도 같은 런타임을 쓴다.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("pier-core")
            .build()
            .expect("tokio 런타임 생성 실패")
    })
}

/// 코어 future를 공유 런타임에서 실행하고 결과를 기다린다.
/// 반환 타입이 'static이어야 spawn 가능하므로 코어 호출은 모두
/// 이 함수를 거친다.
async fn run_core<F, T>(future: F) -> Result<T, PierError>
where
    F: std::future::Future<Output = Result<T, CoreError>> + Send + 'static,
    T: Send + 'static,
{
    match runtime().spawn(future).await {
        Ok(result) => result.map_err(PierError::from),
        Err(join_err) => Err(PierError::Other {
            message: format!("백그라운드 작업이 중단됐다: {join_err}"),
        }),
    }
}

#[derive(Clone, Debug, uniffi::Record)]
pub struct PierConfig {
    /// "aws"|"r2"|"wasabi"|"minio"|"custom". 커스텀/없음은 "custom".
    pub provider: String,
    pub endpoint: Option<String>,
    pub region: String,
    pub bucket: String,
    pub prefix: Option<String>,
    pub path_style: bool,
    pub access_key_id: String,
    /// 전송 청크 크기(MB). None이면 프리셋 기본값.
    pub chunk_size_mb: Option<u32>,
    /// 동시 전송 수. None이면 프리셋 기본값.
    pub concurrency: Option<u32>,
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

/// 서비스 프리셋 (SPEC 4.1).
#[derive(Clone, Debug, uniffi::Record)]
pub struct PierPreset {
    pub id: String,
    pub display_name: String,
    /// true면 사용자가 엔드포인트를 직접 입력해야 한다.
    pub needs_endpoint: bool,
    /// true면 엔드포인트 생성에 계정 ID가 필요하다(R2).
    pub needs_account_id: bool,
    pub default_region: String,
    /// 선택지로 보여줄 리전 목록. 비어 있으면 자유 입력이다.
    pub regions: Vec<String>,
    pub path_style: bool,
    /// 전송 청크 크기 기본값(MB).
    pub chunk_size_mb: u32,
    /// 동시 전송 수 기본값.
    pub concurrency: u32,
}

/// 전송 방향.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum PierDirection {
    Download,
    Upload,
}

/// 대기열 작업 상태.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum PierJobState {
    Queued,
    Running,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

/// 작업 실패 분류.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum PierErrorKind {
    Authentication,
    Network,
    NotFound,
    Conflict,
    Cancelled,
    Other,
}

/// 대기열 작업의 실패 정보.
#[derive(Clone, Debug, uniffi::Record)]
pub struct PierJobError {
    pub kind: PierErrorKind,
    pub message: String,
}

/// 대기열 작업의 공개 스냅샷.
#[derive(Clone, Debug, uniffi::Record)]
pub struct PierJob {
    pub id: u64,
    pub direction: PierDirection,
    pub is_folder: bool,
    /// 표시용 이름(파일·폴더 이름).
    pub name: String,
    /// 요청한 원격 키 또는 접두사.
    pub remote_key: String,
    /// 요청한 로컬 경로(다운로드는 대상, 업로드는 원본).
    pub local_path: String,
    pub state: PierJobState,
    pub transferred: u64,
    pub total: u64,
    /// 최근 약 3초 이동 평균 전송 속도(바이트/초).
    pub bytes_per_sec: u64,
    pub files_done: u64,
    pub files_total: u64,
    pub error: Option<PierJobError>,
    /// 완료 시 결과 항목(업로드는 원격 stat, 다운로드는 로컬 stat).
    pub result_entry: Option<PierEntry>,
}

/// 객체 정보 조회 결과. stat 항목과 스토리지 클래스.
#[derive(Clone, Debug, uniffi::Record)]
pub struct PierObjectInfo {
    pub entry: PierEntry,
    pub storage_class: String,
}

/// 코어 로그 레벨. Info 이상만 싱크로 전달된다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum PierLogLevel {
    Error,
    Warn,
    Info,
    Debug,
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

impl From<JobSnapshot> for PierJob {
    fn from(job: JobSnapshot) -> Self {
        Self {
            id: job.id,
            direction: match job.direction {
                Direction::Download => PierDirection::Download,
                Direction::Upload => PierDirection::Upload,
            },
            is_folder: job.is_folder,
            name: job.name,
            remote_key: job.remote_key,
            local_path: job.local_path,
            state: match job.state {
                JobState::Queued => PierJobState::Queued,
                JobState::Running => PierJobState::Running,
                JobState::Paused => PierJobState::Paused,
                JobState::Completed => PierJobState::Completed,
                JobState::Failed => PierJobState::Failed,
                JobState::Cancelled => PierJobState::Cancelled,
            },
            transferred: job.transferred,
            total: job.total,
            bytes_per_sec: job.bytes_per_sec,
            files_done: job.files_done,
            files_total: job.files_total,
            error: job.error.map(pier_job_error),
            result_entry: job.result_entry.map(PierEntry::from),
        }
    }
}

fn pier_job_error(err: JobError) -> PierJobError {
    let kind = match err.kind {
        ErrorKind::Authentication => PierErrorKind::Authentication,
        ErrorKind::Network => PierErrorKind::Network,
        ErrorKind::NotFound => PierErrorKind::NotFound,
        ErrorKind::Conflict => PierErrorKind::Conflict,
        ErrorKind::Cancelled => PierErrorKind::Cancelled,
        ErrorKind::Other => PierErrorKind::Other,
    };
    PierJobError {
        kind,
        message: err.message,
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

/// provider 문자열을 코어 Provider로 변환한다.
/// 프리셋 id와 같은 값("aws" 등)을 받고, 알 수 없는 값은 커스텀으로 본다.
fn core_provider(provider: &str) -> Provider {
    match provider {
        "aws" => Provider::Aws,
        "r2" => Provider::R2,
        "wasabi" => Provider::Wasabi,
        "minio" => Provider::MinIO,
        _ => Provider::Custom,
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

/// 대기열 작업 변화 콜백. 백그라운드 스레드에서 호출되므로
/// Swift 구현은 스레드 안전해야 한다. 통지는 작업당 100ms로
/// 스로틀되고 상태 변화는 즉시 온다.
#[uniffi::export(with_foreign)]
pub trait PierJobListener: Send + Sync {
    fn on_job_update(&self, job: PierJob);
}

/// Swift의 PierJobListener를 코어의 QueueListener에 연결하는 어댑터.
struct JobListenerAdapter(Arc<dyn PierJobListener>);

impl QueueListener for JobListenerAdapter {
    fn on_job_update(&self, job: JobSnapshot) {
        self.0.on_job_update(PierJob::from(job));
    }
}

/// 코어 로그를 받는 싱크. `log` 매크로를 부른 스레드에서 동기로
/// 호출되므로 Swift 구현은 빠르게 반환해야 한다.
#[uniffi::export(with_foreign)]
pub trait PierLogSink: Send + Sync {
    fn on_log(&self, level: PierLogLevel, target: String, message: String);
}

/// Swift의 PierLogSink를 코어의 LogSink에 연결하는 어댑터.
struct LogSinkAdapter(Arc<dyn PierLogSink>);

impl pier_core::LogSink for LogSinkAdapter {
    fn log(&self, level: log::Level, target: &str, message: &str) {
        let level = match level {
            log::Level::Error => PierLogLevel::Error,
            log::Level::Warn => PierLogLevel::Warn,
            log::Level::Info => PierLogLevel::Info,
            log::Level::Debug | log::Level::Trace => PierLogLevel::Debug,
        };
        self.0
            .on_log(level, target.to_string(), message.to_string());
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
    conn: Arc<CoreConnection<OpendalS3>>,
    /// presign·storage class·멀티파트 정리용 원시 S3 백엔드.
    raw: Arc<S3Raw>,
    /// 연결당 최대 3개 작업의 전송 대기열.
    queue: TransferQueue<OpendalS3>,
    /// 연결 설정에서 만든 전송 옵션(직접 전송·대기열 공용).
    opts: TransferOptions,
    /// metadata.sqlite와 previews/의 부모 디렉터리.
    cache_dir: PathBuf,
}

#[uniffi::export(async_runtime = "tokio")]
impl PierConnection {
    /// cache_dir은 연결별 캐시 디렉터리. 내부에 metadata.sqlite를 둔다.
    ///
    /// 공유 런타임 안에서 연결·대기열을 만들어 대기열 작업이
    /// 이 런타임 위에서 돌게 하고, 이전 실행에서 abort하지 못한
    /// 멀티파트 업로드 정리를 백그라운드로 띄운다.
    #[uniffi::constructor]
    pub fn open(
        config: PierConfig,
        secret_access_key: String,
        cache_dir: String,
    ) -> Result<Arc<Self>, PierError> {
        let core_config = ConnectionConfig {
            id: String::new(),
            display_name: config.bucket.clone(),
            provider: core_provider(&config.provider),
            endpoint: config.endpoint,
            region: config.region,
            bucket: config.bucket,
            prefix: config.prefix,
            path_style: config.path_style,
            access_key_id: config.access_key_id,
            chunk_size_mb: config.chunk_size_mb,
            concurrency: config.concurrency,
        };
        let opts = core_config.transfer_options();

        let rt = runtime();
        // TransferQueue::new는 현재 스레드의 런타임 핸들을 잡는다 —
        // 대기열 작업이 공유 런타임 위에서 돌도록 enter 안에서 만든다.
        let _guard = rt.enter();
        let cache_dir = PathBuf::from(cache_dir);
        let conn = Arc::new(CoreConnection::open(
            &core_config,
            &secret_access_key,
            &cache_dir,
        )?);
        let mut raw = S3Raw::new(&core_config, &secret_access_key)?;
        raw.set_metadata_store(conn.metadata_store());
        let raw = Arc::new(raw);
        let queue = TransferQueue::new(Arc::clone(&conn), opts, Some(Arc::clone(&raw)))?;

        // 이전 실행에서 abort하지 못한 멀티파트 업로드를 정리한다.
        // 실패해도 연결 열기는 성공 — 범주만 로그(SPEC 4.6).
        let cleanup_raw = Arc::clone(&raw);
        let store = conn.metadata_store();
        drop(rt.spawn(async move {
            if let Err(e) = cleanup_raw.cleanup_pending_uploads(store.as_ref()).await {
                log::warn!("미완료 멀티파트 업로드 정리 실패 ({})", e.category());
            }
        }));

        Ok(Arc::new(Self {
            conn,
            raw,
            queue,
            opts,
            cache_dir,
        }))
    }

    /// 원격 루트 목록 조회 + 쓰기 권한(임시 객체 쓰기/삭제) 확인.
    /// 쓰기 권한이 없으면 can_write=false, 그 외 오류는 PierError.
    pub async fn test_connection(&self) -> Result<PierTestResult, PierError> {
        let conn = Arc::clone(&self.conn);
        let can_write = run_core(async move { conn.test_connection().await }).await?;
        Ok(PierTestResult { can_write })
    }

    /// parent 아래 names 중 이미 존재하는 항목(파일·폴더 모두,
    /// 이름은 끝 '/' 없이)을 반환한다. 없으면 결과에서 빠진다.
    pub async fn existing(
        &self,
        parent: String,
        names: Vec<String>,
    ) -> Result<Vec<PierEntry>, PierError> {
        let conn = Arc::clone(&self.conn);
        let items = run_core(async move { conn.existing(&parent, &names).await }).await?;
        Ok(items.into_iter().map(PierEntry::from).collect())
    }

    /// src_path 파일을 parent 아래 name으로 업로드한다.
    /// 전송 옵션은 연결 설정의 것을 쓴다.
    pub async fn upload(
        &self,
        src_path: String,
        parent: String,
        name: String,
        mode: PierUploadMode,
        progress: Arc<dyn PierProgress>,
        cancel: Arc<PierCancelToken>,
    ) -> Result<PierEntry, PierError> {
        let conn = Arc::clone(&self.conn);
        let mode = core_upload_mode(mode);
        let opts = self.opts;
        let token = cancel.inner.clone();
        let item = run_core(async move {
            conn.upload(
                Path::new(&src_path),
                &parent,
                &name,
                &mode,
                &opts,
                &ProgressAdapter(progress),
                &token,
            )
            .await
        })
        .await?;
        Ok(PierEntry::from(item))
    }

    /// src_dir를 parent 아래 폴더로 재귀 업로드한다(3.6 제외 규칙 적용).
    /// mode는 대상 폴더 생성과 모든 파일에 적용된다.
    /// 전송 옵션은 연결 설정의 것을 쓴다.
    pub async fn upload_folder(
        &self,
        src_dir: String,
        parent: String,
        mode: PierUploadMode,
        progress: Arc<dyn PierProgress>,
        cancel: Arc<PierCancelToken>,
    ) -> Result<PierFolderUpload, PierError> {
        let conn = Arc::clone(&self.conn);
        let mode = core_upload_mode(mode);
        let opts = self.opts;
        let token = cancel.inner.clone();
        let result = run_core(async move {
            conn.upload_folder(
                Path::new(&src_dir),
                &parent,
                &mode,
                &opts,
                &ProgressAdapter(progress),
                &token,
            )
            .await
        })
        .await?;
        Ok(PierFolderUpload::from(result))
    }

    /// parent 아래 name으로 새 폴더를 만든다. 대상이 이미 있으면 Conflict.
    pub async fn create_folder(
        &self,
        parent: String,
        name: String,
    ) -> Result<PierEntry, PierError> {
        let conn = Arc::clone(&self.conn);
        let item = run_core(async move { conn.create_folder(&parent, &name).await }).await?;
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
        let conn = Arc::clone(&self.conn);
        let item = run_core(async move { conn.rename(&key, &new_parent, &new_name).await }).await?;
        Ok(PierEntry::from(item))
    }

    /// 삭제. 폴더 키(끝 '/')면 하위 객체 전체가 삭제된다.
    pub async fn delete(&self, key: String) -> Result<(), PierError> {
        let conn = Arc::clone(&self.conn);
        run_core(async move { conn.delete(&key).await }).await
    }

    /// 중단된 폴더 이름 변경을 이어서 완료하고 처리 건수를 반환한다.
    pub async fn resume_pending_ops(&self) -> Result<u32, PierError> {
        let conn = Arc::clone(&self.conn);
        run_core(async move { conn.resume_pending_ops().await }).await
    }

    /// 폴더 목록 한 페이지. complete 캐시면 캐시에서 즉시 응답한다.
    pub async fn list_page(
        &self,
        folder: String,
        cursor: Option<String>,
    ) -> Result<PierListPage, PierError> {
        let conn = Arc::clone(&self.conn);
        let page =
            run_core(async move { conn.list_folder(&folder, cursor.as_deref()).await }).await?;
        Ok(PierListPage {
            items: page.items.into_iter().map(PierEntry::from).collect(),
            next_cursor: page.next_cursor,
            from_cache: page.from_cache,
        })
    }

    /// 폴더를 원격에서 다시 읽어 캐시를 교체하고 변경 사항을 반환한다.
    pub async fn refresh(&self, folder: String) -> Result<PierChanges, PierError> {
        let conn = Arc::clone(&self.conn);
        let changes = run_core(async move { conn.refresh(&folder).await }).await?;
        Ok(PierChanges {
            upserted: changes.upserted.into_iter().map(PierEntry::from).collect(),
            removed: changes.removed,
        })
    }

    /// 단일 아이템 메타데이터. 캐시에 있으면 캐시, 없으면 원격.
    pub async fn item(&self, key: String) -> Result<PierEntry, PierError> {
        let conn = Arc::clone(&self.conn);
        let item = run_core(async move { conn.stat(&key).await }).await?;
        Ok(PierEntry::from(item))
    }

    /// 객체를 dest_path 파일로 다운로드하고 쓴 바이트 수를 반환한다.
    /// 전송 옵션은 연결 설정의 것을 쓴다.
    pub async fn download(
        &self,
        key: String,
        dest_path: String,
        progress: Arc<dyn PierProgress>,
        cancel: Arc<PierCancelToken>,
    ) -> Result<u64, PierError> {
        let conn = Arc::clone(&self.conn);
        let opts = self.opts;
        let token = cancel.inner.clone();
        run_core(async move {
            conn.download(
                &key,
                Path::new(&dest_path),
                &opts,
                &ProgressAdapter(progress),
                &token,
            )
            .await
        })
        .await
    }

    /// prefix 폴더의 하위 객체를 모두 dest_dir 아래 `<폴더 이름>`으로
    /// 다운로드하고 결과를 반환한다. prefix는 '/'로 끝나는 폴더 키이며
    /// 루트("")는 거부한다. 전송 옵션은 연결 설정의 것을 쓴다.
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
        let conn = Arc::clone(&self.conn);
        let opts = self.opts;
        let token = cancel.inner.clone();
        let result = run_core(async move {
            conn.download_folder(
                &prefix,
                Path::new(&dest_dir),
                &opts,
                &ProgressAdapter(progress),
                &token,
            )
            .await
        })
        .await?;
        Ok(PierFolderDownload {
            local_path: result.local_path.to_string_lossy().into_owned(),
            file_count: result.file_count,
            total_bytes: result.total_bytes,
        })
    }

    /// 대기열 작업 변화 리스너를 등록한다.
    /// 통지는 공유 런타임의 백그라운드 스레드에서 온다.
    pub fn set_job_listener(&self, listener: Arc<dyn PierJobListener>) {
        self.queue
            .set_listener(Some(Arc::new(JobListenerAdapter(listener))));
    }

    /// key 객체를 dest_path 파일로 받는 작업을 넣고 작업 id를 반환한다.
    pub fn enqueue_download(&self, key: String, dest_path: String) -> u64 {
        self.queue.enqueue_download(&key, Path::new(&dest_path))
    }

    /// prefix 폴더 전체를 dest_dir 아래에 받는 작업을 넣는다.
    pub fn enqueue_download_folder(&self, prefix: String, dest_dir: String) -> u64 {
        self.queue
            .enqueue_download_folder(&prefix, Path::new(&dest_dir))
    }

    /// src_path 파일을 parent 아래 name으로 올리는 작업을 넣는다.
    pub fn enqueue_upload(
        &self,
        src_path: String,
        parent: String,
        name: String,
        mode: PierUploadMode,
    ) -> u64 {
        self.queue
            .enqueue_upload(Path::new(&src_path), &parent, &name, core_upload_mode(mode))
    }

    /// src_dir를 parent 아래 폴더로 올리는 작업을 넣는다.
    pub fn enqueue_upload_folder(
        &self,
        src_dir: String,
        parent: String,
        mode: PierUploadMode,
    ) -> u64 {
        self.queue
            .enqueue_upload_folder(Path::new(&src_dir), &parent, core_upload_mode(mode))
    }

    /// 실행 중 작업을 멈추고 Paused로 둔다. 없는 id는 무시한다.
    pub fn pause_job(&self, id: u64) {
        self.queue.pause_job(id);
    }

    /// Paused 작업을 대기열에 다시 넣는다. 없는 id는 무시한다.
    pub fn resume_job(&self, id: u64) {
        self.queue.resume_job(id);
    }

    /// 작업을 취소한다. 임시 파일·진행 중 멀티파트를 정리한다.
    /// 없는 id는 무시한다.
    pub fn cancel_job(&self, id: u64) {
        self.queue.cancel_job(id);
    }

    /// Failed 작업을 같은 자리에서 다시 시도한다. 없는 id는 무시한다.
    pub fn retry_job(&self, id: u64) {
        self.queue.retry_job(id);
    }

    /// 작업을 대기열에서 지운다. 없는 id는 무시한다.
    pub fn remove_job(&self, id: u64) {
        self.queue.remove_job(id);
    }

    /// 모든 작업의 스냅샷을 id 순(FIFO)으로 반환한다.
    pub fn jobs(&self) -> Vec<PierJob> {
        self.queue.jobs().into_iter().map(PierJob::from).collect()
    }

    /// key를 GET할 수 있는 presigned URL을 반환한다.
    /// 만료는 1초~7일(604,800초)이다.
    /// 반환된 URL 전체(서명 포함)는 로그에 남기지 않는다(SPEC 4.6).
    pub async fn presign(&self, key: String, expires_secs: u64) -> Result<String, PierError> {
        let conn = Arc::clone(&self.conn);
        let expires = Duration::from_secs(expires_secs);
        run_core(async move { conn.backend().presign_read(&key, expires).await }).await
    }

    /// 객체 정보(메타데이터 + 스토리지 클래스)를 조회한다.
    pub async fn info(&self, key: String) -> Result<PierObjectInfo, PierError> {
        let conn = Arc::clone(&self.conn);
        let raw = Arc::clone(&self.raw);
        let (item, storage_class) = run_core(async move {
            let item = conn.stat(&key).await?;
            let storage_class = raw.head_storage_class(&key).await?;
            Ok((item, storage_class))
        })
        .await?;
        Ok(PierObjectInfo {
            entry: PierEntry::from(item),
            storage_class,
        })
    }

    /// folder의 최근 목록 아래 하위 폴더를 미리 캐시한다.
    /// 최대 20개 폴더, 동시성 2. 반환값은 새로 받은 폴더 수다.
    pub async fn prefetch(&self, folder: String) -> Result<u32, PierError> {
        let conn = Arc::clone(&self.conn);
        run_core(async move { conn.prefetch(&folder, 20, 2).await }).await
    }

    /// key를 로컬 미리보기 파일로 받고 그 경로를 반환한다.
    /// 연결 캐시 디렉터리의 previews/ 아래에 두고 같은 ETag는 재사용한다.
    pub async fn preview(
        &self,
        key: String,
        progress: Arc<dyn PierProgress>,
        cancel: Arc<PierCancelToken>,
    ) -> Result<String, PierError> {
        let conn = Arc::clone(&self.conn);
        let cache_root = self.cache_dir.clone();
        let token = cancel.inner.clone();
        let path = run_core(async move {
            pier_core::preview_file(&conn, &key, &cache_root, &ProgressAdapter(progress), &token)
                .await
        })
        .await?;
        Ok(path.to_string_lossy().into_owned())
    }
}

/// 모든 서비스 프리셋을 표시 순서대로 반환한다 (SPEC 4.1).
#[uniffi::export]
pub fn presets() -> Vec<PierPreset> {
    pier_core::presets()
        .iter()
        .map(|p| PierPreset {
            id: p.id.to_string(),
            display_name: p.display_name.to_string(),
            needs_endpoint: p.needs_endpoint,
            needs_account_id: p.needs_account_id,
            default_region: p.default_region.to_string(),
            regions: p.regions.iter().map(|r| r.to_string()).collect(),
            path_style: p.path_style,
            chunk_size_mb: p.chunk_size_mb,
            concurrency: p.concurrency,
        })
        .collect()
}

/// 프리셋과 입력으로 엔드포인트를 만든다.
/// 사용자가 직접 입력해야 하거나 형식이 맞지 않으면 None을 반환한다.
#[uniffi::export]
pub fn preset_endpoint(
    preset_id: String,
    account_id: Option<String>,
    region: String,
) -> Option<String> {
    pier_core::preset_endpoint(&preset_id, account_id.as_deref(), &region)
}

/// 코어 로그를 Swift os_log로 전달할 싱크를 설치한다.
/// 프로세스당 한 번만 성공하고 이후 호출은 무시한다.
#[uniffi::export]
pub fn set_log_sink(sink: Arc<dyn PierLogSink>) {
    let _ = pier_core::logging::install(Box::new(LogSinkAdapter(sink)));
}
