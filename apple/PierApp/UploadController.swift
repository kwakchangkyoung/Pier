import Foundation

/// 업로드 시작 전 충돌 확인과 전송 도중 Conflict 재질문을 담당한다.
/// activeConflict를 시트에 바인딩하고, 사용자의 선택을 continuation으로 돌려준다.
@MainActor @Observable
final class UploadController {
    /// 시트에 표시할 현재 충돌. nil이면 시트가 닫힌다.
    var activeConflict: TransferConflict?
    /// "모두에 적용"으로 확정된 선택. 있으면 이후 충돌에 그대로 적용한다.
    private var appliedChoice: TransferConflictChoice?
    /// 동시에 발생한 충돌 질문을 순서대로 처리하기 위한 대기열 꼬리.
    private var askTail: Task<TransferConflictChoice?, Never>?
    private var conflictContinuation: CheckedContinuation<TransferConflictChoice?, Never>?

    /// NSOpenPanel/Finder 드롭으로 온 항목들을 parent 폴더에 업로드한다.
    /// 파일은 enqueueUpload, 폴더는 enqueueUploadFolder를 사용한다.
    /// 완료 시 onFinished에 결과를 넘긴다.
    func start(
        urls: [URL],
        parent: String,
        connection: PierConnection,
        connectionId: UUID,
        transfers: TransferManager,
        onFinished: @escaping @MainActor (PierEntry?) -> Void
    ) {
        // "모두에 적용"은 이번 업로드 묶음에만 유효하다.
        appliedChoice = nil
        Task { @MainActor in
            let modes = await resolveModes(urls: urls, parent: parent, connection: connection)
            for (url, mode) in zip(urls, modes) {
                guard let mode else { continue }
                if Self.isDirectory(url) {
                    transfers.enqueueUploadFolder(
                        connection: connection,
                        connectionId: connectionId,
                        source: url,
                        parent: parent,
                        mode: mode,
                        onFinished: onFinished,
                        conflictHandler: conflictHandler
                    )
                } else {
                    transfers.enqueueUpload(
                        connection: connection,
                        connectionId: connectionId,
                        source: url,
                        parent: parent,
                        name: url.lastPathComponent,
                        mode: mode,
                        onFinished: onFinished,
                        conflictHandler: conflictHandler
                    )
                }
            }
        }
    }

    /// 시트에서 선택이 확정되면 호출한다. applyToAll이면 이후 충돌에도 적용한다.
    func resolveActiveConflict(choice: TransferConflictChoice?, applyToAll: Bool) {
        if applyToAll, let choice {
            appliedChoice = choice
        }
        activeConflict = nil
        conflictContinuation?.resume(returning: choice)
        conflictContinuation = nil
    }

    // MARK: - 내부

    /// 업로드 전 existing()으로 충돌을 확인하고 항목별 모드를 정한다.
    /// 반환 배열은 urls와 같은 순서이며, 건너뛰기는 nil이다.
    private func resolveModes(
        urls: [URL],
        parent: String,
        connection: PierConnection
    ) async -> [PierUploadMode?] {
        let names = urls.map { $0.lastPathComponent }
        var existingByName: [String: PierEntry] = [:]
        // 확인에 실패하면 일단 .fail로 진행한다. 실제 충돌은 전송 중 Conflict로 다시 묻는다.
        if let entries = try? await connection.existing(parent: parent, names: names) {
            for entry in entries {
                existingByName[entry.name] = entry
            }
        }
        var modes: [PierUploadMode?] = []
        for url in urls {
            let name = url.lastPathComponent
            guard let existing = existingByName[name] else {
                modes.append(.fail)
                continue
            }
            let isDir = Self.isDirectory(url)
            // nil(시트 닫기)은 건너뛰기로 처리한다.
            guard
                let choice = await askConflict(
                    TransferConflict(name: name, isDir: isDir, parent: parent)
                )
            else {
                modes.append(nil)
                continue
            }
            switch choice {
            case .replace:
                // 확인 시점 ETag로 조건부 대치. 폴더는 ETag가 없어 무조건 대치가 된다.
                modes.append(.replace(ifMatch: existing.etag))
            case .keepBoth:
                modes.append(.keepBoth)
            case .skip:
                modes.append(nil)
            }
        }
        return modes
    }

    /// 전송 도중 Conflict 재질문용 핸들러.
    private var conflictHandler: UploadConflictHandler {
        { [weak self] conflict in
            guard let self else { return nil }
            return await self.askConflict(conflict)
        }
    }

    /// 충돌 질문. "모두에 적용" 선택이 있으면 묻지 않고 반환한다.
    /// 여러 전송이 동시에 Conflict를 내도 시트는 한 번에 하나만 뜨도록 직렬화한다.
    private func askConflict(_ conflict: TransferConflict) async -> TransferConflictChoice? {
        if let appliedChoice { return appliedChoice }
        let previous = askTail
        let task = Task<TransferConflictChoice?, Never> { @MainActor in
            _ = await previous?.value
            return await self.presentConflict(conflict)
        }
        askTail = task
        return await task.value
    }

    /// 시트를 띄우고 사용자의 선택을 기다린다.
    private func presentConflict(_ conflict: TransferConflict) async -> TransferConflictChoice? {
        activeConflict = conflict
        return await withCheckedContinuation { continuation in
            conflictContinuation = continuation
        }
    }

    private static func isDirectory(_ url: URL) -> Bool {
        let values = try? url.resourceValues(forKeys: [.isDirectoryKey])
        return values?.isDirectory ?? false
    }
}
