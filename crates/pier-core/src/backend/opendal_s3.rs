use std::collections::HashSet;
use std::path::Path;
use std::time::SystemTime;

use opendal::services::S3;
use opendal::{EntryMode, Metadata, Operator};
use tokio::io::AsyncWriteExt;

use crate::backend::Backend;
use crate::config::ConnectionConfig;
use crate::error::Error;
use crate::item::Item;

/// OpenDAL 기반 S3 백엔드.
pub struct OpendalS3 {
    operator: Operator,
}

/// read_to_file에서 범위 읽기 한 번에 가져오는 크기.
const READ_CHUNK_SIZE: u64 = 8 * 1024 * 1024;

impl OpendalS3 {
    /// ConnectionConfig와 시크릿 키로 S3 Operator를 생성한다.
    pub fn new(config: &ConnectionConfig, secret_access_key: &str) -> Result<Self, Error> {
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
        if !config.path_style {
            builder = builder.enable_virtual_host_style();
        }

        // staticlib에서는 ctor가 실행되지 않을 수 있어 HTTP transport를 명시적으로 설치한다.
        // 이미 설치되어 있으면 무시된다(first-installed-wins).
        opendal::install_default();

        let operator = Operator::new(builder)?;
        Ok(Self { operator })
    }

    /// read_to_file의 실제 구현. 실패하면 호출자가 부분 파일을 정리한다.
    async fn read_to_file_inner(&self, key: &str, dest: &Path) -> Result<u64, Error> {
        // 크기는 stat으로 먼저 얻는다(없는 키는 여기서 NotFound).
        let meta = self.operator.stat(key).await?;
        let size = meta.content_length();
        let reader = self.operator.reader(key).await?;

        let mut file = tokio::fs::File::create(dest).await?;
        let mut written = 0u64;
        let mut offset = 0u64;
        // Phase 1은 순차 단일 스트림이다. 범위 읽기로 나눠 쓰되 한 번에
        // 하나의 요청만 진행한다(병렬 범위 다운로드는 Phase 2).
        while offset < size {
            let end = (offset + READ_CHUNK_SIZE).min(size);
            let buf = reader.read(offset..end).await?;
            let bytes = buf.to_vec();
            file.write_all(&bytes).await?;
            written += bytes.len() as u64;
            offset = end;
        }
        if written != size {
            return Err(Error::Other(format!(
                "크기 불일치: {key} — 기대 {size}B, 실제 {written}B"
            )));
        }
        Ok(written)
    }
}

impl Backend for OpendalS3 {
    async fn list_folder(&self, prefix: &str) -> Result<Vec<Item>, Error> {
        // 원격에서는 폴더 바로 아래 목록을 한 번에 전부 받는다.
        // S3 응답 경계를 넘는 부분 읽기는 OpenDAL 목록 순서(CommonPrefixes
        // 먼저, Contents 나중) 때문에 항목이 누락될 수 있어 쓰지 않는다(SPEC 4.3).
        // 페이지 분할은 enumerate::Connection이 캐시에서 로컬로 한다.
        let entries = self.operator.list_with(prefix).recursive(false).await?;

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
        Ok(items)
    }

    async fn stat(&self, key: &str) -> Result<Item, Error> {
        let meta = self.operator.stat(key).await?;
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

    async fn read_to_file(&self, key: &str, dest: &Path) -> Result<u64, Error> {
        let result = self.read_to_file_inner(key, dest).await;
        if result.is_err() {
            // 부분 파일이 남지 않도록 삭제한다.
            let _ = tokio::fs::remove_file(dest).await;
        }
        result
    }
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
