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

/// 정보 보기 시트에 넘길 객체 정보. sheet(item:)용 래퍼.
struct ObjectInfoSheetData: Identifiable {
    let id = UUID()
    let info: PierObjectInfo
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
    /// Quick Look 미리보기 상태. 스페이스바로 토글한다.
    @State var preview = PreviewController()
    /// 정보 보기 시트에 표시할 객체 정보. nil이면 시트가 닫힌다.
    @State var objectInfo: ObjectInfoSheetData?
    /// 짧은 안내 토스트("복사됨" 등).
    @State var toastMessage: String?
    /// 토스트 자동 닫기 세대 번호. 새 토스트가 이전 타이머를 무효화한다.
    @State var toastGeneration = 0

    var body: some View {
        VStack(spacing: 0) {
            breadcrumbBar
            Divider()
            if let message = errorBannerMessage {
                bannerView(message)
                Divider()
            }
            content
            if model.isFiltering {
                Divider()
                filterStatusBar
            }
            if !transfers.items.isEmpty {
                QueuePanel(manager: transfers)
            }
        }
        .navigationTitle(model.title)
        .toolbar { toolbarItems }
        // 툴바 검색 필드. ⌘F로 포커스되고, 입력은 현재 폴더 이름 필터다.
        .searchable(
            text: Binding(
                get: { model.searchQuery },
                set: { model.searchQuery = $0 }),
            placement: .toolbar,
            prompt: "현재 폴더에서 이름 검색"
        )
        .onChange(of: selection) { _, _ in
            preview.selectionDidChange(selected: selectedEntries)
        }
        .onAppear {
            preview.onError = { message in actionError = message }
        }
        .overlay(alignment: .top) {
            toastOverlay
        }
        .overlay(alignment: .bottom) {
            PreviewHUD(preview: preview)
        }
        .sheet(item: $objectInfo) { data in
            ObjectInfoSheet(info: data.info)
        }
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

    /// "복사됨" 같은 짧은 안내를 화면 상단에 띄운다.
    @ViewBuilder
    private var toastOverlay: some View {
        if let toastMessage {
            Text(toastMessage)
                .font(.callout)
                .padding(.horizontal, 12)
                .padding(.vertical, 6)
                .background(.regularMaterial)
                .clipShape(Capsule())
                .padding(.top, 8)
                .transition(.opacity)
        }
    }

    /// 충돌 시트 표시 여부. 닫힐 때 선택 없이(nil) resolves해 건너뛰기로 처리한다.
    private var conflictSheetPresented: Binding<Bool> {
        Binding(
            get: { uploads.activeConflict != nil },
            set: { if !$0 { uploads.resolveActiveConflict(choice: nil, applyToAll: false) } }
        )
    }

    // MARK: - 내용

    /// 필터 결과 요약 표시줄. 필터가 켜져 있을 때만 목록 아래에 보인다.
    var filterStatusBar: some View {
        HStack {
            Text("전체 \(model.entries.count)개 중 \(model.displayedEntries.count)개")
                .font(.caption)
                .foregroundStyle(.secondary)
            Spacer()
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 4)
        .background(.bar)
    }

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
        } else if model.displayedEntries.isEmpty {
            // 목록은 있는데 이름 필터에 걸린 경우.
            ContentUnavailableView(
                "‘\(model.trimmedQuery)’와 일치하는 항목이 없습니다",
                systemImage: "magnifyingglass"
            )
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .onDrop(of: RemoteDropDelegate.types, delegate: dropDelegate(for: model.currentFolder))
        } else {
            FileTableView(
                entries: model.displayedEntries,
                sort: model.sort,
                onSortChange: { sort in
                    model.applySort(sort)
                },
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
                onPresign: { entry, seconds in
                    copyPresignedURL(entry: entry, expiresSecs: seconds)
                },
                onShowInfo: { entry in
                    showObjectInfo(entry: entry)
                },
                onQuickLook: {
                    quickLookSelection()
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
