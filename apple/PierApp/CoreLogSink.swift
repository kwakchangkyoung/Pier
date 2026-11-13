import Foundation

/// 코어 `log` 크레이트 출력을 os_log로 넘기는 싱크.
/// 앱 시작 시 `setLogSink`로 한 번만 설치한다. 코어는 시크릿·서명을
/// 로그에 남기지 않는다는 계약이 있으므로 메시지를 그대로 기록한다.
final class CoreLogSink: PierLogSink, @unchecked Sendable {
    func onLog(level: PierLogLevel, target: String, message: String) {
        let text = "[\(target)] \(message)"
        switch level {
        case .error:
            Log.core.error("\(text, privacy: .public)")
        case .warn:
            Log.core.warning("\(text, privacy: .public)")
        case .info:
            Log.core.info("\(text, privacy: .public)")
        case .debug:
            Log.core.debug("\(text, privacy: .public)")
        }
    }
}
