import Foundation
import Quartz

/// 스페이스바 Quick Look 미리보기 상태를 관리한다.
/// preview(key:progress:cancel:)로 연결 캐시 디렉터리에 받은 파일을 QLPreviewPanel로 보여준다.
@MainActor @Observable
final class PreviewController {
    /// 다운로드 진행 중인지. HUD 표시 기준.
    private(set) var isLoading = false
    private(set) var transferred: UInt64 = 0
    private(set) var total: UInt64 = 0
    /// 패널 데이터 소스. 닫힘 통지를 받아 취소로 연결한다.
    let itemSource = PreviewItemSource()
    /// 미리보기 실패 시 사용자에게 보여줄 메시지를 넘기는 콜백.
    var onError: (@MainActor (String) -> Void)?

    private var cancelToken: PierCancelToken?
    /// 진행 중/표시 중인 객체 키. 선택 변경 비교에 쓴다.
    private var currentKey: String?

    init() {
        itemSource.onClose = { [weak self] in
            Task { @MainActor in
                self?.cancelDownload()
            }
        }
    }

    /// 스페이스바 동작: 같은 항목이면 닫고, 아니면 새로 받아 연다.
    func toggle(entry: PierEntry, connection: PierConnection) {
        if isPanelVisible, currentKey == entry.identifier {
            cancelDownload()
            return
        }
        start(entry: entry, connection: connection)
    }

    /// 선택이 바뀌면 진행 중이던 다운로드를 취소하고 패널을 닫는다.
    func selectionDidChange(selected: [PierEntry]) {
        guard currentKey != nil else { return }
        let first = selected.first { !$0.isDir }
        if first?.identifier != currentKey {
            cancelDownload()
        }
    }

    // MARK: - 내부

    private var isPanelVisible: Bool {
        QLPreviewPanel.sharedPreviewPanelExists() && QLPreviewPanel.shared()?.isVisible == true
    }

    private func start(entry: PierEntry, connection: PierConnection) {
        cancelDownload()
        currentKey = entry.identifier
        isLoading = true
        transferred = 0
        total = entry.size
        let token = PierCancelToken()
        cancelToken = token
        let progress = ProgressBridge { [weak self] done, full in
            Task { @MainActor in
                self?.updateProgress(transferred: done, total: full)
            }
        }
        Task { @MainActor [weak self] in
            guard let self else { return }
            do {
                let path = try await connection.preview(
                    key: entry.identifier, progress: progress, cancel: token)
                guard self.currentKey == entry.identifier else { return }
                self.isLoading = false
                self.itemSource.url = URL(fileURLWithPath: path)
                self.showPanel()
            } catch {
                self.finishLoading()
                if let message = PierErrorPresenter.message(for: error) {
                    self.onError?(message)
                }
            }
        }
    }

    private func updateProgress(transferred: UInt64, total: UInt64) {
        self.transferred = transferred
        if total > 0 {
            self.total = total
        }
    }

    private func showPanel() {
        guard let panel = QLPreviewPanel.shared() else { return }
        panel.dataSource = itemSource
        panel.delegate = itemSource
        panel.reloadData()
        panel.makeKeyAndOrderFront(nil)
    }

    /// 다운로드 취소 + 패널 닫기 + 상태 정리.
    func cancelDownload() {
        cancelToken?.cancel()
        cancelToken = nil
        finishLoading()
        itemSource.url = nil
        if isPanelVisible {
            QLPreviewPanel.shared()?.orderOut(nil)
        }
    }

    private func finishLoading() {
        isLoading = false
        currentKey = nil
        transferred = 0
        total = 0
    }
}

/// QLPreviewPanel의 데이터 소스/델리게이트. 로컬 파일 URL 하나를 보여준다.
/// 패널이 닫히면 onClose로 알린다(다운로드 취소 연결용).
final class PreviewItemSource: NSObject, QLPreviewPanelDataSource, QLPreviewPanelDelegate {
    /// 미리보기할 로컬 파일. nil이면 항목 없음.
    var url: URL?
    /// 패널이 닫힐 때 호출. 메인 스레드에서 온다.
    var onClose: (@Sendable () -> Void)?

    func numberOfPreviewItems(in panel: QLPreviewPanel) -> Int {
        url == nil ? 0 : 1
    }

    func previewPanel(_ panel: QLPreviewPanel, previewItemAt index: Int) -> QLPreviewItem? {
        url.map { $0 as NSURL }
    }

    func windowShouldClose(_ sender: NSWindow) -> Bool {
        onClose?()
        return true
    }
}

/// FFI `PierProgress` 구현. Rust가 백그라운드 스레드에서 호출하므로
/// 0.1초로 스로틀한 뒤 메인 액터로 넘긴다.
final class ProgressBridge: PierProgress, @unchecked Sendable {
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
