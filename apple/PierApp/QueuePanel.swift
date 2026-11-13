import SwiftUI

/// 메인 창 하단의 전송 대기열 패널. 헤더를 눌러 접고 편다.
/// 작업별 표시·제어는 QueueRowView가 담당한다.
struct QueuePanel: View {
    @Bindable var manager: TransferManager

    var body: some View {
        VStack(spacing: 0) {
            Divider()
            header
            if manager.panelExpanded {
                Divider()
                ScrollView {
                    LazyVStack(spacing: 0) {
                        ForEach(manager.items) { item in
                            QueueRowView(item: item, manager: manager)
                        }
                    }
                }
                .frame(maxHeight: 240)
            }
        }
    }

    private var header: some View {
        HStack(spacing: 8) {
            Button {
                manager.panelExpanded.toggle()
            } label: {
                HStack(spacing: 6) {
                    Image(systemName: manager.panelExpanded ? "chevron.down" : "chevron.right")
                        .font(.caption.weight(.semibold))
                    Text("전송")
                        .font(.callout.weight(.medium))
                    Text(summaryText)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
            .buttonStyle(.plain)
            Spacer()
            if manager.items.contains(where: { $0.job.state == .completed }) {
                Button("완료 항목 지우기") {
                    manager.clearCompleted()
                }
                .controlSize(.small)
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
    }

    /// 헤더 요약문: "3개 진행 중 · 12.3 MB/s" 또는 "최근 작업 n개".
    private var summaryText: String {
        if manager.activeCount > 0 {
            var text = "\(manager.activeCount)개 진행 중"
            if manager.aggregateSpeed > 0 {
                text += " · \(Self.formatBytes(manager.aggregateSpeed))/s"
            }
            return text
        }
        return "최근 작업 \(manager.items.count)개"
    }

    static func formatBytes(_ bytes: UInt64) -> String {
        ByteCountFormatter.string(fromByteCount: Int64(bytes), countStyle: .file)
    }

    /// 남은 시간 추정(초). "약 2분", "45초" 형태.
    static func formatRemaining(_ seconds: TimeInterval) -> String {
        if seconds >= 3600 {
            return "약 \(Int(seconds / 3600))시간 \(Int(seconds.truncatingRemainder(dividingBy: 3600) / 60))분"
        }
        if seconds >= 60 {
            return "약 \(Int(seconds / 60))분"
        }
        return "\(max(1, Int(seconds)))초"
    }
}
