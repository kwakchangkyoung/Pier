import SwiftUI

/// 메인 창: 연결 사이드바 + 파일 브라우저.
struct ContentView: View {
    let store: ConnectionStore
    @Bindable var sessions: PierSessions
    let transfers: TransferManager
    let uploads: UploadController
    @Environment(\.openSettings) private var openSettings

    var body: some View {
        NavigationSplitView {
            sidebar
        } detail: {
            detailContent
        }
        .onChange(of: sessions.selectedId) { _, id in
            sessions.activate(store.connection(withId: id))
        }
        .task {
            if sessions.selectedId == nil, let first = store.connections.first {
                sessions.selectedId = first.id
            }
            sessions.activate(store.connection(withId: sessions.selectedId))
        }
    }

    private var sidebar: some View {
        List(selection: $sessions.selectedId) {
            ForEach(store.connections) { connection in
                Label {
                    VStack(alignment: .leading, spacing: 1) {
                        Text(connection.displayName)
                        Text(sidebarSubtitle(connection))
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                } icon: {
                    Image(systemName: iconName(for: connection.provider))
                }
                .tag(connection.id)
            }
        }
        .navigationTitle("연결")
        .toolbar {
            ToolbarItem(placement: .primaryAction) {
                SettingsLink {
                    Label("설정", systemImage: "gearshape")
                }
            }
        }
    }

    @ViewBuilder
    private var detailContent: some View {
        if let browser = sessions.activeBrowser {
            BrowserView(model: browser, transfers: transfers, uploads: uploads)
                // 연결 재생성(설정 변경) 시 뷰 상태를 초기화하기 위해 인스턴스 기준 id를 쓴다.
                .id(ObjectIdentifier(browser))
        } else {
            ContentUnavailableView {
                Label(emptyTitle, systemImage: "externaldrive")
            } description: {
                if store.connections.isEmpty {
                    Text("설정에서 S3 연결을 추가하면 여기에서 파일을 탐색할 수 있습니다.")
                }
            } actions: {
                if store.connections.isEmpty {
                    Button("설정 열기") { openSettings() }
                }
            }
        }
    }

    private var emptyTitle: String {
        store.connections.isEmpty ? "연결이 없습니다" : "연결을 선택하세요"
    }

    private func iconName(for provider: String) -> String {
        switch provider {
        case "minio": return "shippingbox"
        case "custom": return "globe"
        default: return "cloud"
        }
    }

    private func sidebarSubtitle(_ connection: SavedConnection) -> String {
        if let prefix = connection.prefix, !prefix.isEmpty {
            return "\(connection.bucket)/\(prefix)"
        }
        return connection.bucket
    }
}
