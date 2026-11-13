import Foundation

/// 다운로드 항목 하나의 상태.
struct DownloadItem: Identifiable, Equatable {
    enum State: Equatable {
        case downloading
        case completed
        case failed(String)
    }

    let id = UUID()
    let name: String
    /// S3 객체 키. 파일은 객체 키, 폴더는 '/'로 끝나는 접두사.
    let key: String
    /// 폴더 다운로드 작업인지 여부.
    let isFolder: Bool
    var state: State = .downloading
    var transferred: UInt64 = 0
    var total: UInt64 = 0
    /// 초당 바이트. progress 콜백이 올 때마다 갱신한다.
    var speed: Double = 0
    /// 완료 시 Finder에서 보기용 로컬 경로.
    var destinationPath: String?
    /// 폴더 다운로드 완료 시 받은 파일 수.
    var fileCount: UInt64 = 0
}

/// 진행 중인 다운로드를 관리한다. 모든 상태 변경은 메인 액터에서 일어난다.
@MainActor @Observable
final class DownloadManager {
    private(set) var items: [DownloadItem] = []
    private var cancelTokens: [UUID: PierCancelToken] = [:]
    /// 속도 계산용 마지막 (전송량, 시각).
    private var lastProgress: [UUID: (bytes: UInt64, at: Date)] = [:]

    /// 선택한 항목들을 directory 아래로 다운로드한다. 파일과 폴더를 섞어 시작할 수 있고
    /// 여러 작업을 동시에 실행할 수 있다.
    func start(connection: PierConnection, entries: [PierEntry], into directory: URL) {
        for entry in entries {
            if entry.isDir {
                startFolder(connection: connection, entry: entry, into: directory)
            } else {
                startFile(connection: connection, entry: entry, into: directory)
            }
        }
    }

    /// 단일 객체 다운로드.
    private func startFile(connection: PierConnection, entry: PierEntry, into directory: URL) {
        let destination = Self.uniqueDestination(in: directory, name: entry.name)
        let item = DownloadItem(
            name: entry.name,
            key: entry.identifier,
            isFolder: false,
            total: entry.size,
            destinationPath: destination.path
        )
        let token = begin(item: item)
        let id = item.id
        let progress = DownloadProgressBridge { [weak self] transferred, total in
            Task { @MainActor in
                self?.recordProgress(id: id, transferred: transferred, total: total)
            }
        }
        Task {
            do {
                _ = try await connection.download(
                    key: entry.identifier,
                    destPath: destination.path,
                    progress: progress,
                    cancel: token
                )
                finish(id: id, state: .completed)
            } catch {
                fail(id: id, error: error)
            }
        }
    }

    /// 폴더 재귀 다운로드. 코어가 destDir 안에 '<폴더 이름>'을 만들고
    /// 하위 객체를 상대 경로 구조 그대로 저장한다.
    private func startFolder(connection: PierConnection, entry: PierEntry, into directory: URL) {
        // prefix는 '/'로 끝나는 폴더 키여야 한다(빈 문자열=루트는 코어가 거부).
        let prefix = entry.identifier.hasSuffix("/") ? entry.identifier : entry.identifier + "/"
        let item = DownloadItem(name: entry.name, key: prefix, isFolder: true)
        let token = begin(item: item)
        let id = item.id
        let progress = DownloadProgressBridge { [weak self] transferred, total in
            Task { @MainActor in
                self?.recordProgress(id: id, transferred: transferred, total: total)
            }
        }
        Task {
            do {
                let result = try await connection.downloadFolder(
                    prefix: prefix,
                    destDir: directory.path,
                    progress: progress,
                    cancel: token
                )
                completeFolder(id: id, result: result)
            } catch {
                fail(id: id, error: error)
            }
        }
    }

    /// 항목을 목록에 넣고 취소 토큰을 만들어 돌려준다.
    private func begin(item: DownloadItem) -> PierCancelToken {
        items.append(item)
        let token = PierCancelToken()
        cancelTokens[item.id] = token
        return token
    }

    /// 폴더 다운로드 완료. 실제 생성 경로·파일 수·전체 바이트를 기록한다.
    private func completeFolder(id: UUID, result: PierFolderDownload) {
        finish(id: id, state: .completed)
        guard let index = items.firstIndex(where: { $0.id == id }) else { return }
        items[index].total = result.totalBytes
        items[index].transferred = result.totalBytes
        items[index].fileCount = result.fileCount
        items[index].destinationPath = result.localPath
    }

    /// 실패 처리. 취소는 오류로 표시하지 않고 목록에서 지운다.
    private func fail(id: UUID, error: Error) {
        guard let message = PierErrorPresenter.message(for: error) else {
            remove(id)
            return
        }
        finish(id: id, state: .failed(message))
    }

    /// 진행 중인 다운로드를 취소한다. 항목은 즉시 목록에서 사라진다.
    /// 코어는 임시 파일에 쓰므로 취소해도 부분 파일이 남지 않는다.
    func cancel(_ id: UUID) {
        cancelTokens[id]?.cancel()
        remove(id)
    }

    /// 완료/실패 항목을 목록에서 지운다.
    func remove(_ id: UUID) {
        items.removeAll { $0.id == id }
        cancelTokens.removeValue(forKey: id)
        lastProgress.removeValue(forKey: id)
    }

    // MARK: - 내부

    private func finish(id: UUID, state: DownloadItem.State) {
        guard let index = items.firstIndex(where: { $0.id == id }) else {
            cancelTokens.removeValue(forKey: id)
            lastProgress.removeValue(forKey: id)
            return
        }
        if case .completed = state {
            items[index].transferred = items[index].total
        }
        items[index].state = state
        cancelTokens.removeValue(forKey: id)
        lastProgress.removeValue(forKey: id)
    }

    private func recordProgress(id: UUID, transferred: UInt64, total: UInt64) {
        guard let index = items.firstIndex(where: { $0.id == id }),
            case .downloading = items[index].state
        else { return }
        let now = Date()
        if let last = lastProgress[id] {
            let elapsed = now.timeIntervalSince(last.at)
            if elapsed > 0 {
                items[index].speed = Double(transferred &- last.bytes) / elapsed
            }
        }
        lastProgress[id] = (transferred, now)
        items[index].transferred = transferred
        if total > 0 {
            items[index].total = total
        }
    }

    /// `이름 (2).확장자` 형태로 대상 폴더에서 비어 있는 파일 이름을 고른다.
    static func uniqueDestination(in directory: URL, name: String) -> URL {
        let first = directory.appendingPathComponent(name)
        guard FileManager.default.fileExists(atPath: first.path) else {
            return first
        }
        let nsName = name as NSString
        let base = nsName.deletingPathExtension
        let ext = nsName.pathExtension
        var index = 2
        while true {
            let newName = ext.isEmpty ? "\(base) (\(index))" : "\(base) (\(index)).\(ext)"
            let url = directory.appendingPathComponent(newName)
            if !FileManager.default.fileExists(atPath: url.path) {
                return url
            }
            index += 1
        }
    }
}

/// FFI `PierProgress` 구현. Rust가 백그라운드 스레드에서 호출하므로
/// 0.1초로 스로틀한 뒤 메인 액터로 넘긴다.
final class DownloadProgressBridge: PierProgress, @unchecked Sendable {
    private let lock = NSLock()
    private var lastFire = Date.distantPast
    private let handler: @Sendable (UInt64, UInt64) -> Void

    init(handler: @escaping @Sendable (UInt64, UInt64) -> Void) {
        self.handler = handler
    }

    func onProgress(transferred: UInt64, total: UInt64) {
        let now = Date()
        // 마지막 업데이트는 스로틀과 무관하게 보낸다.
        let isFinal = total > 0 && transferred >= total
        lock.lock()
        let shouldFire = now.timeIntervalSince(lastFire) >= 0.1 || isFinal
        if shouldFire {
            lastFire = now
        }
        lock.unlock()
        if shouldFire {
            handler(transferred, total)
        }
    }
}
