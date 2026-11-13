import AppKit

/// 테이블 행을 Finder로 끌 때 쓰는 NSFilePromiseProvider의 delegate.
/// AppKit이 fileNameForType으로 약속 파일명을, writePromiseToURL로
/// 드롭 위치의 최종 URL을 넘겨주므로 임시 파일 없이 그곳으로 바로 받는다.
///
/// 수명: delegate는 weak라 드래그 세션 동안 살아 있어야 한다.
/// FileTableCoordinator가 인스턴스를 보관하고 writePromise 완료 시 제거한다.
@MainActor
final class PromiseDownloadHandler: NSObject, NSFilePromiseProviderDelegate {
    private let entry: PierEntry
    private let connection: PierConnection
    private let connectionId: UUID
    private let transfers: TransferManager
    /// writePromiseToURL이 완료됐는지. 세션 종료 시 정리 기준으로 쓴다.
    private(set) var didFinish = false

    /// 프로바이더가 요구하는 파일 타입. 폴더는 public.folder여야 한다.
    private let fileType: String

    init(
        entry: PierEntry,
        connection: PierConnection,
        connectionId: UUID,
        transfers: TransferManager
    ) {
        self.entry = entry
        self.connection = connection
        self.connectionId = connectionId
        self.transfers = transfers
        self.fileType = entry.isDir ? "public.folder" : "public.data"
        super.init()
    }

    /// 드래그 pasteboard에 쓸 프로바이더. delegate는 self(weak)이므로
    /// 코디네이터가 이 핸들러를 세션 동안 보관해야 한다.
    func makeProvider() -> NSFilePromiseProvider {
        NSFilePromiseProvider(fileType: fileType, delegate: self)
    }

    /// 약속 파일의 기본 이름. '/'는 붙이지 않는다.
    nonisolated func filePromiseProvider(
        _ filePromiseProvider: NSFilePromiseProvider,
        fileNameForType fileType: String
    ) -> String {
        entry.name
    }

    /// 드롭 위치 URL로 다운로드한다. 폴더는 enqueueDownloadFolder가 destDir 안에
    /// '<폴더 이름>'을 만들기 때문에 TransferManager가 임시 디렉터리 경유로 옮긴다.
    nonisolated func filePromiseProvider(
        _ filePromiseProvider: NSFilePromiseProvider,
        writePromiseTo url: URL,
        completionHandler: @escaping (Error?) -> Void
    ) {
        Task { @MainActor in
            self.transfers.startPromiseDownload(
                connection: self.connection,
                connectionId: self.connectionId,
                entry: self.entry,
                destination: url
            ) { error in
                self.didFinish = true
                completionHandler(error)
            }
        }
    }
}
