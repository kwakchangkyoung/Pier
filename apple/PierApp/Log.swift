import OSLog

/// pier 전체 공용 로거. subsystem은 `com.gajago.pier`.
enum Log {
    static let app = Logger(subsystem: "com.gajago.pier", category: "app")
    static let core = Logger(subsystem: "com.gajago.pier", category: "core")
}
