import Foundation

/// PierSessions/BrowserModel이 던지는 앱 레벨 오류.
enum PierSessionError: Error {
    /// 세션 관리자가 이미 해제된 경우. 앱 수명 동안은 사실상 발생하지 않는다.
    case unavailable
    /// 저장된 연결 설정이 FFI `PierConfig`로 변환되지 않는 경우.
    case invalidConfiguration
    /// 폴더/항목 이름이 비어 있거나 '/'를 포함하는 경우.
    case invalidName
}

/// `PierConnection` 인스턴스와 연결별 `BrowserModel`을 관리한다.
/// 연결 객체는 connection id별로 재사용하고, 설정 변경·삭제 시 폐기한다.
@MainActor @Observable
final class PierSessions {
    private var browsers: [UUID: BrowserModel] = [:]
    private var connections: [UUID: PierConnection] = [:]
    /// PierConnection을 열 때 사용한 SavedConnection. 설정이 바뀌면 다시 연다.
    private var openedRecords: [UUID: SavedConnection] = [:]

    /// 사이드바/메뉴에서 선택된 연결 id.
    var selectedId: UUID?

    /// 대기열 집계자. 앱 시작 시 한 번 연결하면 이후 열리는 연결에 리스너를 단다.
    var transfers: TransferManager?

    /// 현재 선택된 연결의 브라우저. 없으면 nil.
    var activeBrowser: BrowserModel? {
        guard let selectedId else { return nil }
        return browsers[selectedId]
    }

    /// 사이드바 선택이 바뀔 때 호출한다. record가 nil이면 선택 해제.
    func activate(_ record: SavedConnection?) {
        guard let record else {
            selectedId = nil
            return
        }
        selectedId = record.id
        if browsers[record.id] == nil {
            browsers[record.id] = makeBrowser(record)
        }
        browsers[record.id]?.activate()
    }

    /// 연결 설정이 저장됐을 때 호출한다. 열려 있던 연결·브라우저를 폐기하고,
    /// 현재 선택 중이면 새 설정으로 다시 연다.
    func connectionDidChange(_ record: SavedConnection) {
        connections.removeValue(forKey: record.id)
        openedRecords.removeValue(forKey: record.id)
        browsers.removeValue(forKey: record.id)
        transfers?.unregister(connectionId: record.id)
        if selectedId == record.id {
            browsers[record.id] = makeBrowser(record)
            browsers[record.id]?.activate()
        }
    }

    /// 연결이 삭제됐을 때 호출한다. 관련 상태를 모두 버린다.
    func connectionDidDelete(_ id: UUID) {
        browsers.removeValue(forKey: id)
        connections.removeValue(forKey: id)
        openedRecords.removeValue(forKey: id)
        transfers?.unregister(connectionId: id)
        if selectedId == id {
            selectedId = nil
        }
    }

    private func makeBrowser(_ record: SavedConnection) -> BrowserModel {
        // canWrite가 nil(이전 버전 연결)이면 쓰기 허용으로 본다.
        BrowserModel(
            connectionId: record.id,
            title: record.displayName,
            canWrite: record.canWrite != false
        ) { [weak self] in
            guard let self else { throw PierSessionError.unavailable }
            return try self.pierConnection(for: record)
        }
    }

    /// connection id에 대응하는 PierConnection을 반환한다. 캐시가 있고
    /// 설정이 그대로면 재사용하고, 아니면 키체인 시크릿으로 새로 연다.
    private func pierConnection(for record: SavedConnection) throws -> PierConnection {
        if let cached = connections[record.id], openedRecords[record.id] == record {
            return cached
        }
        guard let config = record.pierConfig else {
            throw PierSessionError.invalidConfiguration
        }
        guard let secret = try Keychain.load(account: record.id.uuidString) else {
            throw Keychain.KeychainError.itemNotFound
        }
        let cacheDir = try PierPaths.ensureCacheDirectory(for: record.id)
        let connection = try PierConnection.open(
            config: config,
            secretAccessKey: secret,
            cacheDir: cacheDir.path
        )
        connections[record.id] = connection
        openedRecords[record.id] = record
        transfers?.register(connection: connection, connectionId: record.id)
        return connection
    }
}
