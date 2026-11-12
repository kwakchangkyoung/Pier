/// 서비스 프리셋 종류. Phase 0에서는 식별용 열거형만 둔다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Aws,
    R2,
    Wasabi,
    MinIO,
    Custom,
}

/// SPEC 4.1의 연결 설정.
/// secret_access_key는 이 구조체에 넣지 않고 연결을 열 때 별도 인자로만 받는다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionConfig {
    pub id: String,
    pub display_name: String,
    pub provider: Provider,
    pub endpoint: Option<String>,
    pub region: String,
    pub bucket: String,
    pub prefix: Option<String>,
    pub path_style: bool,
    pub access_key_id: String,
}
