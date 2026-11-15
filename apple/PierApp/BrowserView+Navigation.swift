import SwiftUI

/// BrowserView의 경로 표시줄(breadcrumb)과 목록 밖 드롭 처리.
extension BrowserView {
    var breadcrumbBar: some View {
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
                    // 경로 구간에 놓으면 그 폴더로 이동/업로드한다.
                    .onDrop(of: RemoteDropDelegate.types, delegate: dropDelegate(for: model.path[index]))
                }
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 7)
        }
    }

    func segmentTitle(at index: Int) -> String {
        let folder = model.path[index]
        guard !folder.isEmpty else { return model.title }
        let trimmed = folder.hasSuffix("/") ? String(folder.dropLast()) : folder
        return trimmed.split(separator: "/").last.map(String.init) ?? folder
    }

    /// breadcrumb 구간/빈 영역용 SwiftUI 드롭 델리게이트.
    func dropDelegate(for parent: String) -> RemoteDropDelegate {
        RemoteDropDelegate(parentKey: parent) { payload, target in
            handleDrop(payload: payload, parent: target)
        }
    }

    /// 외부 파일은 업로드, 내부 항목 키는 이동으로 처리한다.
    func handleDrop(payload: DropPayload, parent: String) {
        guard model.canWrite, model.connection != nil else { return }
        if !payload.keys.isEmpty {
            Task {
                do {
                    try await model.moveEntries(keys: payload.keys, toParent: parent)
                } catch {
                    actionError = PierErrorPresenter.message(for: error) ?? "이동에 실패했습니다."
                }
            }
        }
        if !payload.urls.isEmpty {
            startUploads(payload.urls, parent: parent)
        }
    }
}
