import AppKit
import SwiftUI

/// 이름 입력 시트의 대상. 새 폴더 만들기와 이름 변경을 구분한다.
enum NameAction: Identifiable {
    case createFolder
    case rename(PierEntry)

    var id: String {
        switch self {
        case .createFolder: return "create-folder"
        case .rename(let entry): return "rename-\(entry.identifier)"
        }
    }
}

/// 선택한 연결의 파일 브라우저 본문.
/// 쓰기 동작(업로드/이름/삭제)은 BrowserView+Write.swift의 extension이 담당한다.
struct BrowserView: View {
    let model: BrowserModel
    let transfers: TransferManager
    let uploads: UploadController
    /// extension 파일에서도 접근하므로 internal이다.
    @State var selection = Set<String>()
    @State var nameAction: NameAction?
    @State var nameDraft = ""
    @State var nameError: String?
    @State var pendingDelete: [PierEntry]?
    @State var actionError: String?

    var body: some View {
        VStack(spacing: 0) {
            breadcrumbBar
            Divider()
            if let message = errorBannerMessage {
                bannerView(message)
                Divider()
            }
            content
            if !transfers.items.isEmpty {
                TransferBar(manager: transfers)
            }
        }
        .navigationTitle(model.title)
        .toolbar { toolbarItems }
        .sheet(isPresented: conflictSheetPresented) {
            if let conflict = uploads.activeConflict {
                UploadConflictSheet(conflict: conflict) { choice, applyToAll in
                    uploads.resolveActiveConflict(choice: choice, applyToAll: applyToAll)
                }
            }
        }
        .sheet(item: $nameAction) { action in
            NamePromptSheet(
                title: nameSheetTitle(for: action),
                name: $nameDraft,
                error: nameError,
                onSubmit: { submitName(action) },
                onCancel: { nameAction = nil }
            )
        }
        .confirmationDialog(
            "삭제",
            isPresented: Binding(
                get: { pendingDelete != nil },
                set: { if !$0 { pendingDelete = nil } }
            ),
            titleVisibility: .visible,
            presenting: pendingDelete
        ) { entries in
            Button("삭제", role: .destructive) { confirmDelete(entries) }
            Button("취소", role: .cancel) { pendingDelete = nil }
        } message: { entries in
            Text(deleteMessage(for: entries))
        }
        .alert(
            "작업을 완료하지 못했습니다",
            isPresented: Binding(
                get: { actionError != nil },
                set: { if !$0 { actionError = nil } }
            )
        ) {
            Button("확인") { actionError = nil }
        } message: {
            Text(actionError ?? "")
        }
    }

    /// 충돌 시트 표시 여부. 닫힐 때 선택 없이(nil) resolves해 건너뛰기로 처리한다.
    private var conflictSheetPresented: Binding<Bool> {
        Binding(
            get: { uploads.activeConflict != nil },
            set: { if !$0 { uploads.resolveActiveConflict(choice: nil, applyToAll: false) } }
        )
    }

    // MARK: - breadcrumb

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

    // MARK: - 내용

    /// 목록이 있는데 phase가 failed면 상단에 오류 배너를 띄운다(갱신 실패 등).
    var errorBannerMessage: String? {
        guard case .failed(let message) = model.phase, !model.entries.isEmpty else { return nil }
        return message
    }

    func bannerView(_ message: String) -> some View {
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
    var content: some View {
        if model.entries.isEmpty {
            emptyState
                // 빈 폴더/로딩 영역에 놓인 드롭도 현재 폴더 대상으로 처리한다.
                .onDrop(of: RemoteDropDelegate.types, delegate: dropDelegate(for: model.currentFolder))
        } else {
            FileTableView(
                entries: model.entries,
                selection: $selection,
                currentParent: model.currentFolder,
                canWrite: model.canWrite,
                onOpenFolder: { entry in
                    model.navigate(toFolder: entry.identifier)
                },
                onDownload: { entries in
                    downloadEntries(entries)
                },
                onRename: { entry in
                    beginRename(entry)
                },
                onDelete: { entries in
                    pendingDelete = entries
                },
                onMoveKeys: { keys, parent in
                    handleDrop(payload: DropPayload(keys: keys), parent: parent)
                },
                onUploadURLs: { urls, parent in
                    startUploads(urls, parent: parent)
                },
                makePromiseHandler: { entry in
                    makePromiseHandler(for: entry)
                }
            )
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    @ViewBuilder
    var emptyState: some View {
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
}
