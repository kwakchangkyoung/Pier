use crate::error::Error;
use crate::transfer::TransferOptions;

/// 서비스 종류. 프리셋과 1:1로 대응한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Aws,
    R2,
    Wasabi,
    MinIO,
    Custom,
}

/// 서비스 프리셋 (SPEC 4.1). FFI의 PierPreset에 대응한다.
///
/// `needs_endpoint`/`needs_account_id`는 연결 설정 창이 어떤 입력을
/// 보여줄지 결정하는 힌트이고, `chunk_size_mb`/`concurrency`는
/// `transfer_options`의 기본값이다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    pub id: &'static str,
    pub display_name: &'static str,
    /// 이 프리셋이 기본값을 제공하는 Provider. 둘은 1:1로 대응한다.
    pub provider: Provider,
    /// true면 사용자가 엔드포인트를 직접 입력해야 한다.
    pub needs_endpoint: bool,
    /// true면 엔드포인트 생성에 계정 ID가 필요하다(R2).
    pub needs_account_id: bool,
    pub default_region: &'static str,
    /// 선택지로 보여줄 리전 목록. 비어 있으면 자유 입력이다.
    pub regions: &'static [&'static str],
    pub path_style: bool,
    /// 전송 청크 크기 기본값(MB).
    pub chunk_size_mb: u32,
    /// 동시 전송 수 기본값.
    pub concurrency: u32,
}

const AWS_REGIONS: &[&str] = &[
    "us-east-1",
    "us-east-2",
    "us-west-1",
    "us-west-2",
    "ca-central-1",
    "eu-west-1",
    "eu-west-2",
    "eu-west-3",
    "eu-central-1",
    "eu-north-1",
    "eu-south-1",
    "ap-northeast-1",
    "ap-northeast-2",
    "ap-northeast-3",
    "ap-southeast-1",
    "ap-southeast-2",
    "ap-south-1",
    "sa-east-1",
    "af-south-1",
    "me-south-1",
    "me-central-1",
];

const WASABI_REGIONS: &[&str] = &[
    "us-east-1",
    "us-east-2",
    "us-central-1",
    "us-west-1",
    "us-west-2",
    "ca-central-1",
    "eu-central-1",
    "eu-central-2",
    "eu-west-1",
    "eu-west-2",
    "eu-west-3",
    "eu-north-1",
    "ap-northeast-1",
    "ap-northeast-2",
    "ap-southeast-1",
    "ap-southeast-2",
];

/// 서비스별 프리셋. 배열 순서가 UI에 보이는 순서다.
const PRESETS: &[Preset] = &[
    Preset {
        id: "aws",
        display_name: "Amazon S3",
        provider: Provider::Aws,
        needs_endpoint: false,
        needs_account_id: false,
        default_region: "us-east-1",
        regions: AWS_REGIONS,
        path_style: false,
        chunk_size_mb: 8,
        concurrency: 8,
    },
    Preset {
        id: "r2",
        display_name: "Cloudflare R2",
        provider: Provider::R2,
        needs_endpoint: false,
        needs_account_id: true,
        default_region: "auto",
        regions: &["auto"],
        // R2는 가상 호스트 방식을 지원하고 path-style은 지원하지 않는다.
        path_style: false,
        chunk_size_mb: 8,
        concurrency: 8,
    },
    Preset {
        id: "wasabi",
        display_name: "Wasabi",
        provider: Provider::Wasabi,
        needs_endpoint: false,
        needs_account_id: false,
        default_region: "us-east-1",
        regions: WASABI_REGIONS,
        path_style: false,
        chunk_size_mb: 8,
        concurrency: 8,
    },
    Preset {
        id: "minio",
        display_name: "MinIO",
        provider: Provider::MinIO,
        needs_endpoint: true,
        needs_account_id: false,
        default_region: "us-east-1",
        regions: &["us-east-1"],
        path_style: true,
        chunk_size_mb: 8,
        concurrency: 8,
    },
    Preset {
        id: "custom",
        display_name: "S3 호환 (커스텀)",
        provider: Provider::Custom,
        needs_endpoint: true,
        needs_account_id: false,
        default_region: "us-east-1",
        regions: &[],
        // 자체 호스팅을 포함한 임의 엔드포인트에는 path-style이
        // 안전한 기본값이다. 필요하면 연결 설정에서 바꿀 수 있다.
        path_style: true,
        chunk_size_mb: 8,
        concurrency: 8,
    },
];

/// 모든 프리셋을 표시 순서대로 반환한다.
pub fn presets() -> &'static [Preset] {
    PRESETS
}

/// Provider에 해당하는 프리셋을 반환한다. 둘은 1:1로 대응한다.
pub fn preset_for(provider: Provider) -> &'static Preset {
    PRESETS
        .iter()
        .find(|p| p.provider == provider)
        .expect("모든 Provider에는 프리셋이 있다")
}

/// 프리셋과 입력으로 엔드포인트를 만든다.
///
/// 엔드포인트가 없거나(AWS) 사용자가 직접 입력하는 경우(MinIO·커스텀)에는
/// None을 반환한다. R2는 계정 ID, Wasabi는 리전이 필요하며 입력 형식이
/// 맞지 않아도 None이다.
pub fn preset_endpoint(preset_id: &str, account_id: Option<&str>, region: &str) -> Option<String> {
    let preset = PRESETS.iter().find(|p| p.id == preset_id)?;
    match preset.provider {
        Provider::R2 => {
            let account_id = account_id?;
            valid_r2_account_id(account_id)
                .then(|| format!("https://{account_id}.r2.cloudflarestorage.com"))
        }
        Provider::Wasabi => {
            let valid = region
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            (!region.is_empty() && valid).then(|| format!("https://s3.{region}.wasabisys.com"))
        }
        _ => None,
    }
}

/// R2 계정 ID는 32자 영숫자다.
fn valid_r2_account_id(account_id: &str) -> bool {
    account_id.len() == 32 && account_id.bytes().all(|b| b.is_ascii_alphanumeric())
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
    /// 전송 청크 크기(MB). None이면 프리셋 기본값을 쓴다.
    pub chunk_size_mb: Option<u32>,
    /// 동시 전송 수. None이면 프리셋 기본값을 쓴다.
    pub concurrency: Option<u32>,
}

impl ConnectionConfig {
    /// 연결을 열기 전에 설정 값을 검사한다.
    ///
    /// 네트워크 없이 즉시 실패시켜 '/' 같은 잘못된 버킷 이름이
    /// 알아보기 힘든 전송 오류로 나타나는 것을 막는다. 에러 메시지에는
    /// 버킷·엔드포인트·리전처럼 비밀이 아닌 값만 넣고
    /// access key·secret은 절대 넣지 않는다.
    pub fn validate(&self) -> Result<(), Error> {
        validate_bucket(&self.bucket)?;
        if self.region.trim().is_empty() {
            return Err(Error::Other("리전이 비어 있습니다".to_string()));
        }
        if let Some(endpoint) = self.endpoint.as_deref() {
            validate_endpoint(endpoint)?;
        }
        if self.access_key_id.trim().is_empty() {
            return Err(Error::Other("Access Key ID가 비어 있습니다".to_string()));
        }
        Ok(())
    }

    /// 이 연결의 전송 옵션 (SPEC 4.4).
    ///
    /// 설정이 없으면 프리셋 기본값을 쓰고, 청크 5~512MB·동시성 1~32
    /// 범위로 클램프한다.
    pub fn transfer_options(&self) -> TransferOptions {
        let preset = preset_for(self.provider);
        let chunk_mb = self
            .chunk_size_mb
            .unwrap_or(preset.chunk_size_mb)
            .clamp(5, 512);
        let concurrency = self.concurrency.unwrap_or(preset.concurrency).clamp(1, 32);
        TransferOptions {
            chunk_size: u64::from(chunk_mb) * 1024 * 1024,
            concurrency: concurrency as usize,
        }
    }
}

/// S3 버킷 이름 규칙을 검사한다.
/// 3~63자, 소문자·숫자·'.'·'-'만, 첫/끝은 영숫자,
/// '..' 금지, IPv4 주소 형태 금지.
fn validate_bucket(bucket: &str) -> Result<(), Error> {
    let invalid = |reason: &str| {
        Error::Other(format!(
            "버킷 이름이 올바르지 않습니다: '{bucket}' — {reason}"
        ))
    };
    if !bucket
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
    {
        return Err(invalid("소문자·숫자·'.'·'-'만 쓸 수 있습니다"));
    }
    if !(3..=63).contains(&bucket.chars().count()) {
        return Err(invalid("3~63자여야 합니다"));
    }
    // 길이를 통과했으므로 첫/끝 문자가 항상 존재한다.
    let first = bucket.chars().next().unwrap_or_default();
    let last = bucket.chars().next_back().unwrap_or_default();
    if !first.is_ascii_alphanumeric() || !last.is_ascii_alphanumeric() {
        return Err(invalid(
            "첫 글자와 마지막 글자는 영문자 또는 숫자여야 합니다",
        ));
    }
    if bucket.contains("..") {
        return Err(invalid("'.'이 연속으로 올 수 없습니다"));
    }
    if bucket.parse::<std::net::Ipv4Addr>().is_ok() {
        return Err(invalid("IP 주소 형태는 쓸 수 없습니다"));
    }
    Ok(())
}

/// endpoint가 있으면 http:// 또는 https://로 시작하고 호스트가 있어야 한다.
fn validate_endpoint(endpoint: &str) -> Result<(), Error> {
    let rest = endpoint
        .strip_prefix("https://")
        .or_else(|| endpoint.strip_prefix("http://"))
        .ok_or_else(|| {
            Error::Other(format!(
                "엔드포인트가 올바르지 않습니다: '{endpoint}' — http:// 또는 https://로 시작해야 합니다"
            ))
        })?;
    // scheme 뒤 authority(호스트[:포트]) 부분이 비어 있으면 호스트가 없는 것이다.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() {
        return Err(Error::Other(format!(
            "엔드포인트에 호스트가 없습니다: '{endpoint}'"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 모든 필드가 올바른 기본 설정. 각 테스트에서 한 필드만 바꿔 검사한다.
    fn valid_config() -> ConnectionConfig {
        ConnectionConfig {
            id: "test".to_string(),
            display_name: "테스트".to_string(),
            provider: Provider::Aws,
            endpoint: None,
            region: "ap-northeast-2".to_string(),
            bucket: "my-bucket".to_string(),
            prefix: None,
            path_style: false,
            access_key_id: "AKIAEXAMPLE".to_string(),
            chunk_size_mb: None,
            concurrency: None,
        }
    }

    #[test]
    fn validate_accepts_valid_config() {
        assert!(valid_config().validate().is_ok());
        // 커스텀 엔드포인트(MinIO 등)도 허용한다.
        let mut config = valid_config();
        config.provider = Provider::MinIO;
        config.endpoint = Some("http://127.0.0.1:9000".to_string());
        config.path_style = true;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_bucket_length() {
        let cases = [
            ("ab".to_string(), false),
            ("abc".to_string(), true),
            ("a".repeat(63), true),
            ("a".repeat(64), false),
        ];
        for (bucket, ok) in cases {
            let mut config = valid_config();
            config.bucket = bucket.clone();
            assert_eq!(config.validate().is_ok(), ok, "bucket={bucket:?}");
        }
    }

    #[test]
    fn validate_bucket_rejects_invalid_names() {
        // 대문자, '/', 앞뒤 '-', '..', IPv4 형태, 빈 문자열.
        for bucket in ["MyBucket", "/", "-abc", "abc-", "a..b", "192.168.0.1", ""] {
            let mut config = valid_config();
            config.bucket = bucket.to_string();
            let err = config.validate().expect_err("거부돼야 한다");
            assert!(matches!(err, Error::Other(_)), "{err:?}");
            assert!(err.to_string().contains(bucket), "{err}");
        }
    }

    #[test]
    fn validate_rejects_empty_region() {
        let mut config = valid_config();
        config.region = String::new();
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_rejects_bad_endpoint() {
        // http/https가 아닌 scheme, scheme만 있고 호스트가 없는 경우.
        for endpoint in ["ftp://x", "http://"] {
            let mut config = valid_config();
            config.endpoint = Some(endpoint.to_string());
            assert!(config.validate().is_err(), "endpoint={endpoint:?}");
        }
        let mut config = valid_config();
        config.endpoint = Some("https://s3.example.com".to_string());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_rejects_empty_access_key_id() {
        let mut config = valid_config();
        config.access_key_id = String::new();
        assert!(config.validate().is_err());
    }

    #[test]
    fn presets_cover_every_provider() {
        assert_eq!(presets().len(), 5);
        for provider in [
            Provider::Aws,
            Provider::R2,
            Provider::Wasabi,
            Provider::MinIO,
            Provider::Custom,
        ] {
            let preset = preset_for(provider);
            assert_eq!(preset.provider, provider);
            assert!(presets().iter().any(|p| p.id == preset.id));
        }
    }

    #[test]
    fn preset_flags() {
        let get = |id: &str| presets().iter().find(|p| p.id == id).unwrap();
        assert!(!get("aws").needs_endpoint);
        assert!(!get("aws").needs_account_id);
        assert!(get("r2").needs_account_id);
        assert!(!get("r2").needs_endpoint);
        assert_eq!(get("r2").default_region, "auto");
        assert!(get("minio").needs_endpoint);
        assert!(get("minio").path_style);
        assert!(get("custom").needs_endpoint);
    }

    #[test]
    fn preset_endpoint_cases() {
        // R2: 유효한 32자 영숫자 계정 ID로 만든다.
        let account = "a1".repeat(16);
        assert_eq!(
            preset_endpoint("r2", Some(&account), "auto").as_deref(),
            Some(format!("https://{account}.r2.cloudflarestorage.com").as_str())
        );
        // 계정 ID가 없거나 형식이 틀리면 None.
        assert!(preset_endpoint("r2", None, "auto").is_none());
        assert!(preset_endpoint("r2", Some("short"), "auto").is_none());
        assert!(preset_endpoint("r2", Some(&"!".repeat(32)), "auto").is_none());
        // Wasabi: 리전에서 생성한다.
        assert_eq!(
            preset_endpoint("wasabi", None, "eu-west-1").as_deref(),
            Some("https://s3.eu-west-1.wasabisys.com")
        );
        assert!(preset_endpoint("wasabi", None, "").is_none());
        // AWS·MinIO·커스텀은 자동 생성 엔드포인트가 없다.
        assert!(preset_endpoint("aws", None, "us-east-1").is_none());
        assert!(preset_endpoint("minio", None, "us-east-1").is_none());
        assert!(preset_endpoint("없는id", None, "x").is_none());
    }

    #[test]
    fn transfer_options_defaults_and_clamps() {
        let mut config = valid_config();
        // 설정이 없으면 AWS 프리셋 기본값(8MB 청크, 동시성 8).
        let opts = config.transfer_options();
        assert_eq!(opts.chunk_size, 8 * 1024 * 1024);
        assert_eq!(opts.concurrency, 8);

        // 범위 밖 값은 클램프된다.
        config.chunk_size_mb = Some(1);
        config.concurrency = Some(0);
        let opts = config.transfer_options();
        assert_eq!(opts.chunk_size, 5 * 1024 * 1024);
        assert_eq!(opts.concurrency, 1);

        config.chunk_size_mb = Some(9999);
        config.concurrency = Some(99);
        let opts = config.transfer_options();
        assert_eq!(opts.chunk_size, 512 * 1024 * 1024);
        assert_eq!(opts.concurrency, 32);
    }
}
