import AppKit
import SwiftUI

/// 메인 창 하단의 다운로드 진행 바. Phase 4의 대기열 패널 전까지의 단순 표시.
struct DownloadBar: View {
    let manager: DownloadManager

    var body: some View {
        VStack(spacing: 0) {
            Divider()
            ScrollView {
                VStack(alignment: .leading, spacing: 8) {
                    ForEach(manager.items) { item in
                        row(item)
                    }
                }
                .padding(10)
            }
            .frame(maxHeight: 180)
        }
    }

    private func row(_ item: DownloadItem) -> some View {
        HStack(spacing: 8) {
            icon(for: item)
            VStack(alignment: .leading, spacing: 2) {
                Text(item.name)
                    .lineLimit(1)
                    .truncationMode(.middle)
                statusText(item)
            }
            Spacer(minLength: 8)
            progressView(item)
            actionButtons(item)
        }
    }

    @ViewBuilder
    private func icon(for item: DownloadItem) -> some View {
        // 폴더 작업은 폴더 아이콘을 상태 색으로 표시한다.
        if item.isFolder {
            Image(systemName: "folder")
                .foregroundStyle(iconColor(for: item.state))
        } else {
            Image(systemName: fileIconName(for: item.state))
                .foregroundStyle(iconColor(for: item.state))
        }
    }

    private func fileIconName(for state: DownloadItem.State) -> String {
        switch state {
        case .downloading: return "arrow.down.circle"
        case .completed: return "checkmark.circle.fill"
        case .failed: return "xmark.circle.fill"
        }
    }

    private func iconColor(for state: DownloadItem.State) -> Color {
        switch state {
        case .downloading: return .secondary
        case .completed: return .green
        case .failed: return .red
        }
    }

    @ViewBuilder
    private func statusText(_ item: DownloadItem) -> some View {
        switch item.state {
        case .downloading:
            // 폴더 작업은 하위 목록을 받은 뒤에야 total이 정해지므로
            // total==0 동안은 불확정 상태로 표시한다.
            if item.isFolder && item.total == 0 {
                Text("목록 확인 중…")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else {
                HStack(spacing: 4) {
                    if item.total > 0 {
                        Text("\(Self.formatBytes(item.transferred)) / \(Self.formatBytes(item.total))")
                    } else {
                        Text(Self.formatBytes(item.transferred))
                    }
                    if item.speed > 0 {
                        Text("· \(Self.formatBytes(UInt64(item.speed)))/s")
                    }
                }
                .font(.caption)
                .foregroundStyle(.secondary)
            }
        case .completed:
            if item.isFolder {
                Text("완료 · \(item.fileCount)개 파일")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else {
                Text("완료")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        case .failed(let message):
            Text(message)
                .font(.caption)
                .foregroundStyle(.red)
                .lineLimit(2)
        }
    }

    @ViewBuilder
    private func progressView(_ item: DownloadItem) -> some View {
        switch item.state {
        case .downloading:
            if item.total > 0 {
                ProgressView(value: Double(item.transferred), total: Double(item.total))
                    .progressViewStyle(.linear)
                    .frame(width: 120)
            } else {
                ProgressView()
                    .controlSize(.small)
            }
        case .completed, .failed:
            EmptyView()
        }
    }

    @ViewBuilder
    private func actionButtons(_ item: DownloadItem) -> some View {
        switch item.state {
        case .downloading:
            Button("취소") { manager.cancel(item.id) }
                .controlSize(.small)
        case .completed:
            Button("Finder에서 보기") { reveal(item) }
                .controlSize(.small)
            dismissButton(item)
        case .failed:
            dismissButton(item)
        }
    }

    private func dismissButton(_ item: DownloadItem) -> some View {
        Button {
            manager.remove(item.id)
        } label: {
            Image(systemName: "xmark")
        }
        .controlSize(.small)
        .buttonStyle(.borderless)
        .help("닫기")
    }

    private func reveal(_ item: DownloadItem) {
        guard let path = item.destinationPath else { return }
        NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: path)])
    }

    private static func formatBytes(_ bytes: UInt64) -> String {
        ByteCountFormatter.string(fromByteCount: Int64(bytes), countStyle: .file)
    }
}
