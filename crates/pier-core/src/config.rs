use crate::error::Error;

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
}
