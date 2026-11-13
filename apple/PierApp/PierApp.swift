import SwiftUI

@main
struct PierApp: App {
    @State private var store: ConnectionStore
    @State private var sessions: PierSessions
    @State private var transfers: TransferManager
    @State private var uploads: UploadController

    init() {
        // 코어 로그를 os_log로 보내는 싱크는 한 번만 설치한다.
        setLogSink(sink: CoreLogSink())
        _store = State(initialValue: ConnectionStore())
        _sessions = State(initialValue: PierSessions())
        _transfers = State(initialValue: TransferManager())
        _uploads = State(initialValue: UploadController())
        // 새로 열리는 연결에 대기열 리스너를 달 수 있게 세션에 집계자를 연결한다.
        sessions.transfers = transfers
    }

    var body: some Scene {
        // Window는 단일 창 scene이다. openWindow는 새 창을 만들지 않고 기존 창을 앞으로 가져온다.
        Window("Pier", id: "main") {
            ContentView(store: store, sessions: sessions, transfers: transfers, uploads: uploads)
                .frame(minWidth: 720, minHeight: 460)
        }
        .defaultSize(width: 960, height: 620)

        Settings {
            SettingsView(store: store, sessions: sessions)
        }

        MenuBarExtra {
            MenuBarContent(store: store, sessions: sessions, transfers: transfers)
        } label: {
            // 전송이 진행 중이면 아이콘을 바꿔 상태를 알린다.
            Image(systemName: transfers.hasActiveJobs ? "arrow.up.arrow.down.circle.fill" : "externaldrive")
        }
        .menuBarExtraStyle(.menu)
    }
}
