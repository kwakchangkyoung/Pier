import Foundation

/// 프로미스 완료 핸들러에 넘길 최소 오류.
struct PromiseTransferError: LocalizedError {
    let message: String
    var errorDescription: String? { message }
}

/// TransferManager의 작업 종료 후속 동작.
/// apply()에서 종료 상태를 감지하면 finishJob이 contexts를 꺼내 한 번만 실행한다.
extension TransferManager {
    /// 종료 상태에 들어간 작업의 후속 동작을 한 번 실행한다.
    /// contexts를 꺼내 소비하므로 같은 작업에 두 번 실행되지 않는다.
    func finishJob(key: JobKey, job: PierJob) {
        guard let context = contexts.removeValue(forKey: key) else { return }
        switch job.state {
        case .completed:
            if let promise = context.promise {
                completePromise(promise)
            }
            context.onFinished?(job.resultEntry)
        case .failed:
            if job.error?.kind == .conflict, let upload = context.upload {
                reaskConflict(key: key, job: job, upload: upload)
                return
            }
            context.promise?.completion(
                PromiseTransferError(message: job.error?.message ?? "전송에 실패했습니다."))
        case .cancelled:
            context.promise?.completion(
                PromiseTransferError(message: "전송이 취소되었습니다."))
        default:
            break
        }
    }

    // MARK: - 내부

    /// Conflict 실패 시 기존처럼 다시 묻고, 선택하면 새 모드로 새 작업을 enqueue한다.
    /// 사용자가 답하면(건너뛰기 포함) 실패한 작업은 목록에서 지운다.
    private func reaskConflict(key: JobKey, job: PierJob, upload: UploadRequest) {
        guard let conflictHandler = upload.conflictHandler else { return }
        Task { @MainActor in
            let conflict = TransferConflict(
                name: job.name, isDir: upload.isDir, parent: upload.parent)
            // nil(시트 닫기)은 건너뛰기로 처리한다.
            let choice = await conflictHandler(conflict) ?? .skip
            removeByKey(key)
            switch choice {
            case .skip:
                return
            case .keepBoth:
                reenqueue(upload: upload, mode: .keepBoth)
            case .replace:
                let mode = await refreshedReplaceMode(
                    connection: upload.connection,
                    parent: upload.parent, name: upload.name, isDir: upload.isDir
                )
                reenqueue(upload: upload, mode: mode)
            }
        }
    }

    /// 같은 요청을 새 모드로 다시 enqueue하고 context를 이어 붙인다.
    private func reenqueue(upload: UploadRequest, mode: PierUploadMode) {
        let jobId =
            upload.isDir
            ? upload.connection.enqueueUploadFolder(
                srcDir: upload.source.path, parent: upload.parent, mode: mode)
            : upload.connection.enqueueUpload(
                srcPath: upload.source.path, parent: upload.parent,
                name: upload.name, mode: mode)
        contexts[JobKey(connectionId: upload.connectionId, jobId: jobId)] = JobContext(
            onFinished: upload.onFinished,
            upload: upload
        )
    }

    /// 대치 재시도용 모드. 파일은 원격 ETag를 다시 읽어 조건부 쓰기로,
    /// 폴더는 파일별 ETag를 알 수 없어 무조건 대치로 보낸다.
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

    /// 폴더 프로미스 완료 처리. 임시 디렉터리에 받은 '<폴더 이름>'을
    /// 프로미스가 요구한 destination으로 옮긴다.
    private func completePromise(_ request: PromiseRequest) {
        do {
            if request.isDir {
                try moveFolderDownload(request)
            }
            request.completion(nil)
        } catch {
            request.completion(error)
        }
    }

    /// downloadFolder 결과(임시 디렉터리 안의 유일한 폴더)를 destination으로 옮긴다.
    /// 수신자가 이미 디렉터리를 만들었으면 내용을 그 안으로 옮긴다.
    private func moveFolderDownload(_ request: PromiseRequest) throws {
        guard let tempDir = request.tempDir else { return }
        let manager = FileManager.default
        defer { try? manager.removeItem(at: tempDir) }
        let children = try manager.contentsOfDirectory(
            at: tempDir, includingPropertiesForKeys: nil)
        guard let source = children.first else { return }
        let destination = request.destination
        if manager.fileExists(atPath: destination.path) {
            for child in try manager.contentsOfDirectory(
                at: source, includingPropertiesForKeys: nil)
            {
                try manager.moveItem(
                    at: child,
                    to: destination.appendingPathComponent(
                        child.lastPathComponent, isDirectory: true)
                )
            }
        } else {
            try manager.moveItem(at: source, to: destination)
        }
    }
}
