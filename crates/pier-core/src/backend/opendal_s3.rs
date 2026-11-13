use std::collections::HashSet;
use std::time::SystemTime;

use opendal::services::S3;
use opendal::{EntryMode, Metadata, Operator};

use crate::backend::Backend;
use crate::config::ConnectionConfig;
use crate::error::Error;
use crate::item::Item;

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
