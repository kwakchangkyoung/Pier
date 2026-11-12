import AppKit
import SwiftUI

@main
struct PierApp: App {
    @State private var browserModel = RootBrowserModel()

    var body: some Scene {
        WindowGroup(id: "main") {
            RootBrowserView(model: browserModel)
                .frame(minWidth: 640, minHeight: 400)
        }
        .defaultSize(width: 760, height: 480)

        MenuBarExtra("Pier", systemImage: "externaldrive") {
            MenuBarContent()
        }
        .menuBarExtraStyle(.menu)
    }
}

/// 메뉴바 메뉴 내용.
private struct MenuBarContent: View {
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        Button("창 열기") {
            openWindow(id: "main")
            NSApp.activate()
        }
        Divider()
        Button("종료") {
            NSApp.terminate(nil)
        }
    }
}
