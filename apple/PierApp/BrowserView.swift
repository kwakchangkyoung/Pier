import AppKit
import SwiftUI

/// 선택한 연결의 파일 브라우저 본문.
struct BrowserView: View {
    let model: BrowserModel
    let downloads: DownloadManager
    @State private var selection = Set<String>()

    var body: some View {
        VStack(spacing: 0) {
            breadcrumbBar
            Divider()
            if let message = errorBannerMessage {
                bannerView(message)
                Divider()
            }
            content
            if !downloads.items.isEmpty {
                DownloadBar(manager: downloads)
            }
        }
        .navigationTitle(model.title)
        .toolbar { toolbarItems }
    }

    // MARK: - breadcrumb

    private var breadcrumbBar: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 2) {
                ForEach(model.path.indices, id: \.self) { index in
                    if index > 0 {
                        Image(systemName: "chevron.right")
                            .font(.caption2)
                            .foregroundStyle(.tertiary)
                    }
                    Button {
                        model.jumpToBreadcrumb(index)
                    } label: {
                        Text(segmentTitle(at: index))
                            .lineLimit(1)
                    }
                    .buttonStyle(.plain)
                    .font(index == model.path.count - 1 ? .body.weight(.medium) : .body)
                    .foregroundStyle(index == model.path.count - 1 ? .primary : .secondary)
                }
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 7)
        }
    }

    private func segmentTitle(at index: Int) -> String {
        let folder = model.path[index]
        guard !folder.isEmpty else { return model.title }
        let trimmed = folder.hasSuffix("/") ? String(folder.dropLast()) : folder
        return trimmed.split(separator: "/").last.map(String.init) ?? folder
    }

    // MARK: - 내용

    /// 목록이 있는데 phase가 failed면 상단에 오류 배너를 띄운다(갱신 실패 등).
    private var errorBannerMessage: String? {
        guard case .failed(let message) = model.phase, !model.entries.isEmpty else { return nil }
        return message
    }

    private func bannerView(_ message: String) -> some View {
        HStack(spacing: 8) {
            Image(systemName: "exclamationmark.triangle.fill")
                .foregroundStyle(.yellow)
            Text(message)
                .font(.callout)
                .lineLimit(2)
            Spacer()
            Button("닫기") { model.clearError() }
                .controlSize(.small)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
        .background(.bar)
    }

    @ViewBuilder
    private var content: some View {
        if model.entries.isEmpty {
            emptyState
        } else {
            FileTableView(
                entries: model.entries,
                selection: $selection,
                onOpenFolder: { entry in
                    model.navigate(toFolder: entry.identifier)
                },
                onDownload: { entries in
                    downloadEntries(entries)
                }
            )
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    @ViewBuilder
    private var emptyState: some View {
        switch model.phase {
        case .idle, .loading:
            ProgressView("목록을 불러오는 중…")
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        case .failed(let message):
            ContentUnavailableView {
                Label("불러오지 못했습니다", systemImage: "exclamationmark.triangle")
            } description: {
                Text(message)
            } actions: {
                Button("다시 시도") { model.reload() }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        case .ready:
            ContentUnavailableView("빈 폴더", systemImage: "folder")
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    // MARK: - 툴바/다운로드

    @ToolbarContentBuilder
    private var toolbarItems: some ToolbarContent {
        ToolbarItemGroup(placement: .navigation) {
            Button {
                model.goBack()
            } label: {
                Label("뒤로", systemImage: "chevron.left")
            }
            .disabled(!model.canGoBack)
            Button {
                model.goForward()
            } label: {
                Label("앞으로", systemImage: "chevron.right")
            }
            .disabled(!model.canGoForward)
        }
        ToolbarItemGroup(placement: .primaryAction) {
            if model.phase == .loading || model.isRefreshing {
                ProgressView()
                    .controlSize(.small)
            }
            Button {
                model.reload()
            } label: {
                Label("새로고침", systemImage: "arrow.clockwise")
            }
            .keyboardShortcut("r")
            Button {
                downloadSelection()
            } label: {
                Label("다운로드", systemImage: "arrow.down.to.line")
            }
            .disabled(!canDownload)
        }
    }

    private var selectedEntries: [PierEntry] {
        model.entries.filter { selection.contains($0.identifier) }
    }

    /// 선택 항목이 하나라도 있으면 다운로드할 수 있다(파일·폴더 모두 지원).
    private var canDownload: Bool {
        !selectedEntries.isEmpty
    }

    private func downloadSelection() {
        downloadEntries(selectedEntries)
    }

    /// 선택한 파일·폴더를 한 번의 패널로 저장 위치를 고른 뒤 DownloadManager에 넘긴다.
    private func downloadEntries(_ entries: [PierEntry]) {
        guard !entries.isEmpty, let connection = model.connection else { return }
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        panel.prompt = "다운로드"
        panel.message = "항목을 저장할 폴더를 선택하세요."
        guard panel.runModal() == .OK, let directory = panel.url else { return }
        downloads.start(connection: connection, entries: entries, into: directory)
    }
}
