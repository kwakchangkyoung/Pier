use std::collections::HashSet;
use std::time::SystemTime;

use opendal::services::S3;
use opendal::{EntryMode, Operator};

use crate::backend::Entry;
use crate::config::ConnectionConfig;
use crate::error::Error;

/// OpenDAL 기반 S3 백엔드. Phase 0에서는 루트 비재귀 목록만 제공한다.
pub struct OpendalS3 {
    operator: Operator,
}

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

    /// 루트 바로 아래 항목을 비재귀로 나열한다.
    pub async fn list_root(&self) -> Result<Vec<Entry>, Error> {
        let mut entries = Vec::new();
        let mut seen = HashSet::new();
        for opendal_entry in self.operator.list("/").await? {
            let entry = to_entry(&opendal_entry);
            // 루트 자기 자신(빈 key)과 폴더 마커/공통 접두사 중복을 제외한다.
            if entry.key.is_empty() || !seen.insert(entry.key.clone()) {
                continue;
            }
            entries.push(entry);
        }
        Ok(entries)
    }
}

fn to_entry(opendal_entry: &opendal::Entry) -> Entry {
    let metadata = opendal_entry.metadata();
    let is_dir = metadata.mode() == EntryMode::DIR;
    let key = opendal_entry.path().trim_start_matches('/').to_string();
    Entry {
        name: display_name(&key),
        key,
        is_dir,
        size: metadata.content_length(),
        etag: metadata.etag().map(str::to_string),
        last_modified: metadata.last_modified().map(SystemTime::from),
    }
}

/// key("photos/a.jpg", "photos/")에서 표시 이름(끝 '/' 제외 마지막 구간)을 뽑는다.
fn display_name(key: &str) -> String {
    let trimmed = key.trim_end_matches('/');
    match trimmed.rsplit('/').next() {
        Some(name) if !name.is_empty() => name.to_string(),
        _ => trimmed.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::display_name;

    #[test]
    fn display_name_cases() {
        assert_eq!(display_name("a.jpg"), "a.jpg");
        assert_eq!(display_name("photos/"), "photos");
        assert_eq!(display_name("photos/a.jpg"), "a.jpg");
        assert_eq!(
            display_name("photos/2024/report final.pdf"),
            "report final.pdf"
        );
        assert_eq!(display_name(""), "");
    }
}
