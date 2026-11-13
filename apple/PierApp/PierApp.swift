import SwiftUI

@main
struct PierApp: App {
    @State private var store: ConnectionStore
    @State private var sessions: PierSessions
    @State private var downloads: DownloadManager

    init() {
        _store = State(initialValue: ConnectionStore())
        _sessions = State(initialValue: PierSessions())
        _downloads = State(initialValue: DownloadManager())
    }

    var body: some Scene {
        // Window는 단일 창 scene이다. openWindow는 새 창을 만들지 않고 기존 창을 앞으로 가져온다.
        Window("Pier", id: "main") {
            ContentView(store: store, sessions: sessions, downloads: downloads)
                .frame(minWidth: 720, minHeight: 460)
        }
        .defaultSize(width: 960, height: 620)

        Settings {
            SettingsView(store: store, sessions: sessions)
        }

        MenuBarExtra("Pier", systemImage: "externaldrive") {
            MenuBarContent(store: store, sessions: sessions)
        }
        .menuBarExtraStyle(.menu)
    }
}
