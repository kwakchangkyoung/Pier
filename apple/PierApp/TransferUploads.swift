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

/// TransferManager의 업로드 경로. 멀티파트·조건부 쓰기·재질문 재시도는 코어가 처리하고
/// 여기서는 모드 결정과 재시도 루프만 담당한다.
extension TransferManager {
    /// 단일 파일 업로드. 완료 시 onFinished에 결과 PierEntry를 넘긴다.
    /// conflictHandler가 있으면 Conflict 오류 시 재질문 후 모드를 바꿔 재시도한다.
    func uploadFile(
        connection: PierConnection,
        source: URL,
        parent: String,
        name: String,
        mode: PierUploadMode,
        onFinished: (@MainActor (PierEntry?) -> Void)? = nil,
        conflictHandler: UploadConflictHandler? = nil
    ) {
        let remoteKey = parent.isEmpty ? name : parent + name
        let item = TransferItem(
            name: name,
            key: remoteKey,
            isFolder: false,
            direction: .upload,
            total: localFileSize(source),
            destinationPath: source.path
        )
        let token = begin(item: item)
        let id = item.id
        let progress = ProgressBridge { [weak self] transferred, total in
            Task { @MainActor in
                self?.recordProgress(id: id, transferred: transferred, total: total)
            }
        }
        Task { @MainActor in
            var currentMode = mode
            while true {
                do {
                    let entry = try await connection.upload(
                        srcPath: source.path,
                        parent: parent,
                        name: name,
                        mode: currentMode,
                        progress: progress,
                        cancel: token
                    )
                    completeUploadItem(id: id, entry: entry, folderResult: nil)
                    onFinished?(entry)
                    return
                } catch {
                    guard
                        let retryMode = await uploadRetryMode(
                            for: error,
                            id: id,
                            item: item,
                            connection: connection,
                            parent: parent,
                            conflictHandler: conflictHandler
                        )
                    else { return }
                    currentMode = retryMode
                }
            }
        }
    }

    /// 폴더 재귀 업로드. 코어가 3.6 제외 규칙을 적용하고 srcDir 이름으로
    /// parent 아래 폴더를 만든다. mode는 하위 모든 파일에 적용된다.
    /// 완료 시 onFinished에 nil을 넘긴다(결과에 항목이 없다).
    func uploadFolder(
        connection: PierConnection,
        source: URL,
        parent: String,
        mode: PierUploadMode,
        onFinished: (@MainActor (PierEntry?) -> Void)? = nil,
        conflictHandler: UploadConflictHandler? = nil
    ) {
        let name = source.lastPathComponent
        let prefix = parent.isEmpty ? name + "/" : parent + name + "/"
        let item = TransferItem(name: name, key: prefix, isFolder: true, direction: .upload)
        let token = begin(item: item)
        let id = item.id
        let progress = ProgressBridge { [weak self] transferred, total in
            Task { @MainActor in
                self?.recordProgress(id: id, transferred: transferred, total: total)
            }
        }
        Task { @MainActor in
            var currentMode = mode
            while true {
                do {
                    let result = try await connection.uploadFolder(
                        srcDir: source.path,
                        parent: parent,
                        mode: currentMode,
                        progress: progress,
                        cancel: token
                    )
                    completeUploadItem(id: id, entry: nil, folderResult: result)
                    onFinished?(nil)
                    return
                } catch {
                    guard
                        let retryMode = await uploadRetryMode(
                            for: error,
                            id: id,
                            item: item,
                            connection: connection,
                            parent: parent,
                            conflictHandler: conflictHandler
                        )
                    else { return }
                    currentMode = retryMode
                }
            }
        }
    }

    /// Finder로의 파일 프로미스 드롭. download/downloadFolder 결과를
    /// destination URL에 직접 쓴다(프로미스 수신자가 정한 최종 위치).
    /// 완료/실패 시 completion을 호출한다.
    func startPromiseDownload(
        connection: PierConnection,
        entry: PierEntry,
        destination: URL,
        completion: @escaping @MainActor @Sendable (Error?) -> Void
    ) {
        let item = TransferItem(
            name: entry.name,
            key: entry.identifier,
            isFolder: entry.isDir,
            direction: .download,
            total: entry.size,
            destinationPath: destination.path
        )
        let token = begin(item: item)
        let id = item.id
        let progress = ProgressBridge { [weak self] transferred, total in
            Task { @MainActor in
                self?.recordProgress(id: id, transferred: transferred, total: total)
            }
        }
        Task { @MainActor in
            do {
                if entry.isDir {
                    try await downloadFolderToPromiseDestination(
                        connection: connection,
                        entry: entry,
                        destination: destination,
                        progress: progress,
                        cancel: token
                    )
                } else {
                    _ = try await connection.download(
                        key: entry.identifier,
                        destPath: destination.path,
                        progress: progress,
                        cancel: token
                    )
                }
                finish(id: id, state: .completed)
                completion(nil)
            } catch {
                fail(id: id, error: error)
                completion(error)
            }
        }
    }

    // MARK: - 내부

    /// 업로드 오류 처리. Conflict이고 재질문 핸들러가 있으면 사용자 선택에 따라
    /// 재시도할 모드를 반환한다. 종료(실패/취소/건너뛰기)면 nil.
    private func uploadRetryMode(
        for error: Error,
        id: UUID,
        item: TransferItem,
        connection: PierConnection,
        parent: String,
        conflictHandler: UploadConflictHandler?
    ) async -> PierUploadMode? {
        if isConflict(error), let conflictHandler {
            let context = TransferConflict(name: item.name, isDir: item.isFolder, parent: parent)
            // nil(시트 닫기)은 건너뛰기로 처리한다.
            guard let choice = await conflictHandler(context) else {
                remove(id)
                return nil
            }
            switch choice {
            case .replace:
                return await refreshedReplaceMode(
                    connection: connection, parent: parent, name: item.name, isDir: item.isFolder
                )
            case .keepBoth:
                return .keepBoth
            case .skip:
                remove(id)
                return nil
            }
        }
        // Conflict가 아니거나 핸들러가 없으면 일반 실패/취소 처리.
        fail(id: id, error: error)
        return nil
    }

    /// 대치 재시도용 모드. 파일은 원격 ETag를 다시 읽어 조건부 쓰기로,
    /// 폴더는 파일별 ETag를 알 수 없어 무조건 대치로 보낸다(모드는 전체에 적용).
    private func refreshedReplaceMode(
        connection: PierConnection,
        parent: String,
        name: String,
        isDir: Bool
    ) async -> PierUploadMode {
        guard !isDir else { return .replace(ifMatch: nil) }
        let entries = try? await connection.existing(parent: parent, names: [name])
        let etag = entries?.first(where: { $0.name == name && !$0.isDir })?.etag
        return .replace(ifMatch: etag)
    }

    /// PierError.Conflict 판별.
    private func isConflict(_ error: Error) -> Bool {
        guard let pierError = error as? PierError else { return false }
        if case .Conflict = pierError { return true }
        return false
    }

    private func localFileSize(_ url: URL) -> UInt64 {
        let values = try? url.resourceValues(forKeys: [.fileSizeKey])
        return UInt64(values?.fileSize ?? 0)
    }

    /// 폴더 프로미스용 다운로드. downloadFolder는 destDir 안에 '<폴더 이름>'을
    /// 만들므로 임시 디렉터리에 받은 뒤 프로미스가 요구하는 url로 이동한다.
    /// url의 마지막 경로 구성요소가 이미 폴더 이름과 같으므로 이름을 다시 맞출 필요가 없다.
    private func downloadFolderToPromiseDestination(
        connection: PierConnection,
        entry: PierEntry,
        destination: URL,
        progress: ProgressBridge,
        cancel: PierCancelToken
    ) async throws {
        let prefix = entry.identifier.hasSuffix("/") ? entry.identifier : entry.identifier + "/"
        let tempDir = FileManager.default.temporaryDirectory
            .appendingPathComponent("pier-promise-\(UUID().uuidString)", isDirectory: true)
        let result = try await connection.downloadFolder(
            prefix: prefix,
            destDir: tempDir.path,
            progress: progress,
            cancel: cancel
        )
        let source = URL(fileURLWithPath: result.localPath, isDirectory: true)
        let manager = FileManager.default
        if manager.fileExists(atPath: destination.path) {
            // 수신자가 이미 디렉터리를 만들었으면 내용을 그 안으로 옮긴다.
            for child in try manager.contentsOfDirectory(at: source, includingPropertiesForKeys: nil) {
                try manager.moveItem(
                    at: child,
                    to: destination.appendingPathComponent(child.lastPathComponent, isDirectory: true)
                )
            }
            try? manager.removeItem(at: source)
        } else {
            try manager.moveItem(at: source, to: destination)
        }
    }
}
