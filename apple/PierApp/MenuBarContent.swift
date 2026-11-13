import AppKit
import SwiftUI

/// 메뉴바 메뉴 내용. 진행 중 전송 요약, 연결 목록, 새로고침, 설정을 담는다.
struct MenuBarContent: View {
    let store: ConnectionStore
    let sessions: PierSessions
    let transfers: TransferManager
    @Environment(\.openWindow) private var openWindow
    @Environment(\.openSettings) private var openSettings

    var body: some View {
        Button("창 열기") {
            openMainWindow()
        }
        if transfers.hasActiveJobs {
            Divider()
            Section("전송") {
                ForEach(transfers.runningItems(limit: 3)) { item in
                    Text(jobSummary(item))
                }
                Text(totalSummary)
                Button("전송 창 열기") {
                    transfers.panelExpanded = true
                    openMainWindow()
                }
            }
        }
        if !store.connections.isEmpty {
            Divider()
            Section("연결") {
                ForEach(store.connections) { connection in
                    Button(connection.displayName) {
                        sessions.activate(connection)
                        openMainWindow()
                    }
                }
            }
        }
        Divider()
        Button("현재 폴더 새로고침") {
            sessions.activeBrowser?.reload()
        }
        .disabled(sessions.activeBrowser == nil)
        Divider()
        Button("설정…") {
            openSettings()
        }
        Divider()
        Button("종료") {
            NSApp.terminate(nil)
        }
    }

    /// 실행 중 작업 한 줄 요약: "↑ name — 45%" / "↓ name — 1.2 MB/s".
    private func jobSummary(_ item: QueueItem) -> String {
        let arrow = item.job.direction == .download ? "↓" : "↑"
        var text = "\(arrow) \(item.job.name)"
        if item.job.total > 0 {
            let percent = Int(Double(item.job.transferred) / Double(item.job.total) * 100)
            text += " — \(percent)%"
        } else if item.job.bytesPerSec > 0 {
            text += " — \(formatBytes(item.job.bytesPerSec))/s"
        }
        return text
    }

    /// "전체 n개 · 12.3 MB/s" 요약.
    private var totalSummary: String {
        var text = "전체 \(transfers.activeCount)개"
        if transfers.aggregateSpeed > 0 {
            text += " · \(formatBytes(transfers.aggregateSpeed))/s"
        }
        return text
    }

    private func formatBytes(_ bytes: UInt64) -> String {
        ByteCountFormatter.string(fromByteCount: Int64(bytes), countStyle: .file)
    }

    private func openMainWindow() {
        openWindow(id: "main")
        NSApp.activate()
    }
}
