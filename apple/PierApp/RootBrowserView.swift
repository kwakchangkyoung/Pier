import Foundation
import SwiftUI

/// 루트 목록을 보여주는 메인 창 내용.
struct RootBrowserView: View {
    let model: RootBrowserModel

    var body: some View {
        Group {
            switch model.phase {
            case .idle, .loading:
                ProgressView("목록을 불러오는 중…")
            case .failed(let message) where model.entries.isEmpty:
                errorView(message: message)
            case .failed, .ready:
                table
            }
        }
        .task { await model.load() }
        .toolbar {
            ToolbarItem(placement: .primaryAction) {
                Button {
                    Task { await model.load() }
                } label: {
                    Label("새로고침", systemImage: "arrow.clockwise")
                }
                .keyboardShortcut("r")
            }
        }
    }

    private var table: some View {
        Table(model.entries) {
            TableColumn("이름") { entry in
                Label(entry.name, systemImage: entry.isDir ? "folder" : "doc")
            }
            TableColumn("크기") { entry in
                if entry.isDir {
                    Text("—")
                } else {
                    Text(ByteCountFormatter.string(fromByteCount: Int64(entry.size), countStyle: .file))
                }
            }
            TableColumn("수정 시각") { entry in
                if let ms = entry.lastModifiedMs {
                    Text(Date(timeIntervalSince1970: Double(ms) / 1000), format: .dateTime)
                } else {
                    Text("—")
                }
            }
        }
    }

    private func errorView(message: String) -> some View {
        ContentUnavailableView {
            Label("불러오지 못했습니다", systemImage: "exclamationmark.triangle")
        } description: {
            Text(message)
        } actions: {
            Button("다시 시도") {
                Task { await model.load() }
            }
        }
    }
}
