import OSLog

/// pier 전체 공용 로거. subsystem은 `com.yourname.pier`.
enum Log {
    static let app = Logger(subsystem: "com.yourname.pier", category: "app")
    static let core = Logger(subsystem: "com.yourname.pier", category: "core")
}
