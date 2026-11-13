import Foundation

/// 업로드 Conflict 오류가 났을 때 Swift UI에 재질문하기 위한 컨텍스트.
struct TransferConflict: Identifiable {
    let id = UUID()
    let name: String
    let isDir: Bool
    /// 업로드 대상 부모 폴더 키(표시용).
    let parent: String
}

/// 재질문에 대한 사용자 선택.
enum TransferConflictChoice {
    /// 대치. 파일은 최신 ETag를 다시 읽어 조건부 쓰기로 재시도한다.
    case replace
    /// 둘 다 유지. 코어가 비어 있는 이름을 골라 업로드한다.
    case keepBoth
    /// 건너뛰기. 이 항목의 전송을 중단한다.
    case skip
}

/// 전송 도중 Conflict가 나면 호출되는 재질문 클로저. nil을 반환하면 건너뛴다.
typealias UploadConflictHandler = @MainActor (TransferConflict) async -> TransferConflictChoice?

/// 작업별 후속 동작 묶음. 작업이 종료 상태에 들어갈 때 한 번만 소비된다.
struct JobContext {
    /// 완료 시 결과 반영 콜백(업로드 항목 갱신 등). 실패·취소에는 호출하지 않는다.
    var onFinished: (@MainActor (PierEntry?) -> Void)?
    /// 업로드 재시도 정보. Conflict 재질문에 쓴다.
    var upload: UploadRequest?
    /// Finder 프로미스 다운로드 정보.
    var promise: PromiseRequest?
}

/// 업로드 작업의 재시도에 필요한 원래 요청.
struct UploadRequest {
    let connection: PierConnection
    let connectionId: UUID
    let source: URL
    let parent: String
    let name: String
    let isDir: Bool
    let onFinished: (@MainActor (PierEntry?) -> Void)?
    let conflictHandler: UploadConflictHandler?
}

/// Finder 파일 프로미스 드롭의 완료 정보.
struct PromiseRequest {
    let destination: URL
    let isDir: Bool
    /// 폴더 다운로드가 '<폴더 이름>'을 만든 임시 디렉터리. 파일 프로미스는 nil.
    let tempDir: URL?
    let completion: @MainActor @Sendable (Error?) -> Void
}

/// TransferManager의 enqueue 경로. 전송 실행은 코어 대기열이 하고
/// 여기서는 작업 등록만 담당한다. 종료 후속 동작은 TransferFinish.swift에 있다.
extension TransferManager {
    // MARK: - 다운로드

    /// 선택한 항목들을 directory 아래로 받는다. 파일과 폴더를 섞어 enqueue할 수 있다.
    func startDownloads(
        connection: PierConnection,
        entries: [PierEntry],
        into directory: URL
    ) {
        for entry in entries {
            if entry.isDir {
                // prefix는 '/'로 끝나는 폴더 키여야 한다(루트는 코어가 거부).
                let prefix =
                    entry.identifier.hasSuffix("/") ? entry.identifier : entry.identifier + "/"
                _ = connection.enqueueDownloadFolder(prefix: prefix, destDir: directory.path)
            } else {
                let destination = Self.uniqueDestination(in: directory, name: entry.name)
                _ = connection.enqueueDownload(key: entry.identifier, destPath: destination.path)
            }
        }
    }

    // MARK: - 업로드

    /// 단일 파일 업로드 enqueue. 완료 시 onFinished에 결과 PierEntry를 넘긴다.
    /// conflictHandler가 있으면 Conflict 실패 시 재질문 후 새 작업으로 enqueue한다.
    func enqueueUpload(
        connection: PierConnection,
        connectionId: UUID,
        source: URL,
        parent: String,
        name: String,
        mode: PierUploadMode,
        onFinished: (@MainActor (PierEntry?) -> Void)?,
        conflictHandler: UploadConflictHandler?
    ) {
        let jobId = connection.enqueueUpload(
            srcPath: source.path, parent: parent, name: name, mode: mode)
        contexts[JobKey(connectionId: connectionId, jobId: jobId)] = JobContext(
            onFinished: onFinished,
            upload: UploadRequest(
                connection: connection, connectionId: connectionId,
                source: source, parent: parent, name: name,
                isDir: false, onFinished: onFinished, conflictHandler: conflictHandler
            )
        )
    }

    /// 폴더 재귀 업로드 enqueue. 코어가 3.6 제외 규칙을 적용하고
    /// srcDir 이름으로 parent 아래 폴더를 만든다. mode는 하위 모든 파일에 적용된다.
    func enqueueUploadFolder(
        connection: PierConnection,
        connectionId: UUID,
        source: URL,
        parent: String,
        mode: PierUploadMode,
        onFinished: (@MainActor (PierEntry?) -> Void)?,
        conflictHandler: UploadConflictHandler?
    ) {
        let jobId = connection.enqueueUploadFolder(
            srcDir: source.path, parent: parent, mode: mode)
        contexts[JobKey(connectionId: connectionId, jobId: jobId)] = JobContext(
            onFinished: onFinished,
            upload: UploadRequest(
                connection: connection, connectionId: connectionId,
                source: source, parent: parent,
                name: source.lastPathComponent,
                isDir: true, onFinished: onFinished, conflictHandler: conflictHandler
            )
        )
    }

    // MARK: - Finder 프로미스

    /// 앱→Finder 파일 프로미스 드롭. 파일은 프로미스가 정한 destination에 직접 받고,
    /// 폴더는 임시 디렉터리에 받아 완료 후 옮긴다. 완료/실패 시 completion을 호출한다.
    func startPromiseDownload(
        connection: PierConnection,
        connectionId: UUID,
        entry: PierEntry,
        destination: URL,
        completion: @escaping @MainActor @Sendable (Error?) -> Void
    ) {
        if entry.isDir {
            let prefix =
                entry.identifier.hasSuffix("/") ? entry.identifier : entry.identifier + "/"
            let tempDir = FileManager.default.temporaryDirectory
                .appendingPathComponent("pier-promise-\(UUID().uuidString)", isDirectory: true)
            let jobId = connection.enqueueDownloadFolder(prefix: prefix, destDir: tempDir.path)
            contexts[JobKey(connectionId: connectionId, jobId: jobId)] = JobContext(
                promise: PromiseRequest(
                    destination: destination, isDir: true,
                    tempDir: tempDir, completion: completion
                )
            )
        } else {
            let jobId = connection.enqueueDownload(
                key: entry.identifier, destPath: destination.path)
            contexts[JobKey(connectionId: connectionId, jobId: jobId)] = JobContext(
                promise: PromiseRequest(
                    destination: destination, isDir: false,
                    tempDir: nil, completion: completion
                )
            )
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
