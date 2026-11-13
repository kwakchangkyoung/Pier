import Foundation

/// 앱이 사용하는 파일시스템 위치 모음.
enum PierPaths {
    private static let appDirectoryName = "com.yourname.pier"

    /// `~/Library/Application Support/com.yourname.pier`
    static var applicationSupportDirectory: URL {
        FileManager.default
            .urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent(appDirectoryName, isDirectory: true)
    }

    /// 연결 목록 JSON.
    static var connectionsFileURL: URL {
        applicationSupportDirectory.appendingPathComponent("connections.json")
    }

    /// `~/Library/Caches/com.yourname.pier`
    private static var cachesRoot: URL {
        FileManager.default
            .urls(for: .cachesDirectory, in: .userDomainMask)[0]
            .appendingPathComponent(appDirectoryName, isDirectory: true)
    }

    /// 연결별 캐시 디렉터리. `~/Library/Caches/com.yourname.pier/connections/<id>/`
    static func cacheDirectory(for connectionId: UUID) -> URL {
        cachesRoot
            .appendingPathComponent("connections", isDirectory: true)
            .appendingPathComponent(connectionId.uuidString, isDirectory: true)
    }

    /// 캐시 디렉터리를 만들고 경로를 반환한다.
    static func ensureCacheDirectory(for connectionId: UUID) throws -> URL {
        let url = cacheDirectory(for: connectionId)
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url
    }

    /// 연결 삭제 시 캐시 디렉터리를 통째로 지운다. 없으면 무시한다.
    static func removeCacheDirectory(for connectionId: UUID) {
        try? FileManager.default.removeItem(at: cacheDirectory(for: connectionId))
    }
}
