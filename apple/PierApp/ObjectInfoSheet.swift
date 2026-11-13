import AppKit
import SwiftUI

/// "정보 보기" 우클릭 메뉴의 시트. info(key:) 결과를 표에 보여주고
/// 각 값 옆에 복사 버튼을 둔다.
struct ObjectInfoSheet: View {
    let info: PierObjectInfo
    /// 마지막으로 복사한 필드 제목. 짧게 표시했다 지운다.
    @State private var copiedLabel: String?
    @Environment(\.dismiss) private var dismiss

    private var entry: PierEntry { info.entry }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(entry.name)
                .font(.headline)
                .lineLimit(1)
                .truncationMode(.middle)
            Grid(alignment: .leadingFirstTextBaseline, horizontalSpacing: 12, verticalSpacing: 8) {
                row("키", value: entry.identifier)
                row("크기", value: sizeText)
                row("ETag", value: entry.etag ?? "—")
                row("스토리지 클래스", value: info.storageClass)
                row("최종 수정", value: modifiedText)
            }
            HStack {
                if let copiedLabel {
                    Text("\(copiedLabel) 복사됨")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                Button("닫기") { dismiss() }
                    .keyboardShortcut(.defaultAction)
            }
        }
        .padding(20)
        .frame(width: 440)
    }

    /// "이름 | 값 | 복사 버튼" 한 행.
    private func row(_ label: String, value: String) -> some View {
        GridRow {
            Text(label)
                .foregroundStyle(.secondary)
            Text(value)
                .textSelection(.enabled)
                .lineLimit(2)
            Button {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(value, forType: .string)
                flashCopied(label)
            } label: {
                Image(systemName: "doc.on.doc")
            }
            .buttonStyle(.borderless)
            .help("복사")
        }
    }

    /// 크기를 바이트와 읽기 쉬운 단위로 함께 표시한다.
    private var sizeText: String {
        let human = ByteCountFormatter.string(
            fromByteCount: Int64(entry.size), countStyle: .file)
        return "\(entry.size)바이트 (\(human))"
    }

    private var modifiedText: String {
        guard let ms = entry.lastModifiedMs else { return "—" }
        return Date(timeIntervalSince1970: Double(ms) / 1000)
            .formatted(date: .abbreviated, time: .standard)
    }

    /// "n 복사됨" 표시를 잠깐 보여준다.
    private func flashCopied(_ label: String) {
        copiedLabel = label
        Task { @MainActor in
            try? await Task.sleep(for: .seconds(1.2))
            if copiedLabel == label {
                copiedLabel = nil
            }
        }
    }
}
