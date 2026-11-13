use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, SystemTime};

use opendal::options::WriteOptions;
use opendal::services::S3;
use opendal::{EntryMode, Metadata, Operator, Writer};

use crate::backend::{Backend, PutOptions, WriteCondition};
use crate::config::ConnectionConfig;
use crate::error::Error;
use crate::item::Item;
use crate::transfer::{CancelToken, ProgressSink};

/// OpenDAL 기반 S3 백엔드.
/// Operator는 내부가 Arc라 clone이 저렴하며, 병렬 다운로드가 청크별
/// 태스크에 복제해 쓴다.
#[derive(Clone)]
pub struct OpendalS3 {
    operator: Operator,
}

impl OpendalS3 {
    /// ConnectionConfig와 시크릿 키로 S3 Operator를 생성한다.
    /// 설정 값 검증을 먼저 해서 잘못된 입력이 전송 오류로 번지지 않게 한다.
    pub fn new(config: &ConnectionConfig, secret_access_key: &str) -> Result<Self, Error> {
        config.validate()?;
        let mut builder = S3::default()
            .bucket(&config.bucket)
            .region(&config.region)
            .access_key_id(&config.access_key_id)
            .secret_access_key(secret_access_key)
            // 환경 변수·AWS 설정 파일·EC2 메타데이터를 읽지 않도록 명시한다.
            .disable_config_load()
            .disable_ec2_metadata()
            .root(config.prefix.as_deref().unwrap_or("/"));

        if let Some(endpoint) = config.endpoint.as_deref() {
            builder = builder.endpoint(endpoint);
        }
        // OpenDAL 기본값이 path-style이다. 가상 호스트 스타일이 필요하면 켠다.
        if use_virtual_host_style(config) {
            builder = builder.enable_virtual_host_style();
        }

        // staticlib에서는 ctor가 실행되지 않을 수 있어 HTTP transport를 명시적으로 설치한다.
        // 이미 설치되어 있으면 무시된다(first-installed-wins).
        opendal::install_default();

        let operator = Operator::new(builder)?;
        Ok(Self { operator })
    }

    /// WriteCondition을 OpenDAL WriteOptions로 변환한다.
    /// 조건부 쓰기면 capability와 무관하게 쓰기 직전 stat(HEAD)으로 조건을
    /// 확인한다 — OpenDAL은 조건부 쓰기 capability를 모든 S3 계열 서비스에
    /// true로 보고해, 헤더를 조용히 무시하는 백엔드를 걸러낼 수 없다.
    /// 헤더만 믿으면 다른 클라이언트의 변경을 덮어쓰므로 SPEC 3.4(데이터
    /// 유실 금지)를 어긴다. 조건부 헤더도 함께 보내 지원하는 백엔드에서는
    /// stat~쓰기 사이의 경쟁까지 서버가 막게 한다.
    async fn condition_options(
        &self,
        key: &str,
        condition: &WriteCondition,
    ) -> Result<WriteOptions, Error> {
        match condition {
            WriteCondition::Any => {}
            WriteCondition::NotExists => match self.operator.stat(key).await {
                Ok(_) => {
                    return Err(Error::Conflict(format!("{key}가 이미 존재한다")));
                }
                Err(e) if e.kind() == opendal::ErrorKind::NotFound => {}
                Err(e) => return Err(Error::from_opendal(e, "stat", key)),
            },
            WriteCondition::IfMatch(etag) => match self.operator.stat(key).await {
                Ok(meta) => {
                    if meta.etag() != Some(etag.as_str()) {
                        return Err(Error::Conflict(format!("{key}의 ETag가 다르다")));
                    }
                }
                Err(e) if e.kind() == opendal::ErrorKind::NotFound => {
                    return Err(Error::Conflict(format!("{key}가 없다")));
                }
                Err(e) => return Err(Error::from_opendal(e, "stat", key)),
            },
        }
        let cap = self.operator.info().capability();
        let mut opts = WriteOptions::default();
        match condition {
            WriteCondition::Any => {}
            WriteCondition::NotExists => {
                if cap.write_with_if_not_exists {
                    opts.if_not_exists = true;
                }
            }
            WriteCondition::IfMatch(etag) => {
                if cap.write_with_if_match {
                    opts.if_match = Some(etag.clone());
                }
            }
        }
        Ok(opts)
    }
}

/// 가상 호스트 스타일을 쓸지 결정한다.
///
/// path_style이 아니어도 버킷 이름에 '.'이 있으면 path-style을 유지한다.
/// OpenDAL은 가상 호스트 방식에서 점 포함 버킷을 ConfigInvalid로 거부한다.
/// HTTPS 인증서 와일드카드가 점 하나 수준의 서브도메인만 커버하기 때문이며,
/// AWS도 점 포함 버킷에는 path-style을 권장한다.
fn use_virtual_host_style(config: &ConnectionConfig) -> bool {
    !config.path_style && !config.bucket.contains('.')
}

impl Backend for OpendalS3 {
    async fn list_folder(&self, prefix: &str) -> Result<Vec<Item>, Error> {
        // 원격에서는 폴더 바로 아래 목록을 한 번에 전부 받는다.
        // S3 응답 경계를 넘는 부분 읽기는 OpenDAL 목록 순서(CommonPrefixes
        // 먼저, Contents 나중) 때문에 항목이 누락될 수 있어 쓰지 않는다(SPEC 4.3).
        // 페이지 분할은 enumerate::Connection이 캐시에서 로컬로 한다.
        let entries = self
            .operator
            .list_with(prefix)
            .recursive(false)
            .await
            .map_err(|e| Error::from_opendal(e, "list", prefix))?;

        Ok(entries_to_items(entries, prefix))
    }

    async fn list_recursive(&self, prefix: &str) -> Result<Vec<Item>, Error> {
        let entries = self
            .operator
            .list_with(prefix)
            .recursive(true)
            .await
            .map_err(|e| Error::from_opendal(e, "list", prefix))?;

        Ok(entries_to_items(entries, prefix))
    }

    async fn stat(&self, key: &str) -> Result<Item, Error> {
        let meta = self
            .operator
            .stat(key)
            .await
            .map_err(|e| Error::from_opendal(e, "stat", key))?;
        if meta.mode() == EntryMode::DIR || key.ends_with('/') {
            Ok(Item::folder(key))
        } else {
            Ok(Item::file(
                key,
                meta.content_length(),
                meta.etag().map(str::to_string),
                last_modified_ms(&meta),
            ))
        }
    }

    async fn read_range(&self, key: &str, offset: u64, len: u64) -> Result<Vec<u8>, Error> {
        // reader()는 lazy라 생성 자체는 네트워크를 타지 않고,
        // read(range)가 Range GET 한 번을 보낸다.
        let reader = self
            .operator
            .reader(key)
            .await
            .map_err(|e| Error::from_opendal(e, "read", key))?;
        let buf = reader
            .read(offset..offset + len)
            .await
            .map_err(|e| Error::from_opendal(e, "read", key))?;
        Ok(buf.to_vec())
    }

    async fn create_dir(&self, key: &str) -> Result<(), Error> {
        // S3는 네이티브 create_dir이 없지만 Operator::new에 기본 적용된
        // SimulateLayer가 0바이트 `key/` 객체 쓰기로 흉내낸다.
        self.operator
            .create_dir(key)
            .await
            .map_err(|e| Error::from_opendal(e, "mkdir", key))
    }

    async fn copy(&self, from: &str, to: &str) -> Result<(), Error> {
        // OpenDAL copy는 크기에 따라 서버 측 단일/멀티파트 복사를 자동 선택한다.
        self.operator
            .copy(from, to)
            .await
            .map_err(|e| Error::from_opendal(e, "copy", from))?;
        Ok(())
    }

    async fn delete_keys(&self, keys: &[String]) -> Result<(), Error> {
        // Deleter가 S3 DeleteObjects 기준 요청당 최대 1,000개로 배치한다.
        self.operator
            .delete_iter(keys.to_vec())
            .await
            .map_err(|e| Error::from_opendal(e, "delete", ""))
    }

    async fn write_bytes(&self, key: &str, data: Vec<u8>) -> Result<(), Error> {
        self.operator
            .write(key, data)
            .await
            .map_err(|e| Error::from_opendal(e, "write", key))?;
        Ok(())
    }

    async fn put_file(
        &self,
        key: &str,
        src: &Path,
        opts: PutOptions,
        progress: &dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<(), Error> {
        let mut file = tokio::fs::File::open(src).await?;
        let size = file.metadata().await?.len();
        if cancel.is_cancelled() {
            return Err(cancelled_error());
        }
        progress.on_progress(0, size);

        let mut wopts = self.condition_options(key, &opts.condition).await?;
        if size <= opts.part_size {
            put_single(
                &self.operator,
                key,
                &mut file,
                size,
                wopts,
                progress,
                cancel,
            )
            .await
        } else {
            wopts.chunk = Some(opts.part_size as usize);
            wopts.concurrent = opts.concurrency.max(1);
            put_multipart(
                &self.operator,
                key,
                &mut file,
                size,
                wopts,
                progress,
                cancel,
            )
            .await
        }
    }

    async fn presign_read(&self, key: &str, expires: Duration) -> Result<String, Error> {
        // S3 presigned URL의 최대 만료는 7일(604,800초)이다.
        const MAX_EXPIRE: Duration = Duration::from_secs(604_800);
        if expires.is_zero() || expires > MAX_EXPIRE {
            return Err(Error::Other(format!(
                "presign 만료는 1~604800초여야 한다: {}초",
                expires.as_secs()
            )));
        }
        let req = self
            .operator
            .presign_read(key, expires)
            .await
            .map_err(|e| Error::from_opendal(e, "presign", key))?;
        // URL 전체(서명 포함)는 로그에 남기지 않는다(SPEC 4.6).
        log::info!("presigned URL을 만들었다 (만료 {}초)", expires.as_secs());
        Ok(req.uri().to_string())
    }
}

/// opendal 목록 엔트리를 Item으로 변환한다.
/// 목록 prefix 자기 자신(폴더 마커 객체)과 빈 키는 제외하고,
/// 같은 키가 중복으로 와도 첫 번째 것만 취한 뒤 키 순으로 정렬한다.
fn entries_to_items(entries: Vec<opendal::Entry>, prefix: &str) -> Vec<Item> {
    let mut seen = HashSet::with_capacity(entries.len());
    let mut items = Vec::with_capacity(entries.len());
    for entry in entries {
        let key = entry.path().trim_start_matches('/');
        // 목록 prefix 자기 자신(폴더 마커 객체)은 제외한다.
        if key.is_empty() || key == prefix {
            continue;
        }
        // 폴더 마커 객체와 CommonPrefix가 같은 키로 둘 다 올 수 있다.
        // 둘 다 같은 폴더이므로 첫 번째 것만 취한다(SPEC 3.2).
        if !seen.insert(key.to_string()) {
            continue;
        }
        items.push(to_item(key, entry.metadata()));
    }

    // CommonPrefixes와 Contents를 따로 반환하므로 스트림 순서가
    // 키 순서와 다를 수 있다. 반드시 정렬한다.
    items.sort_by(|a, b| a.key.cmp(&b.key));
    items
}

/// opendal 엔트리의 경로·메타데이터를 Item으로 변환한다.
fn to_item(key: &str, meta: &Metadata) -> Item {
    if meta.mode() == EntryMode::DIR {
        Item::folder(key)
    } else {
        Item::file(
            key,
            meta.content_length(),
            meta.etag().map(str::to_string),
            last_modified_ms(meta),
        )
    }
}

/// opendal Timestamp를 Unix epoch 밀리초로 변환한다.
fn last_modified_ms(meta: &Metadata) -> Option<i64> {
    meta.last_modified().map(|ts| {
        SystemTime::from(ts)
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or_default()
    })
}

fn cancelled_error() -> Error {
    Error::Cancelled("업로드가 취소됐다".to_string())
}

/// 취소·실패 시 원격에 객체를 남기지 않기 위해 부분 객체를 지운다.
async fn delete_quietly(operator: &Operator, key: &str) {
    let _ = operator.delete(key).await;
}

/// 단일 PUT 업로드. 파일 전체를 메모리에 읽는다(≤ part_size ≤ 수십 MB).
async fn put_single(
    operator: &Operator,
    key: &str,
    file: &mut tokio::fs::File,
    size: u64,
    wopts: WriteOptions,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<(), Error> {
    use tokio::io::AsyncReadExt;
    let mut data = Vec::with_capacity(size as usize);
    file.read_to_end(&mut data).await.map_err(Error::from)?;
    if cancel.is_cancelled() {
        return Err(cancelled_error());
    }
    // 요청이 서버에 도달한 뒤 취소될 수 있으므로, 쓰기 future를
    // 취소와 select로 묶고 취소가 이기면 부분 객체를 지운다.
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            delete_quietly(operator, key).await;
            return Err(cancelled_error());
        }
        r = operator.write_options(key, data, wopts) => r,
    };
    match result {
        Ok(_) => {
            if cancel.is_cancelled() {
                // complete 응답 수신과 취소가 경주했을 수 있다.
                delete_quietly(operator, key).await;
                return Err(cancelled_error());
            }
            progress.on_progress(size, size);
            Ok(())
        }
        Err(e) => Err(Error::from_opendal(e, "write", key)),
    }
}

/// 멀티파트 업로드. part_size 크기로 파일을 나눠 쓰고 close로 완결한다.
/// 어떤 경로로 끝나든(성공·실패·취소·호출 측 future drop) 미완료
/// 멀티파트가 남지 않게 WriterGuard가 abort를 보장한다.
async fn put_multipart(
    operator: &Operator,
    key: &str,
    file: &mut tokio::fs::File,
    size: u64,
    wopts: WriteOptions,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<(), Error> {
    let part_size = wopts.chunk.unwrap_or(16 * 1024 * 1024) as u64;
    let mut writer = match operator.writer_options(key, wopts).await {
        Ok(w) => WriterGuard::new(w),
        Err(e) => return Err(Error::from_opendal(e, "write", key)),
    };

    let mut submitted = 0u64;
    let feed_result: Result<(), Error> = async {
        loop {
            if cancel.is_cancelled() {
                return Err(cancelled_error());
            }
            let mut part = vec![0u8; part_size as usize];
            let n = read_part(file, &mut part).await?;
            if n == 0 {
                break;
            }
            part.truncate(n);
            writer
                .write(part)
                .await
                .map_err(|e| Error::from_opendal(e, "write", key))?;
            submitted += n as u64;
            progress.on_progress(submitted, size);
        }
        Ok(())
    }
    .await;
    if let Err(e) = feed_result {
        let _ = writer.abort().await;
        return Err(e);
    }

    // close도 complete 요청과 경주할 수 있으므로 취소와 select로 묶는다.
    let closed = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            let _ = writer.abort().await;
            delete_quietly(operator, key).await;
            return Err(cancelled_error());
        }
        r = writer.close() => r,
    };
    match closed {
        Ok(_) => {
            if cancel.is_cancelled() {
                delete_quietly(operator, key).await;
                return Err(cancelled_error());
            }
            writer.disarm();
            progress.on_progress(size, size);
            Ok(())
        }
        Err(e) => {
            let _ = writer.abort().await;
            Err(Error::from_opendal(e, "write", key))
        }
    }
}

/// 파일에서 최대 buf.len() 바이트를 읽는다. 파일 끝이면 더 짧게 반환한다.
async fn read_part(file: &mut tokio::fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    use tokio::io::AsyncReadExt;
    let mut filled = 0;
    while filled < buf.len() {
        let n = file.read(&mut buf[filled..]).await?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

/// Writer를 close/abort 없이 버려질 때(호출 측 future drop, 태스크 abort)
/// 멀티파트 업로드를 정리하는 가드. 명시적 abort/disarm 후에는 아무것도
/// 하지 않는다.
struct WriterGuard {
    inner: Option<Writer>,
}

impl WriterGuard {
    fn new(writer: Writer) -> Self {
        Self {
            inner: Some(writer),
        }
    }

    async fn write(&mut self, bs: Vec<u8>) -> opendal::Result<()> {
        self.inner
            .as_mut()
            .expect("writer는 abort 후에 쓸 수 없다")
            .write(bs)
            .await
    }

    async fn close(&mut self) -> opendal::Result<Metadata> {
        self.inner
            .as_mut()
            .expect("writer는 abort 후에 닫을 수 없다")
            .close()
            .await
    }

    async fn abort(&mut self) -> opendal::Result<()> {
        match self.inner.take() {
            Some(mut w) => w.abort().await,
            None => Ok(()),
        }
    }

    /// close 성공 후 호출. 이후 drop돼도 abort하지 않는다.
    fn disarm(&mut self) {
        self.inner = None;
    }
}

impl Drop for WriterGuard {
    fn drop(&mut self) {
        if let Some(mut writer) = self.inner.take() {
            // 런타임이 살아 있으면 abort를 백그라운드로 보낸다.
            // 태스크 abort 등으로 이미 종료 중이면 best-effort로 포기한다
            // (이 경우 서버 측 MPU 수명주기 규칙이 최종 정리를 맡는다).
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    let _ = writer.abort().await;
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Provider;

    fn config_with(path_style: bool, bucket: &str) -> ConnectionConfig {
        ConnectionConfig {
            id: "test".to_string(),
            display_name: "테스트".to_string(),
            provider: Provider::Aws,
            endpoint: None,
            region: "ap-northeast-2".to_string(),
            bucket: bucket.to_string(),
            prefix: None,
            path_style,
            access_key_id: "AKIAEXAMPLE".to_string(),
            chunk_size_mb: None,
            concurrency: None,
        }
    }

    #[test]
    fn virtual_host_style_needs_path_style_off_and_no_dot() {
        // 점 없는 AWS 버킷 → 가상 호스트 스타일.
        assert!(use_virtual_host_style(&config_with(false, "my-bucket")));
        // 점 포함 버킷 → OpenDAL이 가상 호스트에서 거부하므로 path-style.
        assert!(!use_virtual_host_style(&config_with(
            false,
            "assets.example.com"
        )));
        // path_style=true(MinIO 등) → 버킷과 무관하게 path-style.
        assert!(!use_virtual_host_style(&config_with(true, "my-bucket")));
        assert!(!use_virtual_host_style(&config_with(true, "a.b")));
    }
}
