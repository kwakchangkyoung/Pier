import SwiftUI

@main
struct PierApp: App {
    @State private var store: ConnectionStore
    @State private var sessions: PierSessions
    @State private var transfers: TransferManager
    @State private var uploads: UploadController
    @State private var updater: UpdaterController

    init() {
        // 코어 로그를 os_log로 보내는 싱크는 한 번만 설치한다.
        setLogSink(sink: CoreLogSink())
        // 옛 번들 ID 데이터 이관은 ConnectionStore가 connections.json을 읽기 전에 끝낸다.
        LegacyMigration.runIfNeeded()
        _store = State(initialValue: ConnectionStore())
        _sessions = State(initialValue: PierSessions())
        // UpdaterController는 같은 TransferManager를 봐야 하므로 로컬로 먼저 만든다.
        let transferManager = TransferManager()
        _transfers = State(initialValue: transferManager)
        _uploads = State(initialValue: UploadController())
        _updater = State(initialValue: UpdaterController(transfers: transferManager))
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
        .commands {
            // "Pier에 관하여" 아래에 수동 업데이트 확인을 둔다.
            CommandGroup(after: .appInfo) {
                Button("업데이트 확인…") { updater.checkForUpdates() }
                    .disabled(!updater.canCheckForUpdates)
            }
        }

        Settings {
            SettingsView(store: store, sessions: sessions)
        }

        MenuBarExtra {
            MenuBarContent(store: store, sessions: sessions, transfers: transfers, updater: updater)
        } label: {
            // 전송이 진행 중이면 아이콘을 바꿔 상태를 알린다.
            Image(systemName: transfers.hasActiveJobs ? "arrow.up.arrow.down.circle.fill" : "externaldrive")
        }
        .menuBarExtraStyle(.menu)
    }
}
