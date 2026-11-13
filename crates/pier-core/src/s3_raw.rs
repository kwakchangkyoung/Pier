//! 서명된 S3 REST 직접 호출과 이어올리기 가능한 멀티파트 업로드.
//!
//! OpenDAL이 제공하지 않는 작업(Create/UploadPart/ListParts/Complete/Abort
//! 멀티파트, storage class 조회)을 reqwest + reqsign SigV4로 직접 호출한다.
//! 엔드포인트·리전·path-style(점 포함 버킷은 path-style)·prefix 규칙은
//! `backend::opendal_s3`와 동일하다. 키는 연결 루트 기준 상대 키다.
//!
//! 비밀값(시크릿, Authorization 헤더, presigned URL)은 로그·에러·DB에
//! 남기지 않는다. 에러 메시지에는 HTTP 상태와 S3 코드, 작업·키만 넣는다.

use std::collections::BTreeMap;
use std::io::SeekFrom;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use http::header::{HeaderMap, HeaderValue, IF_MATCH, IF_NONE_MATCH};
use http::{HeaderName, Method, StatusCode};
use quick_xml::escape::unescape;
use quick_xml::events::BytesText;
use quick_xml::events::Event;
use quick_xml::Reader;
use reqsign_aws_v4::{Credential, RequestSigner};
use reqsign_core::hash::hex_sha256;
use reqsign_core::{Context, SignRequest};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::backend::WriteCondition;
use crate::config::ConnectionConfig;
use crate::error::Error;
use crate::key;
use crate::metadata::MetadataStore;
use crate::transfer::{CancelToken, ProgressSink, TransferOptions};

/// 멀티파트 파트 번호 → ETag.
type PartMap = BTreeMap<u32, String>;

/// 서명된 S3 REST 호출기. 시크릿은 메모리에만 둔다.
///
/// `Clone`은 내부 Arc들만 복제하므로 저렴하다 — 파트 업로드 태스크가
/// 복제해서 쓴다.
#[derive(Clone)]
pub struct S3Raw {
    client: reqwest::Client,
    ctx: Context,
    signer: Arc<RequestSigner>,
    credential: Credential,
    /// `https://host[:port]` 형태(끝 '/' 없음, 버킷 미포함).
    endpoint: String,
    bucket: String,
    /// 연결 루트 접두사. "" 또는 "a/b/" 형태.
    root: String,
    /// 가상 호스트 스타일 여부 (opendal_s3의 use_virtual_host_style과 동일 규칙).
    virtual_host: bool,
    /// 미완료 멀티파트 기록용 캐시. set_metadata_store로 주입한다.
    store: Option<Arc<Mutex<MetadataStore>>>,
}

/// 이어올리기 가능한 멀티파트 업로드의 진행 상태.
///
/// `upload_resumable`이 CreateMultipartUpload 직후부터 채우고, 파트가
/// 완료될 때마다 갱신한다. 일시정지·실패로 중단돼도 완료된 파트는 남아
/// 다음 호출이 이어간다. 앱 종료 시 사라지는 메모리 상태다 — 남은 서버 측
/// MPU는 metadata `pending_uploads`가 추적해 `cleanup_pending_uploads`가
/// 정리한다.
#[derive(Debug, Clone)]
pub struct ResumableUpload {
    /// 멀티파트 업로드 ID.
    pub upload_id: String,
    /// 대상 키(연결 루트 기준 상대 키).
    pub key: String,
    /// 파트 크기(바이트).
    pub part_size: u64,
    /// 완료된 파트 번호 → ETag.
    pub parts: PartMap,
    /// 시작 시점의 소스 파일 크기·수정 시각(epoch ms). 재개 시 둘 중
    /// 하나라도 바뀌었으면 기존 업로드를 abort하고 처음부터 시작한다.
    pub file_size: u64,
    pub file_mtime_ms: i64,
}

impl S3Raw {
    /// ConnectionConfig와 시크릿 키로 생성한다.
    /// 엔드포인트·path-style 결정 규칙은 OpendalS3와 동일하다.
    pub fn new(config: &ConnectionConfig, secret: &str) -> Result<Self, Error> {
        config.validate()?;
        let endpoint = match config.endpoint.as_deref() {
            Some(e) if e.starts_with("http://") || e.starts_with("https://") => {
                e.trim_end_matches('/').to_string()
            }
            Some(e) => format!("https://{}", e.trim_end_matches('/')),
            // OpenDAL과 같은 기본 엔드포인트 템플릿.
            None => format!("https://s3.{}.amazonaws.com", config.region),
        };
        // 점 포함 버킷은 가상 호스트에서 TLS 와일드카드가 안 맞아
        // path-style을 유지한다(opendal_s3와 동일).
        let virtual_host = !config.path_style && !config.bucket.contains('.');
        Ok(Self {
            client: reqwest::Client::new(),
            ctx: Context::new(),
            signer: Arc::new(RequestSigner::new("s3", &config.region)),
            credential: Credential {
                access_key_id: config.access_key_id.clone(),
                secret_access_key: secret.to_string(),
                session_token: None,
                expires_in: None,
            },
            endpoint,
            bucket: config.bucket.clone(),
            root: key::normalize_prefix(config.prefix.as_deref().unwrap_or("")),
            virtual_host,
            store: None,
        })
    }

    /// 미완료 멀티파트를 기록·정리할 메타데이터 캐시를 연다.
    /// upload_resumable이 Create 후 기록하고 Complete/Abort 후 지운다.
    pub fn set_metadata_store(&mut self, store: Arc<Mutex<MetadataStore>>) {
        self.store = Some(store);
    }

    /// 객체의 x-amz-storage-class 헤더 값을 반환한다. 헤더가 없으면
    /// S3 기본값인 "STANDARD"를 반환한다.
    pub async fn head_storage_class(&self, key: &str) -> Result<String, Error> {
        let resp = self
            .send("head", key, Method::HEAD, &[], HeaderMap::new(), Vec::new())
            .await?;
        let class = resp
            .headers()
            .get("x-amz-storage-class")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("STANDARD");
        Ok(class.to_string())
    }

    /// HEAD로 대상 객체의 ETag를 읽는다. 객체가 없으면 Error::NotFound.
    async fn head_etag(&self, key: &str) -> Result<Option<String>, Error> {
        let resp = self
            .send("head", key, Method::HEAD, &[], HeaderMap::new(), Vec::new())
            .await?;
        Ok(resp
            .headers()
            .get(http::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string))
    }

    /// 조건부 쓰기 사전 확인. OpenDAL이 조건부 쓰기 capability를 모든
    /// S3 계열 서비스에 true로 보고하듯이, If-Match/If-None-Match를 조용히
    /// 무시하는 백엔드는 응답만으로 걸러낼 수 없다. 헤더만 믿고 쓰면 다른
    /// 클라이언트의 변경을 덮어쓰므로(SPEC 3.4 데이터 유실 금지) 업로드
    /// 직전에 항상 HEAD로 조건을 확인한다. HEAD~쓰기 사이의 경쟁은
    /// Complete 요청에 붙는 조건부 헤더가 막는다(지원하는 백엔드에서).
    async fn check_write_condition(
        &self,
        key: &str,
        condition: &WriteCondition,
    ) -> Result<(), Error> {
        match condition {
            WriteCondition::Any => Ok(()),
            WriteCondition::NotExists => match self.head_etag(key).await {
                Ok(_) => Err(Error::Conflict(format!("{key}가 이미 존재한다"))),
                Err(Error::NotFound(_)) => Ok(()),
                Err(e) => Err(e),
            },
            WriteCondition::IfMatch(etag) => match self.head_etag(key).await {
                Ok(Some(remote)) if remote == *etag => Ok(()),
                Ok(_) => Err(Error::Conflict(format!("{key}의 ETag가 다르다"))),
                Err(Error::NotFound(_)) => Err(Error::Conflict(format!("{key}가 없다"))),
                Err(e) => Err(e),
            },
        }
    }

    /// metadata `pending_uploads`에 남아 있는 멀티파트 업로드를 모두
    /// abort하고 기록을 지운다. 정리한 건수를 반환한다.
    /// 서버에 이미 없는 업로드(NoSuchUpload)는 성공으로 본다.
    pub async fn cleanup_pending_uploads(
        &self,
        store: &Mutex<MetadataStore>,
    ) -> Result<u32, Error> {
        let pendings = lock_store(store)?.pending_uploads()?;
        let mut cleaned = 0u32;
        for (upload_id, key, _created_at) in pendings {
            match self.abort_multipart(&key, &upload_id).await {
                Ok(()) | Err(Error::NotFound(_)) => {
                    lock_store(store)?.remove_pending_upload(&upload_id)?;
                    cleaned += 1;
                }
                Err(e) => return Err(e),
            }
        }
        if cleaned > 0 {
            log::info!("남은 멀티파트 업로드 {cleaned}건을 정리했다");
        }
        Ok(cleaned)
    }

    /// pending_uploads에 업로드 ID를 기록한다. 저장소가 없으면 건너뛴다.
    fn record_pending(&self, upload_id: &str, key: &str) -> Result<(), Error> {
        if let Some(store) = &self.store {
            lock_store(store)?.add_pending_upload(upload_id, key, now_ms())?;
        }
        Ok(())
    }

    /// pending_uploads 기록을 지운다. 저장소가 없으면 건너뛴다.
    fn drop_pending(&self, upload_id: &str) -> Result<(), Error> {
        if let Some(store) = &self.store {
            lock_store(store)?.remove_pending_upload(upload_id)?;
        }
        Ok(())
    }

    /// 상대 키와 쿼리 쌍으로 요청 URL을 만든다.
    /// 경로 구간과 쿼리 값은 AWS 인코딩 규칙(unreserved만 평문)으로
    /// 인코딩해 서명 입력과 실제 요청이 일치하게 한다.
    fn build_url(&self, key: &str, query: &[(String, String)]) -> String {
        let full = format!("{}{key}", self.root);
        let path = aws_encode_path(&full);
        let mut url = if self.virtual_host {
            format!(
                "{}/{path}",
                self.endpoint
                    .replacen("://", &format!("://{}.", self.bucket), 1)
            )
        } else {
            format!("{}/{}/{path}", self.endpoint, self.bucket)
        };
        if !query.is_empty() {
            url.push('?');
            for (i, (k, v)) in query.iter().enumerate() {
                if i > 0 {
                    url.push('&');
                }
                url.push_str(&aws_encode(k));
                if !v.is_empty() {
                    url.push('=');
                    url.push_str(&aws_encode(v));
                }
            }
        }
        url
    }

    /// 서명한 요청을 보내고 성공(2xx) 응답을 반환한다.
    /// 비밀값이 에러 메시지에 섞이지 않게 reqwest 에러는 범주만 변환한다.
    async fn send(
        &self,
        op: &str,
        key: &str,
        method: Method,
        query: &[(String, String)],
        headers: HeaderMap,
        body: Vec<u8>,
    ) -> Result<reqwest::Response, Error> {
        let url = self.build_url(key, query);
        let url: reqwest::Url = url
            .parse()
            .map_err(|e| Error::Other(format!("URL 생성 실패: {e}")))?;

        let mut req = reqwest::Request::new(method, url);
        *req.headers_mut() = headers;
        // UNSIGNED-PAYLOAD를 받지 않는 S3 호환 서비스가 있어 본문의
        // 실제 SHA-256을 항상 넣는다.
        req.headers_mut().insert(
            HeaderName::from_static("x-amz-content-sha256"),
            HeaderValue::from_str(&hex_sha256(&body))
                .map_err(|e| Error::Other(format!("해시 헤더 오류: {e}")))?,
        );
        *req.body_mut() = Some(reqwest::Body::from(body));

        // reqsign은 http::request::Parts에 서명한다.
        let http_req: http::Request<reqwest::Body> = req
            .try_into()
            .map_err(|e: reqwest::Error| Error::Other(format!("요청 생성 실패: {op} ({e})")))?;
        let (mut parts, body) = http_req.into_parts();
        self.signer
            .sign_request(&self.ctx, &mut parts, Some(&self.credential), None)
            .await
            .map_err(|e| Error::Other(format!("서명 실패: {op} {key} ({e})")))?;

        let url: reqwest::Url = parts
            .uri
            .to_string()
            .parse()
            .map_err(|e| Error::Other(format!("URL 생성 실패: {e}")))?;
        let mut out = reqwest::Request::new(parts.method, url);
        *out.headers_mut() = parts.headers;
        *out.body_mut() = Some(body);

        let resp = self
            .client
            .execute(out)
            .await
            .map_err(|e| reqwest_error(e, op, key))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.bytes().await.unwrap_or_default();
            return Err(s3_error(status, &body, op, key));
        }
        Ok(resp)
    }

    /// CreateMultipartUpload: `POST key?uploads` → 업로드 ID.
    async fn create_multipart(&self, key: &str) -> Result<String, Error> {
        let query = vec![("uploads".to_string(), String::new())];
        let resp = self
            .send(
                "create_mpu",
                key,
                Method::POST,
                &query,
                HeaderMap::new(),
                Vec::new(),
            )
            .await?;
        let body = resp
            .bytes()
            .await
            .map_err(|e| reqwest_error(e, "create_mpu", key))?;
        let text = String::from_utf8_lossy(&body);
        xml_field(&text, "UploadId")
            .ok_or_else(|| Error::Other(format!("create_mpu {key}: 응답에 UploadId가 없다")))
    }

    /// UploadPart: `PUT key?partNumber=N&uploadId=U` → 파트 ETag.
    async fn upload_part(
        &self,
        key: &str,
        upload_id: &str,
        part_number: u32,
        body: Vec<u8>,
    ) -> Result<String, Error> {
        let query = vec![
            ("partNumber".to_string(), part_number.to_string()),
            ("uploadId".to_string(), upload_id.to_string()),
        ];
        let resp = self
            .send(
                "upload_part",
                key,
                Method::PUT,
                &query,
                HeaderMap::new(),
                body,
            )
            .await?;
        resp.headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .ok_or_else(|| Error::Other(format!("upload_part {key}: ETag 헤더가 없다")))
    }

    /// ListParts: `GET key?uploadId=U` → 서버에 완료된 파트 목록.
    /// 1,000개 단위 페이지를 part-number-marker로 따라간다.
    async fn list_parts(&self, key: &str, upload_id: &str) -> Result<PartMap, Error> {
        let mut parts = PartMap::new();
        let mut marker: Option<u32> = None;
        loop {
            let mut query = vec![("uploadId".to_string(), upload_id.to_string())];
            if let Some(m) = marker {
                query.push(("part-number-marker".to_string(), m.to_string()));
            }
            let resp = self
                .send(
                    "list_parts",
                    key,
                    Method::GET,
                    &query,
                    HeaderMap::new(),
                    Vec::new(),
                )
                .await?;
            let body = resp
                .bytes()
                .await
                .map_err(|e| reqwest_error(e, "list_parts", key))?;
            let (page, next) = parse_list_parts(&String::from_utf8_lossy(&body))?;
            parts.extend(page);
            match next {
                Some(m) => marker = Some(m),
                None => return Ok(parts),
            }
        }
    }

    /// CompleteMultipartUpload: 조건부 쓰기 헤더를 붙인다.
    /// 200 OK 본문에 <Error>가 오는 경우도 S3 규약대로 실패로 본다.
    async fn complete_multipart(
        &self,
        key: &str,
        upload_id: &str,
        parts: &PartMap,
        condition: &WriteCondition,
    ) -> Result<(), Error> {
        let mut headers = HeaderMap::new();
        match condition {
            WriteCondition::Any => {}
            WriteCondition::NotExists => {
                headers.insert(IF_NONE_MATCH, HeaderValue::from_static("*"));
            }
            WriteCondition::IfMatch(etag) => {
                headers.insert(
                    IF_MATCH,
                    HeaderValue::from_str(etag)
                        .map_err(|e| Error::Other(format!("ETag 헤더 오류: {e}")))?,
                );
            }
        }
        let query = vec![("uploadId".to_string(), upload_id.to_string())];
        let resp = self
            .send(
                "complete_mpu",
                key,
                Method::POST,
                &query,
                headers,
                complete_xml(parts).into_bytes(),
            )
            .await?;
        let body = resp
            .bytes()
            .await
            .map_err(|e| reqwest_error(e, "complete_mpu", key))?;
        if is_error_xml(&body) {
            return Err(s3_error(StatusCode::OK, &body, "complete_mpu", key));
        }
        Ok(())
    }

    /// AbortMultipartUpload: `DELETE key?uploadId=U`.
    /// 서버에 이미 없으면(404 NoSuchUpload) 호출자가 성공으로 처리한다.
    async fn abort_multipart(&self, key: &str, upload_id: &str) -> Result<(), Error> {
        let query = vec![("uploadId".to_string(), upload_id.to_string())];
        self.send(
            "abort_mpu",
            key,
            Method::DELETE,
            &query,
            HeaderMap::new(),
            Vec::new(),
        )
        .await?;
        Ok(())
    }
}

/// src를 key로 멀티파트 업로드한다.
///
/// - `state`가 Some이고 파일 크기·수정 시각·파트 크기가 그대로면
///   ListParts로 서버 상태를 대조해 남은 파트만 이어서 올린다.
///   서버에 업로드가 없으면(NoSuchUpload) 처음부터 시작한다.
///   파일 정보가 바뀌었으면 기존 업로드를 abort하고 처음부터 시작한다.
/// - 일시정지(cancel)면 진행 중인 파트 요청을 중단하고 완료된 파트만
///   state에 남긴다(abort하지 않는다). abort는 호출자가 `abort_upload`로
///   한다.
/// - Create 직후 upload ID를 metadata `pending_uploads`에 기록하고,
///   Complete 성공 후 지운다(S3Raw::set_metadata_store로 저장소 주입).
// 시그니처가 코어 내부 계약(P4-RAW)으로 고정돼 있어 인자를 구조체로 묶지 않는다.
#[allow(clippy::too_many_arguments)]
pub async fn upload_resumable(
    raw: &S3Raw,
    src: &Path,
    key: &str,
    condition: WriteCondition,
    opts: &TransferOptions,
    state: &mut Option<ResumableUpload>,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<(), Error> {
    if cancel.is_cancelled() {
        return Err(cancelled_error());
    }
    let meta = tokio::fs::metadata(src).await?;
    if !meta.is_file() {
        return Err(Error::Other(format!("{}는 파일이 아니다", src.display())));
    }
    let size = meta.len();
    if size == 0 {
        return Err(Error::Other(
            "빈 파일은 멀티파트 업로드할 수 없다".to_string(),
        ));
    }
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default();
    let part_size = part_size_for(size, opts.chunk_size);
    let part_count = size.div_ceil(part_size) as u32;

    // 이전 상태가 이번 파일·조건과 맞지 않으면 abort하고 처음부터.
    if let Some(prev) = state.take() {
        let stale = prev.key != key
            || prev.file_size != size
            || prev.file_mtime_ms != mtime_ms
            || prev.part_size != part_size;
        if !stale {
            *state = Some(prev);
        } else if let Err(e) = abort_upload(raw, &prev).await {
            // abort 실패해도 진행은 막지 않는다 — 남은 MPU는
            // pending_uploads 기록으로 다음 실행 때 정리된다.
            // 메시지에 키가 섞일 수 있어 범주만 로그한다(SPEC 4.6).
            log::warn!("이전 멀티파트 업로드 abort 실패 ({})", e.category());
        }
    }

    // 이어올릴 업로드가 있으면 ListParts로 서버 상태와 대조한다.
    // 서버에 없는 파트는 로컬 기록에서도 버려 다시 올린다.
    let mut gone = false;
    if let Some(s) = state.as_mut() {
        match raw.list_parts(&s.key, &s.upload_id).await {
            Ok(server_parts) => s.parts = server_parts,
            Err(Error::NotFound(_)) => gone = true,
            Err(e) => return Err(e),
        }
    }
    if gone {
        *state = None;
    }

    // 새 업로드 시작. 조건부 헤더를 무시하는 백엔드에서도 다른
    // 클라이언트의 변경을 덮어쓰지 않게 Create 직전에 HEAD로 본다.
    if state.is_none() {
        raw.check_write_condition(key, &condition).await?;
        let upload_id = raw.create_multipart(key).await?;
        let s = ResumableUpload {
            upload_id: upload_id.clone(),
            key: key.to_string(),
            part_size,
            parts: PartMap::new(),
            file_size: size,
            file_mtime_ms: mtime_ms,
        };
        raw.record_pending(&upload_id, key)?;
        log::debug!("멀티파트 업로드를 시작했다 (파트 {part_count}개)");
        *state = Some(s);
    }
    let s = state.as_mut().expect("state는 위에서 채웠다");

    // 남은 파트를 동시 업로드한다. 동시 요청 수는 세마포어로 제한한다.
    let mut done: u64 = s.parts.keys().map(|&n| part_len(n, part_size, size)).sum();
    progress.on_progress(done, size);

    let sem = Arc::new(Semaphore::new(opts.concurrency.max(1)));
    let mut set = JoinSet::new();
    for n in 1..=part_count {
        if s.parts.contains_key(&n) {
            continue;
        }
        let raw = raw.clone();
        let src = src.to_path_buf();
        let key = key.to_string();
        let upload_id = s.upload_id.clone();
        let sem = Arc::clone(&sem);
        let cancel = cancel.clone();
        set.spawn(async move {
            let _permit = sem
                .acquire()
                .await
                .map_err(|e| Error::Other(format!("전송 세마포어 오류: {e}")))?;
            if cancel.is_cancelled() {
                return Err(cancelled_error());
            }
            let etag = upload_one_part(&raw, &src, &key, &upload_id, n, part_size, size).await?;
            Ok((n, etag))
        });
    }

    loop {
        let joined = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                set.abort_all();
                return Err(cancelled_error());
            }
            joined = set.join_next() => joined,
        };
        match joined {
            None => break,
            Some(Ok(Ok((n, etag)))) => {
                s.parts.insert(n, etag);
                done += part_len(n, part_size, size);
                progress.on_progress(done, size);
                if cancel.is_cancelled() {
                    set.abort_all();
                    return Err(cancelled_error());
                }
            }
            Some(Ok(Err(err))) => {
                set.abort_all();
                return Err(err);
            }
            Some(Err(err)) => {
                set.abort_all();
                return Err(Error::Other(format!("업로드 작업 실패: {err}")));
            }
        }
    }

    // 모든 파트가 모이면 완결한다. 완결 직전에 서버 파트 목록을 다시
    // 읽어 진실로 삼는다 — 일시정지로 끊긴 요청이 서버에서 늦게 완료돼
    // 같은 번호 파트의 ETag가 달라졌을 수 있기 때문이다.
    let upload_id = s.upload_id.clone();
    let server_parts = raw.list_parts(&s.key, &upload_id).await?;
    if server_parts.len() != part_count as usize {
        return Err(Error::Other(format!(
            "complete_mpu 준비 중 서버 파트가 부족하다 ({}/{part_count}개)",
            server_parts.len()
        )));
    }
    // 업로드 도중(재개 포함) 원격 객체가 바뀌었을 수 있으므로 Complete
    // 직전에도 같은 확인을 한 번 더 한다.
    raw.check_write_condition(key, &condition).await?;
    raw.complete_multipart(key, &upload_id, &server_parts, &condition)
        .await?;
    raw.drop_pending(&upload_id)?;
    *state = None;
    progress.on_progress(size, size);
    Ok(())
}

/// 진행 중인 멀티파트 업로드를 abort하고 pending 기록을 지운다.
/// 서버에 이미 없는 업로드(NoSuchUpload)는 성공으로 본다.
pub async fn abort_upload(raw: &S3Raw, state: &ResumableUpload) -> Result<(), Error> {
    match raw.abort_multipart(&state.key, &state.upload_id).await {
        Ok(()) | Err(Error::NotFound(_)) => raw.drop_pending(&state.upload_id),
        Err(e) => Err(e),
    }
}

/// 멀티파트 파트 크기: max(chunk_size(최소 5MB 보장), ceil(size/10,000)을
/// 1MB 단위 올림). S3 멀티파트는 파트가 최대 10,000개·각 5GB 미만이다.
fn part_size_for(size: u64, chunk_size: u64) -> u64 {
    const MIN_PART: u64 = 5 * 1024 * 1024;
    const MAX_PARTS: u64 = 10_000;
    const MIB: u64 = 1024 * 1024;
    let base = chunk_size.max(MIN_PART);
    let needed = size.div_ceil(MAX_PARTS);
    let rounded = needed.div_ceil(MIB) * MIB;
    base.max(rounded)
}

/// 파트 번호 n의 바이트 길이(마지막 파트만 짧다).
fn part_len(n: u32, part_size: u64, size: u64) -> u64 {
    part_size.min(size.saturating_sub(u64::from(n - 1) * part_size))
}

/// 파일의 [offset, offset+len)을 읽어 한 파트를 업로드하고 ETag를 반환한다.
async fn upload_one_part(
    raw: &S3Raw,
    src: &Path,
    key: &str,
    upload_id: &str,
    n: u32,
    part_size: u64,
    size: u64,
) -> Result<String, Error> {
    let offset = u64::from(n - 1) * part_size;
    let len = part_len(n, part_size, size);
    let mut file = tokio::fs::File::open(src).await?;
    file.seek(SeekFrom::Start(offset)).await?;
    let mut buf = vec![0u8; len as usize];
    file.read_exact(&mut buf).await?;
    raw.upload_part(key, upload_id, n, buf).await
}

/// AWS URI 인코딩: unreserved(A-Z a-z 0-9 - . _ ~)만 평문으로 두고
/// 나머지 바이트는 %XX(대문자)로 인코딩한다.
fn aws_encode(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0f) as usize] as char);
            }
        }
    }
    out
}

/// 키를 구간별로 인코딩한다('/' 구분자는 유지).
fn aws_encode_path(key: &str) -> String {
    key.split('/').map(aws_encode).collect::<Vec<_>>().join("/")
}

/// 텍스트 이벤트의 엔티티를 디코딩한다.
/// MinIO는 ETag를 `&#34;`(따옴표) 엔티티로 감싸 보내는데, read_text는
/// 원문 그대로 돌려주므로 여기서 디코딩해야 이중 이스케이프를 막는다.
fn decode_text(text: &BytesText<'_>) -> Result<String, Error> {
    unescape(&text.xml10_content())
        .map(|s| s.into_owned())
        .map_err(|_| Error::Other("XML 엔티티 디코딩 실패".to_string()))
}

/// xml 문서에서 처음 나오는 `name` 요소의 텍스트를 반환한다.
fn xml_field(xml: &str, name: &str) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if e.name().as_ref() == name => {
                return reader
                    .read_text(e.name())
                    .ok()
                    .and_then(|t| decode_text(&t).ok());
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

/// 본문이 루트 요소가 <Error>인 S3 에러 문서인지 확인한다.
/// Complete 응답은 200 OK에도 에러 본문을 담을 수 있다.
fn is_error_xml(body: &[u8]) -> bool {
    let text = String::from_utf8_lossy(body);
    let mut reader = Reader::from_str(text.as_ref());
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => return e.name().as_ref() == "Error",
            Ok(Event::Eof) | Err(_) => return false,
            _ => {}
        }
    }
}

/// ListPartsResult XML을 파싱해 (파트 번호 → ETag)와 다음 페이지 마커를
/// 반환한다(IsTruncated일 때만 Some).
fn parse_list_parts(xml: &str) -> Result<(PartMap, Option<u32>), Error> {
    let bad = || Error::Other("list_parts: 응답 XML 파싱 실패".to_string());
    let mut reader = Reader::from_str(xml);
    let mut parts = PartMap::new();
    let mut truncated = false;
    let mut next_marker: Option<u32> = None;
    loop {
        match reader.read_event().map_err(|_| bad())? {
            Event::Start(e) => match e.name().as_ref() {
                "Part" => {
                    let (n, etag) = read_part_element(&mut reader)?;
                    parts.insert(n, etag);
                }
                "IsTruncated" => {
                    truncated = decode_text(&reader.read_text(e.name()).map_err(|_| bad())?)?
                        .eq_ignore_ascii_case("true");
                }
                "NextPartNumberMarker" => {
                    next_marker = decode_text(&reader.read_text(e.name()).map_err(|_| bad())?)?
                        .parse()
                        .ok();
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
    Ok((parts, truncated.then_some(next_marker).flatten()))
}

/// <Part> 요소 안의 PartNumber·ETag를 읽는다.
fn read_part_element(reader: &mut Reader<&[u8]>) -> Result<(u32, String), Error> {
    let bad = || Error::Other("list_parts: Part 요소 파싱 실패".to_string());
    let mut number = None;
    let mut etag = String::new();
    loop {
        match reader.read_event().map_err(|_| bad())? {
            Event::Start(e) => match e.name().as_ref() {
                "PartNumber" => {
                    number = Some(
                        decode_text(&reader.read_text(e.name()).map_err(|_| bad())?)?
                            .parse()
                            .map_err(|_| bad())?,
                    );
                }
                "ETag" => {
                    etag = decode_text(&reader.read_text(e.name()).map_err(|_| bad())?)?;
                }
                _ => {}
            },
            Event::End(e) if e.name().as_ref() == "Part" => break,
            Event::Eof => break,
            _ => {}
        }
    }
    match number {
        Some(n) => Ok((n, etag)),
        None => Err(bad()),
    }
}

/// CompleteMultipartUpload 요청 본문 XML.
fn complete_xml(parts: &PartMap) -> String {
    let mut s = String::from(
        r#"<CompleteMultipartUpload xmlns="http://s3.amazonaws.com/doc/2006-03-01/">"#,
    );
    for (n, etag) in parts {
        s.push_str("<Part><PartNumber>");
        s.push_str(&n.to_string());
        s.push_str("</PartNumber><ETag>");
        s.push_str(&xml_escape(etag));
        s.push_str("</ETag></Part>");
    }
    s.push_str("</CompleteMultipartUpload>");
    s
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// S3 에러 응답을 코어 Error 범주로 변환한다.
/// 메시지에는 상태 코드·작업·키·S3 코드만 넣는다(헤더·서명 값 금지).
fn s3_error(status: StatusCode, body: &[u8], op: &str, key: &str) -> Error {
    let code = parse_error_code(body)
        .unwrap_or_else(|| status.canonical_reason().unwrap_or("Unknown").to_string());
    let detail = format!("{}: {op} {key} ({code})", status.as_u16());
    // 에러 코드가 상태 코드보다 구체적이므로 코드를 먼저 본다.
    match code.as_str() {
        "SignatureDoesNotMatch"
        | "InvalidAccessKeyId"
        | "AccessDenied"
        | "InvalidToken"
        | "ExpiredToken" => Error::Authentication(detail),
        "NoSuchKey" | "NoSuchUpload" | "NoSuchBucket" | "NotFound" => Error::NotFound(detail),
        "PreconditionFailed" | "ConditionalRequestConflict" => Error::Conflict(detail),
        _ => match status.as_u16() {
            401 | 403 => Error::Authentication(detail),
            404 => Error::NotFound(detail),
            412 => Error::Conflict(detail),
            _ => Error::Other(detail),
        },
    }
}

/// <Error><Code>값을 뽑는다. 파싱 불가면 None.
fn parse_error_code(body: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    let mut reader = Reader::from_str(text.as_ref());
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if e.name().as_ref() == "Code" => {
                return reader
                    .read_text(e.name())
                    .ok()
                    .and_then(|t| decode_text(&t).ok());
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

/// reqwest 전송 오류를 코어 범주로 변환한다.
/// err.to_string()에는 URL(업로드 ID·키)이 섞일 수 있어 범주만 남긴다.
fn reqwest_error(err: reqwest::Error, op: &str, key: &str) -> Error {
    if err.is_connect() || err.is_timeout() {
        Error::Network(format!("{op} {key}: 연결 오류"))
    } else if err.is_request() || err.is_body() || err.is_decode() {
        Error::Network(format!("{op} {key}: 요청 중단"))
    } else {
        Error::Other(format!("{op} {key}: 요청 실패"))
    }
}

fn lock_store(store: &Mutex<MetadataStore>) -> Result<MutexGuard<'_, MetadataStore>, Error> {
    store
        .lock()
        .map_err(|e| Error::Other(format!("메타데이터 잠금 오류: {e}")))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

fn cancelled_error() -> Error {
    Error::Cancelled("업로드가 취소됐다".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aws_encode_keeps_unreserved() {
        assert_eq!(aws_encode("aZ09-._~"), "aZ09-._~");
    }

    #[test]
    fn aws_encode_encodes_specials_and_utf8() {
        // 공백은 %20('+' 아님), '+'는 %2B, '/'는 %2F다.
        assert_eq!(aws_encode("a b+c/d"), "a%20b%2Bc%2Fd");
        // 한글은 UTF-8 바이트 단위로 인코딩된다('한'=ED 95 9C).
        assert_eq!(aws_encode("한"), "%ED%95%9C");
        assert_eq!(aws_encode("="), "%3D");
        assert_eq!(aws_encode("&"), "%26");
    }

    #[test]
    fn aws_encode_path_preserves_slashes() {
        assert_eq!(aws_encode_path("a/b c/한+d"), "a/b%20c/%ED%95%9C%2Bd");
        // 빈 구간(연속 슬래시)도 그대로 유지한다.
        assert_eq!(aws_encode_path("a//b"), "a//b");
    }

    #[test]
    fn build_url_path_and_virtual_host() {
        let config = |endpoint: Option<&str>, path_style: bool, bucket: &str| ConnectionConfig {
            id: "t".to_string(),
            display_name: "t".to_string(),
            provider: crate::config::Provider::Custom,
            endpoint: endpoint.map(str::to_string),
            region: "us-east-1".to_string(),
            bucket: bucket.to_string(),
            prefix: None,
            path_style,
            access_key_id: "ak".to_string(),
            chunk_size_mb: None,
            concurrency: None,
        };
        let query = vec![
            ("uploadId".to_string(), "a+b/c".to_string()),
            ("partNumber".to_string(), "3".to_string()),
        ];

        // path-style: /버킷/인코딩된 키?쿼리
        let raw = S3Raw::new(&config(Some("http://127.0.0.1:9000"), true, "bkt"), "sk").unwrap();
        let url = raw.build_url("docs/p4-raw/한글 +.bin", &query);
        assert_eq!(
            url,
            "http://127.0.0.1:9000/bkt/docs/p4-raw/%ED%95%9C%EA%B8%80%20%2B.bin?uploadId=a%2Bb%2Fc&partNumber=3"
        );

        // 가상 호스트 스타일: 버킷이 호스트에 붙는다.
        let raw = S3Raw::new(&config(None, false, "bkt"), "sk").unwrap();
        let url = raw.build_url("k", &query);
        assert_eq!(
            url,
            "https://bkt.s3.us-east-1.amazonaws.com/k?uploadId=a%2Bb%2Fc&partNumber=3"
        );

        // 점 포함 버킷은 path_style=false여도 path-style을 유지한다.
        let raw = S3Raw::new(&config(None, false, "a.bkt"), "sk").unwrap();
        assert_eq!(
            raw.build_url("k", &[]),
            "https://s3.us-east-1.amazonaws.com/a.bkt/k"
        );

        // 연결 루트(prefix)가 있으면 상대 키 앞에 붙는다.
        let mut config = config(Some("http://127.0.0.1:9000"), true, "bkt");
        config.prefix = Some("root/dir".to_string());
        let raw = S3Raw::new(&config, "sk").unwrap();
        assert_eq!(
            raw.build_url("f.bin", &[]),
            "http://127.0.0.1:9000/bkt/root/dir/f.bin"
        );
    }

    #[test]
    fn part_size_for_rules() {
        const MIB: u64 = 1024 * 1024;
        // chunk_size가 5MB 미만이어도 5MB가 최소다.
        assert_eq!(part_size_for(100 * MIB, MIB), 5 * MIB);
        // 보통은 chunk_size 그대로.
        assert_eq!(part_size_for(100 * MIB, 8 * MIB), 8 * MIB);
        // chunk 8MB로는 50GB도 파트가 6,251개라 그대로다.
        assert_eq!(part_size_for(50_001 * MIB, 8 * MIB), 8 * MIB);
        // chunk로는 10,000개를 넘는 크기면 1MB 단위로 올려 키운다.
        assert_eq!(part_size_for(80_001 * MIB, 8 * MIB), 9 * MIB);
        // 정확히 10,000개면 늘리지 않는다.
        assert_eq!(part_size_for(80_000 * MIB, 8 * MIB), 8 * MIB);
    }

    #[test]
    fn complete_xml_lists_sorted_parts() {
        let mut parts = PartMap::new();
        parts.insert(2, "\"e2\"".to_string());
        parts.insert(1, "\"e1\"".to_string());
        let xml = complete_xml(&parts);
        let p1 = xml.find("<PartNumber>1<").unwrap();
        let p2 = xml.find("<PartNumber>2<").unwrap();
        assert!(p1 < p2, "파트는 번호 순이어야 한다");
        assert!(xml.contains("<ETag>\"e1\"</ETag>"));
    }

    #[test]
    fn parse_list_parts_basic() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListPartsResult>
  <Part><PartNumber>1</PartNumber><ETag>"e1"</ETag></Part>
  <Part><PartNumber>3</PartNumber><ETag>"e3"</ETag></Part>
  <IsTruncated>false</IsTruncated>
</ListPartsResult>"#;
        let (parts, next) = parse_list_parts(xml).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[&1], "\"e1\"");
        assert_eq!(parts[&3], "\"e3\"");
        assert_eq!(next, None);
    }

    #[test]
    fn parse_list_parts_truncated() {
        let xml = r#"<ListPartsResult>
  <Part><PartNumber>1</PartNumber><ETag>"e1"</ETag></Part>
  <IsTruncated>true</IsTruncated><NextPartNumberMarker>7</NextPartNumberMarker>
</ListPartsResult>"#;
        let (_, next) = parse_list_parts(xml).unwrap();
        assert_eq!(next, Some(7));
    }

    #[test]
    fn is_error_xml_detects_root_error() {
        assert!(is_error_xml(
            br#"<?xml version="1.0"?><Error><Code>PreconditionFailed</Code></Error>"#
        ));
        assert!(!is_error_xml(
            br#"<CompleteMultipartUploadResult><ETag>"e"</ETag></CompleteMultipartUploadResult>"#
        ));
    }

    #[test]
    fn s3_error_maps_categories() {
        let err = |code: &str| format!("<Error><Code>{code}</Code></Error>");
        assert!(matches!(
            s3_error(
                StatusCode::FORBIDDEN,
                err("AccessDenied").as_bytes(),
                "head",
                "k"
            ),
            Error::Authentication(_)
        ));
        assert!(matches!(
            s3_error(
                StatusCode::NOT_FOUND,
                err("NoSuchUpload").as_bytes(),
                "list_parts",
                "k"
            ),
            Error::NotFound(_)
        ));
        // 200 OK 본문의 <Error>도 코드로 분류된다.
        assert!(matches!(
            s3_error(
                StatusCode::OK,
                err("PreconditionFailed").as_bytes(),
                "complete_mpu",
                "k"
            ),
            Error::Conflict(_)
        ));
        assert!(matches!(
            s3_error(
                StatusCode::OK,
                err("AccessDenied").as_bytes(),
                "complete_mpu",
                "k"
            ),
            Error::Authentication(_)
        ));
        assert!(matches!(
            s3_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                b"oops",
                "upload_part",
                "k"
            ),
            Error::Other(_)
        ));
    }
}
