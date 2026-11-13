//! `log` 파사드의 로그를 앱(Swift의 os_log)으로 전달하는 브리지 (SPEC 4.6).
//!
//! `install`로 싱크를 한 번만 설치하고, Info 이상만 전달한다.
//! 키·시크릿·presigned URL 전체 값은 로그에 남기지 않는다.

use log::{Level, LevelFilter, Log, Metadata, Record};

use crate::error::Error;

/// 코어 로그를 받는 싱크. FFI의 PierLogSink에 대응한다.
/// `log` 매크로를 부른 스레드에서 동기로 호출되므로 구현은 빠르게 반환하고
/// UI 갱신은 싱크 안에서 비동기로 넘겨야 한다.
pub trait LogSink: Send + Sync {
    fn log(&self, level: Level, target: &str, message: &str);
}

/// `log` 전역 로거로 싱크를 설치한다. 프로세스당 한 번만 성공한다.
/// Info 이상(Debug·Trace 제외)만 전달한다.
pub fn install(sink: Box<dyn LogSink>) -> Result<(), Error> {
    log::set_boxed_logger(Box::new(SinkLogger { sink }))
        .map_err(|_| Error::Other("로그 싱크가 이미 설치됐다".to_string()))?;
    log::set_max_level(LevelFilter::Info);
    // 설치 직후 한 줄 — 앱 시작 시 싱크 경로가 살아 있는지
    // os_log에서 확인할 수 있다.
    log::info!("코어 로그 싱크를 설치했다");
    Ok(())
}

/// `LogSink`를 `log::Log`로 감싸는 어댑터.
struct SinkLogger {
    sink: Box<dyn LogSink>,
}

impl Log for SinkLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= Level::Info
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            self.sink
                .log(record.level(), record.target(), &record.args().to_string());
        }
    }

    fn flush(&self) {}
}
