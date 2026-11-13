import AppKit
import SwiftUI

/// 메인 창 하단의 전송 진행 바. 업로드/다운로드를 방향 아이콘으로 구분해 보여준다.
/// Phase 4의 대기열 패널 전까지의 단순 표시.
struct TransferBar: View {
    let manager: TransferManager

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

    private func row(_ item: TransferItem) -> some View {
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
    private func icon(for item: TransferItem) -> some View {
        // 폴더 작업은 폴더 아이콘을 상태 색으로 표시한다.
        if item.isFolder {
            Image(systemName: "folder")
                .foregroundStyle(iconColor(for: item.state))
        } else {
            Image(systemName: fileIconName(for: item))
                .foregroundStyle(iconColor(for: item.state))
        }
    }

    private func fileIconName(for item: TransferItem) -> String {
        switch item.state {
        case .active:
            return item.direction == .download ? "arrow.down.circle" : "arrow.up.circle"
        case .completed: return "checkmark.circle.fill"
        case .failed: return "xmark.circle.fill"
        }
    }

    /// 폴더 작업 완료 문구. 업로드는 업로드/제외/건너뜀 수를 함께 보여준다.
    private func folderCompletedText(_ item: TransferItem) -> String {
        guard item.direction == .upload else {
            return "완료 · \(item.fileCount)개 파일"
        }
        var text = "완료 · \(item.fileCount)개 업로드"
        if item.ignoredCount > 0 {
            text += " · \(item.ignoredCount)개 제외"
        }
        if item.skippedCount > 0 {
            text += " · \(item.skippedCount)개 건너뜀"
        }
        return text
    }

    private func iconColor(for state: TransferItem.State) -> Color {
        switch state {
        case .active: return .secondary
        case .completed: return .green
        case .failed: return .red
        }
    }

    @ViewBuilder
    private func statusText(_ item: TransferItem) -> some View {
        switch item.state {
        case .active:
            // 폴더 작업은 하위 목록을 받은 뒤에야 total이 정해지므로
            // total==0 동안은 불확정 상태로 표시한다.
            if item.isFolder && item.total == 0 {
                Text(item.direction == .download ? "목록 확인 중…" : "파일 확인 중…")
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
                Text(folderCompletedText(item))
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
    private func progressView(_ item: TransferItem) -> some View {
        switch item.state {
        case .active:
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
    private func actionButtons(_ item: TransferItem) -> some View {
        switch item.state {
        case .active:
            Button("취소") { manager.cancel(item.id) }
                .controlSize(.small)
        case .completed:
            // 업로드는 로컬 소스 경로라 Finder에서 보기 대신 닫기만 둔다.
            if item.direction == .download {
                Button("Finder에서 보기") { reveal(item) }
                    .controlSize(.small)
            }
            dismissButton(item)
        case .failed:
            dismissButton(item)
        }
    }

    private func dismissButton(_ item: TransferItem) -> some View {
        Button {
            manager.remove(item.id)
        } label: {
            Image(systemName: "xmark")
        }
        .controlSize(.small)
        .buttonStyle(.borderless)
        .help("닫기")
    }

    private func reveal(_ item: TransferItem) {
        guard let path = item.destinationPath else { return }
        NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: path)])
    }

    private static func formatBytes(_ bytes: UInt64) -> String {
        ByteCountFormatter.string(fromByteCount: Int64(bytes), countStyle: .file)
    }
}
