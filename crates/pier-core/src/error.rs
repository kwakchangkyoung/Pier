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
    #[error("{0}")]
    Other(String),
}

impl From<opendal::Error> for Error {
    fn from(err: opendal::Error) -> Self {
        let kind = err.kind();
        let message = err.to_string();
        match kind {
            opendal::ErrorKind::PermissionDenied => Error::Authentication(message),
            opendal::ErrorKind::NotFound => Error::NotFound(message),
            _ if is_network_error(&err) => Error::Network(message),
            _ => Error::Other(message),
        }
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
