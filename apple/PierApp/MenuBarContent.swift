import AppKit
import SwiftUI

/// 메뉴바 메뉴 내용.
struct MenuBarContent: View {
    let store: ConnectionStore
    let sessions: PierSessions
    @Environment(\.openWindow) private var openWindow
    @Environment(\.openSettings) private var openSettings

    var body: some View {
        Button("창 열기") {
            openMainWindow()
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

    private func openMainWindow() {
        openWindow(id: "main")
        NSApp.activate()
    }
}
