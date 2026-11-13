use thiserror::Error;

/// 코어 에러. SPEC 4.5의 범주와 1:1로 대응한다.
/// 메시지에는 시크릿을 포함하지 않는다.
#[derive(Debug, Error)]
pub enum Error {
    #[error("authentication failed: {0}")]
    Authentication(String),
    #[error("network error: {0}")]
    Network(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("cancelled: {0}")]
    Cancelled(String),
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// opendal 에러를 분류하고 사용자·로그용 짧은 메시지를 만든다.
    ///
    /// `op`은 "stat", "list" 같은 작업 이름, `target`은 대상 키·접두사다.
    /// 결과는 `"NotFound: stat 없는키 (NoSuchKey)"` 수준으로, HTTP 응답
    /// 헤더·URI·서명·자격증명 관련 값은 절대 포함하지 않는다.
    pub fn from_opendal(err: opendal::Error, op: &str, target: &str) -> Self {
        let kind = err.kind();
        let detail = opendal_detail(&err);
        let place = match (op.is_empty(), target.is_empty()) {
            (false, false) => format!("{op} {target}"),
            (false, true) => op.to_string(),
            (true, false) => target.to_string(),
            (true, true) => String::new(),
        };
        // HEAD 요청 등 본문이 없는 오류는 detail이 kind와 같아지므로
        // 중복된 괄호를 붙이지 않는다.
        let suffix = if detail == kind.to_string() {
            String::new()
        } else {
            format!(" ({detail})")
        };
        let message = if place.is_empty() {
            format!("{kind}{suffix}")
        } else {
            format!("{kind}: {place}{suffix}")
        };
        match kind {
            opendal::ErrorKind::PermissionDenied => Error::Authentication(message),
            opendal::ErrorKind::NotFound => Error::NotFound(message),
            _ if is_network_error(&err) => Error::Network(message),
            _ => Error::Other(message),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        if is_network_io_kind(err.kind()) {
            Error::Network(err.to_string())
        } else if err.kind() == std::io::ErrorKind::NotFound {
            Error::NotFound(err.to_string())
        } else {
            Error::Other(err.to_string())
        }
    }
}

impl From<rusqlite::Error> for Error {
    fn from(err: rusqlite::Error) -> Self {
        Error::Other(format!("cache: {err}"))
    }
}

impl From<opendal::Error> for Error {
    fn from(err: opendal::Error) -> Self {
        Error::from_opendal(err, "", "")
    }
}

/// opendal 에러에서 사용자에게 보여줄 짧은 설명을 뽑는다.
///
/// S3 오류 응답은 message가 `S3Error { code, message, resource, request_id }`
/// 디버그 형태라 request_id(헤더 유래 값)가 섞여 있다. 코드만 추린다.
/// 그 외 메시지가 XML/HTML 응답 본문이면 헤더 값이 섞일 수 있어 버리고,
/// 소스 체인에서 첫 설명을 찾는다.
fn opendal_detail(err: &opendal::Error) -> String {
    let msg = err.message();
    if let Some(rest) = msg.strip_prefix("S3Error {") {
        return extract_debug_field(rest, "code")
            .filter(|code| !code.is_empty())
            .unwrap_or_else(|| "s3 error".to_string());
    }
    if !msg.is_empty() && !msg.contains('<') {
        return short_text(msg, 160);
    }
    let mut source = std::error::Error::source(err);
    while let Some(e) = source {
        let text = e.to_string();
        if !text.is_empty() {
            return short_text(&text, 160);
        }
        source = e.source();
    }
    err.kind().to_string()
}

/// `S3Error { code: "NoSuchKey", ... }` 같은 디버그 문자열에서
/// `field: "..."` 값을 꺼낸다. 값에 이스케이프가 있으면 잘린다(코드 필드는
/// 단순 식별자라 문제없다).
fn extract_debug_field(s: &str, field: &str) -> Option<String> {
    let pat = format!("{field}: \"");
    let start = s.find(&pat)? + pat.len();
    let end = s[start..].find('"')? + start;
    Some(s[start..end].to_string())
}

/// 공백을 정리하고 max자를 넘으면 자른다. URL 등은 포함될 수 있으나
/// 길이를 제한해 로그·UI를 오염시키지 않는다.
fn short_text(s: &str, max: usize) -> String {
    let one_line: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        format!("{}…", one_line.chars().take(max).collect::<String>())
    }
}

/// OpenDAL 에러의 소스 체인에서 네트워크 단절·타임아웃 계열인지 판별한다.
fn is_network_error(err: &opendal::Error) -> bool {
    let mut source = std::error::Error::source(err);
    while let Some(e) = source {
        if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
            if is_network_io_kind(io_err.kind()) {
                return true;
            }
        }
        let text = e.to_string().to_lowercase();
        if NETWORK_HINTS.iter().any(|hint| text.contains(hint)) {
            return true;
        }
        source = e.source();
    }
    false
}

fn is_network_io_kind(kind: std::io::ErrorKind) -> bool {
    use std::io::ErrorKind::*;
    matches!(
        kind,
        ConnectionRefused
            | ConnectionReset
            | ConnectionAborted
            | NotConnected
            | TimedOut
            | BrokenPipe
            | UnexpectedEof
            | HostUnreachable
            | NetworkUnreachable
            | NetworkDown
    )
}

const NETWORK_HINTS: &[&str] = &[
    "timed out",
    "timeout",
    "connection refused",
    "connection reset",
    "connection aborted",
    "broken pipe",
    "network is unreachable",
    "no route to host",
    "dns error",
    "failed to lookup",
];

#[cfg(test)]
mod tests {
    use super::*;

    /// S3 HTTP 오류가 헤더·request_id·URI 서명 값을 메시지에 남기지 않는지 확인한다.
    #[test]
    fn opendal_error_strips_sensitive_context() {
        let err = opendal::Error::new(
            opendal::ErrorKind::PermissionDenied,
            "S3Error { code: \"SignatureDoesNotMatch\", message: \"signature mismatch\", resource: \"/pier-test/k\", request_id: \"req-secret-123\" }",
        )
        .with_context(
            "response",
            "HTTP/1.1 403 Forbidden\r\nx-amz-request-id: req-secret-123\r\nx-amz-id-2: host-1\r\n",
        )
        .with_context(
            "uri",
            "http://127.0.0.1:9000/pier-test/k?X-Amz-Signature=deadbeef",
        );
        let e = Error::from_opendal(err, "list", "docs/");
        assert!(matches!(e, Error::Authentication(_)));
        let s = e.to_string();
        assert!(s.contains("PermissionDenied"), "{s}");
        assert!(s.contains("SignatureDoesNotMatch"), "{s}");
        assert!(s.contains("list docs/"), "{s}");
        assert!(!s.contains("x-amz-request-id"), "{s}");
        assert!(!s.contains("req-secret-123"), "{s}");
        assert!(!s.contains("X-Amz-Signature"), "{s}");
        assert!(!s.contains("deadbeef"), "{s}");
    }

    #[test]
    fn opendal_not_found_includes_op_and_target() {
        let err = opendal::Error::new(
            opendal::ErrorKind::NotFound,
            "S3Error { code: \"NoSuchKey\", message: \"missing\", resource: \"/b/k\", request_id: \"r1\" }",
        );
        let e = Error::from_opendal(err, "stat", "없는키");
        assert!(matches!(e, Error::NotFound(_)));
        let s = e.to_string();
        assert!(s.contains("NotFound"), "{s}");
        assert!(s.contains("stat 없는키"), "{s}");
        assert!(s.contains("NoSuchKey"), "{s}");
        assert!(!s.contains("r1"), "{s}");
    }

    #[test]
    fn opendal_xml_body_is_not_leaked() {
        // S3Error로 파싱되지 않은 XML 본문은 request id 등이 섞일 수 있어
        // 메시지에 넣지 않는다.
        let err = opendal::Error::new(
            opendal::ErrorKind::Unexpected,
            "<?xml version=\"1.0\"?><Error><Code>InternalError</Code><RequestId>rid-9</RequestId></Error>",
        );
        let e = Error::from_opendal(err, "read", "a.bin");
        let s = e.to_string();
        assert!(!s.contains("rid-9"), "{s}");
        assert!(!s.contains("RequestId"), "{s}");
    }

    #[test]
    fn cancelled_error_display() {
        let e = Error::Cancelled("다운로드가 취소됐다".to_string());
        assert!(e.to_string().contains("cancelled"));
    }
}
