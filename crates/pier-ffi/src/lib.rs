use std::sync::Arc;

use pier_core::backend::opendal_s3::OpendalS3;
use pier_core::backend::Backend;
use pier_core::config::{ConnectionConfig, Provider};
use pier_core::item::Item as CoreItem;
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
}

#[derive(Debug, uniffi::Error)]
pub enum PierError {
    Authentication { message: String },
    Network { message: String },
    NotFound { message: String },
    Other { message: String },
}

impl std::fmt::Display for PierError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Authentication { message } => write!(f, "authentication failed: {message}"),
            Self::Network { message } => write!(f, "network error: {message}"),
            Self::NotFound { message } => write!(f, "not found: {message}"),
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
        }
    }
}

#[derive(uniffi::Object)]
pub struct PierConnection {
    backend: OpendalS3,
}

#[uniffi::export(async_runtime = "tokio")]
impl PierConnection {
    #[uniffi::constructor]
    pub fn open(config: PierConfig, secret_access_key: String) -> Result<Arc<Self>, PierError> {
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
        let backend = OpendalS3::new(&core_config, &secret_access_key)?;
        Ok(Arc::new(Self { backend }))
    }

    pub async fn list_root(&self) -> Result<Vec<PierEntry>, PierError> {
        // 루트 바로 아래 전체를 한 번에 받아 PierEntry로 변환한다.
        // (캐시 도입은 Phase 2에서 cacheDir 인자와 함께 추가)
        let items = self.backend.list_folder("").await?;
        Ok(items.into_iter().map(PierEntry::from).collect())
    }
}
