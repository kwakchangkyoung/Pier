import Foundation

/// 연결 목록 저장소. `connections.json`을 읽고 쓸 때마다 디스크에 반영한다.
/// 시크릿은 절대 저장하지 않는다(키체인 담당).
@Observable
final class ConnectionStore {
    private(set) var connections: [SavedConnection] = []

    init() {
        load()
    }

    func connection(withId id: UUID?) -> SavedConnection? {
        guard let id else { return nil }
        return connections.first { $0.id == id }
    }

    /// 새 연결을 추가한다. 키체인 저장이 먼저이고, JSON 저장 실패 시 키체인을 되돌린다.
    func add(_ record: SavedConnection, secret: String) throws {
        try Keychain.save(secret, account: record.id.uuidString)
        var list = connections
        list.append(record)
        do {
            try save(list)
            connections = list
        } catch {
            Keychain.delete(account: record.id.uuidString)
            throw error
        }
    }

    /// 기존 연결을 수정한다. secret이 nil이면 키체인의 기존 값을 유지한다.
    func update(_ record: SavedConnection, secret: String?) throws {
        guard let index = connections.firstIndex(where: { $0.id == record.id }) else {
            throw StoreError.connectionNotFound
        }
        var list = connections
        list[index] = record
        try save(list)
        connections = list
        if let secret {
            try Keychain.save(secret, account: record.id.uuidString)
        }
    }

    /// 연결을 삭제한다. JSON을 먼저 지우고, 키체인 항목과 캐시 디렉터리도 함께 지운다(SPEC 6.1).
    /// PierSessions 쪽 정리는 호출자가 `connectionDidDelete`로 처리한다.
    func delete(_ record: SavedConnection) throws {
        var list = connections
        list.removeAll { $0.id == record.id }
        try save(list)
        connections = list
        Keychain.delete(account: record.id.uuidString)
        PierPaths.removeCacheDirectory(for: record.id)
    }

    private func load() {
        guard let data = try? Data(contentsOf: PierPaths.connectionsFileURL),
            let decoded = try? JSONDecoder().decode([SavedConnection].self, from: data)
        else {
            connections = []
            return
        }
        connections = decoded
    }

    private func save(_ list: [SavedConnection]) throws {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
        let data = try encoder.encode(list)
        let url = PierPaths.connectionsFileURL
        try FileManager.default.createDirectory(
            at: url.deletingLastPathComponent(),
            withIntermediateDirectories: true
        )
        try data.write(to: url, options: .atomic)
    }
}

/// ConnectionStore가 던지는 오류.
enum StoreError: Error {
    case connectionNotFound
}
