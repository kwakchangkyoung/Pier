import AppKit
import SwiftUI

/// S3 목록을 보여주는 NSTableView 래퍼.
/// 데이터 소스/델리게이트는 FileTableCoordinator, 드래그 앤 드롭은
/// FileTableDragDrop extension이 담당한다.
struct FileTableView: NSViewRepresentable {
    let entries: [PierEntry]
    @Binding var selection: Set<String>
    /// 행 위가 아닌 곳에 드롭했을 때의 대상 부모 폴더 키(현재 폴더).
    let currentParent: String
    /// 읽기 전용 연결이면 이름 변경/삭제를 비활성화한다.
    let canWrite: Bool
    /// 폴더 더블클릭 시 호출.
    let onOpenFolder: @MainActor (PierEntry) -> Void
    /// 우클릭 메뉴에서 선택 항목을 넘길 때 호출.
    let onDownload: @MainActor ([PierEntry]) -> Void
    /// 우클릭/Return으로 단일 항목 이름 변경을 요청할 때 호출.
    let onRename: @MainActor (PierEntry) -> Void
    /// 우클릭/⌘⌫으로 선택 항목 삭제를 요청할 때 호출.
    let onDelete: @MainActor ([PierEntry]) -> Void
    /// 앱 내부 이동 드롭: 항목 키들을 toParent 폴더로 옮긴다.
    let onMoveKeys: @MainActor ([String], String) -> Void
    /// Finder 파일 드롭: 로컬 URL들을 toParent 폴더로 업로드한다.
    let onUploadURLs: @MainActor ([URL], String) -> Void
    /// 행을 Finder로 끌 때 파일 프로미스 핸들러를 만든다. nil이면 드래그 불가.
    let makePromiseHandler: @MainActor (PierEntry) -> PromiseDownloadHandler?

    func makeCoordinator() -> Coordinator {
        Coordinator(parent: self)
    }

    func makeNSView(context: Context) -> NSScrollView {
        let scrollView = NSScrollView()
        scrollView.hasVerticalScroller = true
        scrollView.autohidesScrollers = true
        scrollView.drawsBackground = false

        let tableView = BrowserTableView()
        tableView.style = .fullWidth
        tableView.usesAutomaticRowHeights = true
        tableView.allowsMultipleSelection = true
        tableView.allowsEmptySelection = true
        tableView.columnAutoresizingStyle = .firstColumnOnlyAutoresizingStyle
        tableView.backgroundColor = .clear
        tableView.headerView = NSTableHeaderView()

        for column in Self.makeColumns() {
            tableView.addTableColumn(column)
        }

        tableView.dataSource = context.coordinator
        tableView.delegate = context.coordinator
        tableView.target = context.coordinator
        tableView.doubleAction = #selector(Coordinator.handleDoubleClick(_:))

        // 내부 이동은 .move, 외부(Finder) 드롭은 모든 연산을 허용한다.
        tableView.setDraggingSourceOperationMask(.move, forLocal: true)
        tableView.setDraggingSourceOperationMask(.every, forLocal: false)
        // 내부 이동 마커(com.yourname.pier.entry-keys)와 Finder 파일 URL을 받는다.
        tableView.registerForDraggedTypes([PierDragTypes.entryKeys, .fileURL])

        let menu = NSMenu()
        menu.addItem(menuItem("다운로드", #selector(Coordinator.downloadSelected(_:)), target: context.coordinator))
        menu.addItem(.separator())
        menu.addItem(menuItem("이름 변경", #selector(Coordinator.renameSelected(_:)), target: context.coordinator))
        menu.addItem(menuItem("삭제", #selector(Coordinator.deleteSelected(_:)), target: context.coordinator))
        menu.delegate = context.coordinator
        tableView.menu = menu

        context.coordinator.tableView = tableView
        context.coordinator.installKeyHandlers(on: tableView)
        scrollView.documentView = tableView
        return scrollView
    }

    func updateNSView(_ scrollView: NSScrollView, context: Context) {
        context.coordinator.parent = self
        guard let tableView = scrollView.documentView as? NSTableView else { return }
        context.coordinator.sync(entries: entries, selection: selection, tableView: tableView)
    }

    // MARK: - 컬럼/메뉴

    /// FileTableCoordinator의 셀 구성에서도 쓰므로 internal이다.
    enum ColumnID: String {
        case name
        case size
        case modified
        case storageClass
    }

    private static func makeColumns() -> [NSTableColumn] {
        func column(_ id: ColumnID, _ title: String, _ width: CGFloat) -> NSTableColumn {
            let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier(id.rawValue))
            column.title = title
            column.width = width
            column.minWidth = width * 0.5
            return column
        }
        return [
            column(.name, "이름", 320),
            column(.size, "크기", 90),
            column(.modified, "수정 시각", 170),
            column(.storageClass, "클래스", 120),
        ]
    }

    private func menuItem(_ title: String, _ action: Selector, target: Coordinator) -> NSMenuItem {
        let item = NSMenuItem(title: title, action: action, keyEquivalent: "")
        item.target = target
        return item
    }
}
