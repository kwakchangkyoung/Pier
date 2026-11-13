import AppKit
import SwiftUI

/// 대기열 작업 한 줄: 방향 아이콘, 이름, 진행률·속도·남은 시간, 상태별 제어 버튼.
struct QueueRowView: View {
    let item: QueueItem
    let manager: TransferManager

    private var job: PierJob { item.job }

    var body: some View {
        HStack(spacing: 8) {
            icon
            VStack(alignment: .leading, spacing: 2) {
                Text(job.name)
                    .lineLimit(1)
                    .truncationMode(.middle)
                statusText
            }
            Spacer(minLength: 8)
            progressView
            actionButtons
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
        .help(job.remoteKey)
    }

    // MARK: - 아이콘/상태 텍스트

    private var icon: some View {
        Image(systemName: iconName)
            .foregroundStyle(iconColor)
    }

    private var iconName: String {
        if job.isFolder { return "folder" }
        return job.direction == .download ? "arrow.down.circle" : "arrow.up.circle"
    }

    private var iconColor: Color {
        switch job.state {
        case .completed: return .green
        case .failed: return .red
        case .cancelled: return .secondary
        case .queued, .running, .paused: return .secondary
        }
    }

    @ViewBuilder
    private var statusText: some View {
        switch job.state {
        case .queued:
            caption("대기 중")
        case .paused:
            caption("일시정지 · \(percentText)")
        case .running:
            if job.isFolder && job.total == 0 {
                // 폴더 작업은 하위 목록을 받은 뒤에야 total이 정해진다.
                caption(job.direction == .download ? "목록 확인 중…" : "파일 확인 중…")
            } else {
                HStack(spacing: 4) {
                    Text(progressText)
                    if job.bytesPerSec > 0 {
                        Text("· \(QueuePanel.formatBytes(job.bytesPerSec))/s")
                        if let remaining = remainingTime {
                            Text("· 남은 시간 \(remaining)")
                        }
                    }
                }
                .font(.caption)
                .foregroundStyle(.secondary)
            }
        case .completed:
            caption(completedText)
        case .failed:
            Text(errorText)
                .font(.caption)
                .foregroundStyle(.red)
                .lineLimit(2)
        case .cancelled:
            caption("취소됨")
        }
    }

    /// 실행 중 진행 문구. 폴더는 파일 수를 함께 보여준다.
    private var progressText: String {
        var text =
            job.total > 0
            ? "\(QueuePanel.formatBytes(job.transferred)) / \(QueuePanel.formatBytes(job.total))"
            : QueuePanel.formatBytes(job.transferred)
        if job.isFolder && job.filesTotal > 0 {
            text += " · \(job.filesDone)/\(job.filesTotal)개 파일"
        }
        return text
    }

    private var percentText: String {
        guard job.total > 0 else { return "0%" }
        return "\(Int(Double(job.transferred) / Double(job.total) * 100))%"
    }

    /// 남은 바이트 / 현재 속도로 추정한 남은 시간.
    private var remainingTime: String? {
        guard job.bytesPerSec > 0, job.total > job.transferred else { return nil }
        let seconds = Double(job.total - job.transferred) / Double(job.bytesPerSec)
        return QueuePanel.formatRemaining(seconds)
    }

    private var completedText: String {
        guard job.isFolder else { return "완료" }
        let count = job.filesDone
        return count > 0 ? "완료 · \(count)개 파일" : "완료"
    }

    private var errorText: String {
        guard let error = job.error else { return "실패" }
        return PierErrorPresenter.message(forJobError: error)
    }

    private func caption(_ text: String) -> some View {
        Text(text)
            .font(.caption)
            .foregroundStyle(.secondary)
    }

    // MARK: - 진행률/버튼

    @ViewBuilder
    private var progressView: some View {
        if job.state == .running || job.state == .paused {
            if job.total > 0 {
                ProgressView(value: Double(job.transferred), total: Double(job.total))
                    .progressViewStyle(.linear)
                    .frame(width: 120)
            } else {
                ProgressView()
                    .controlSize(.small)
            }
        }
    }

    @ViewBuilder
    private var actionButtons: some View {
        switch job.state {
        case .queued:
            Button("취소") { manager.cancel(item) }
                .controlSize(.small)
        case .running:
            Button("일시정지") { manager.pause(item) }
                .controlSize(.small)
            Button("취소") { manager.cancel(item) }
                .controlSize(.small)
        case .paused:
            Button("재개") { manager.resume(item) }
                .controlSize(.small)
            Button("취소") { manager.cancel(item) }
                .controlSize(.small)
        case .completed:
            // 업로드는 로컬 소스 경로라 Finder에서 보기 대신 닫기만 둔다.
            if job.direction == .download {
                Button("Finder에서 보기") { reveal() }
                    .controlSize(.small)
            }
            dismissButton
        case .failed, .cancelled:
            Button("재시도") { manager.retry(item) }
                .controlSize(.small)
            dismissButton
        }
    }

    private var dismissButton: some View {
        Button {
            manager.remove(item)
        } label: {
            Image(systemName: "xmark")
        }
        .controlSize(.small)
        .buttonStyle(.borderless)
        .help("목록에서 제거")
    }

    private func reveal() {
        NSWorkspace.shared.activateFileViewerSelecting(
            [URL(fileURLWithPath: job.localPath)])
    }
}
