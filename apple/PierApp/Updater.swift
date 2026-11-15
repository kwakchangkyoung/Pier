import Sparkle
import SwiftUI

/// 앱 수명 동안 Sparkle 업데이트 컨트롤러를 보유하고 메뉴 상태를 노출한다.
/// SPUStandardUpdaterController는 한 번 만들어 계속 살아 있어야 한다.
@Observable
final class UpdaterController {
    private let controller: SPUStandardUpdaterController
    /// 델리게이트는 약한 참조로 보유되므로 컨트롤러가 직접 유지한다.
    private let installDelegate: UpdateInstallDelegate
    private var observation: NSKeyValueObservation?

    /// "업데이트 확인…" 메뉴의 활성 여부. SPUUpdater의 KVO 값을 옮겨 담는다.
    private(set) var canCheckForUpdates = false

    init(transfers: TransferManager) {
        installDelegate = UpdateInstallDelegate(transfers: transfers)
        controller = SPUStandardUpdaterController(
            startingUpdater: true,
            updaterDelegate: installDelegate,
            userDriverDelegate: nil
        )
        observation = controller.updater.observe(
            \.canCheckForUpdates,
            options: [.initial, .new]
        ) { [weak self] updater, _ in
            let value = updater.canCheckForUpdates
            Task { @MainActor in
                self?.canCheckForUpdates = value
            }
        }
    }

    /// 메뉴 "업데이트 확인…"이 호출한다. 사용자 동작이므로 Sparkle이 진행률 UI를 띄운다.
    @MainActor
    func checkForUpdates() {
        controller.updater.checkForUpdates()
    }
}

/// Sparkle 업데이트 설치·재시동 타이밍을 전송 상태와 맞춘다.
/// 진행 중 전송(대기·실행·일시정지)이 있으면 재시동을 미루고, 끝나면 이어서 진행한다.
final class UpdateInstallDelegate: NSObject, SPUUpdaterDelegate {
    private weak var transfers: TransferManager?
    /// 전송이 끝날 때까지 미뤄 둔 설치 완료 블록들.
    @MainActor private var pendingInstalls: [() -> Void] = []
    /// 폴링 태스크가 이미 도는지.
    @MainActor private var isWaiting = false

    init(transfers: TransferManager) {
        self.transfers = transfers
    }

    func updater(
        _ updater: SPUUpdater,
        shouldPostponeRelaunchForUpdate item: SUAppcastItem,
        untilInvokingBlock installationHandler: @escaping () -> Void
    ) -> Bool {
        // Sparkle는 델리게이트를 메인 스레드에서 호출한다.
        // 혹시 다른 스레드로 오면 메인 큐로 동기 전달한다.
        if Thread.isMainThread {
            return MainActor.assumeIsolated { self.postponeIfBusy(installationHandler) }
        }
        return DispatchQueue.main.sync {
            MainActor.assumeIsolated { self.postponeIfBusy(installationHandler) }
        }
    }

    @MainActor
    private func postponeIfBusy(_ installationHandler: @escaping () -> Void) -> Bool {
        guard let transfers, transfers.hasActiveJobs else { return false }
        pendingInstalls.append(installationHandler)
        if !isWaiting {
            isWaiting = true
            waitForTransfersToFinish()
        }
        return true
    }

    /// 전송 대기열이 빌 때까지 폴링한 뒤 보관해 둔 설치 블록을 모두 호출한다.
    /// 대기열 상태 변화 통지는 코어에서 오지 않으므로 1초 폴말로 충분하다.
    @MainActor
    private func waitForTransfersToFinish() {
        Task { @MainActor [weak self] in
            while let self, self.transfers?.hasActiveJobs == true {
                try? await Task.sleep(for: .seconds(1))
            }
            guard let self else { return }
            let installs = self.pendingInstalls
            self.pendingInstalls = []
            self.isWaiting = false
            for install in installs {
                install()
            }
        }
    }
}
