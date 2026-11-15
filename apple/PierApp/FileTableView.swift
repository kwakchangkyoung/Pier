import AppKit
import SwiftUI

/// S3 목록을 보여주는 NSTableView 래퍼.
/// 데이터 소스/델리게이트는 FileTableCoordinator, 드래그 앤 드롭은
/// FileTableDragDrop extension이 담당한다.
struct FileTableView: NSViewRepresentable {
    /// 표시 배열(정렬·필터 적용 결과). 행 번호는 이 배열의 인덱스다.
    let entries: [PierEntry]
    /// 현재 정렬. 열 머리글 화살표 표시에 쓴다.
    let sort: EntrySort
    /// 열 머리글 클릭으로 정렬이 바뀌면 호출. 실제 정렬은 모델이 한다.
    let onSortChange: @MainActor (EntrySort) -> Void
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
    /// 우클릭 "Presigned URL 복사": 항목과 만료 초를 넘긴다.
    let onPresign: @MainActor (PierEntry, UInt64) -> Void
    /// 우클릭 "정보 보기".
    let onShowInfo: @MainActor (PierEntry) -> Void
    /// 스페이스바 Quick Look 요청.
    let onQuickLook: @MainActor () -> Void
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
        // 열 머리글의 정렬 화살표를 현재 정렬 상태로 표시한다.
        tableView.sortDescriptors = [
            NSSortDescriptor(key: sort.key.rawValue, ascending: sort.ascending)
        ]

        tableView.dataSource = context.coordinator
        tableView.delegate = context.coordinator
        tableView.target = context.coordinator
        tableView.doubleAction = #selector(Coordinator.handleDoubleClick(_:))

        // 내부 이동은 .move, 외부(Finder) 드롭은 모든 연산을 허용한다.
        tableView.setDraggingSourceOperationMask(.move, forLocal: true)
        tableView.setDraggingSourceOperationMask(.every, forLocal: false)
        // 내부 이동 마커(com.gajago.pier.entry-keys)와 Finder 파일 URL을 받는다.
        tableView.registerForDraggedTypes([PierDragTypes.entryKeys, .fileURL])

        let menu = NSMenu()
        let coordinator = context.coordinator
        // 활성화는 menuNeedsUpdate가 수동으로 정한다. 자동 검증(autoenablesItems)은
        // "Presigned URL 복사"처럼 action이 없는 하위 메뉴 부모를 항상 비활성으로 만든다.
        menu.autoenablesItems = false
        menu.addItem(
            menuItem("다운로드", #selector(Coordinator.downloadSelected(_:)), target: coordinator, tag: .download))
        menu.addItem(presignMenuItem(target: coordinator))
        menu.addItem(
            menuItem("정보 보기", #selector(Coordinator.showInfo(_:)), target: coordinator, tag: .info))
        menu.addItem(.separator())
        menu.addItem(
            menuItem("이름 변경", #selector(Coordinator.renameSelected(_:)), target: coordinator, tag: .rename))
        menu.addItem(
            menuItem("삭제", #selector(Coordinator.deleteSelected(_:)), target: coordinator, tag: .delete))
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
    }

    private static func makeColumns() -> [NSTableColumn] {
        func column(_ id: ColumnID, _ title: String, _ width: CGFloat) -> NSTableColumn {
            let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier(id.rawValue))
            column.title = title
            column.width = width
            column.minWidth = width * 0.5
            // 열 머리글 클릭 정렬용 프로토타입. 키는 ColumnID = EntrySortKey raw 값.
            column.sortDescriptorPrototype = NSSortDescriptor(key: id.rawValue, ascending: true)
            return column
        }
        return [
            column(.name, "이름", 320),
            column(.size, "크기", 90),
            column(.modified, "수정 시각", 170),
        ]
    }

    /// 우클릭 메뉴 항목 구분 태그. menuNeedsUpdate에서 인덱스 대신 태그로 찾는다.
    enum MenuTag: Int {
        case download = 1
        case presign
        case info
        case rename
        case delete
    }

    private func menuItem(
        _ title: String, _ action: Selector, target: Coordinator, tag: MenuTag
    ) -> NSMenuItem {
        let item = NSMenuItem(title: title, action: action, keyEquivalent: "")
        item.target = target
        item.tag = tag.rawValue
        return item
    }

    /// "Presigned URL 복사" 하위 메뉴. 만료 선택지는 1시간/1일/7일(최대)이다.
    private func presignMenuItem(target: Coordinator) -> NSMenuItem {
        let item = NSMenuItem(title: "Presigned URL 복사", action: nil, keyEquivalent: "")
        item.tag = MenuTag.presign.rawValue
        let submenu = NSMenu()
        // 하위 항목의 활성화도 부모 menuNeedsUpdate 결과에 맡긴다.
        submenu.autoenablesItems = false
        for (title, seconds) in [("1시간", UInt64(3600)), ("1일", UInt64(86400)), ("7일", UInt64(604800))] {
            let sub = NSMenuItem(
                title: title,
                action: #selector(Coordinator.copyPresigned(_:)),
                keyEquivalent: ""
            )
            sub.target = target
            sub.representedObject = seconds
            submenu.addItem(sub)
        }
        item.submenu = submenu
        return item
    }
}

/// 열 머리글 정렬 클릭 연결. 실제 정렬 계산은 모델이 표시 배열에 적용한다.
extension FileTableView.Coordinator {
    @objc func tableView(_ tableView: NSTableView, sortDescriptorsDidChange oldDescriptors: [NSSortDescriptor]) {
        guard let descriptor = tableView.sortDescriptors.first,
            let key = descriptor.key.flatMap({ EntrySortKey(rawValue: $0) })
        else { return }
        MainActor.assumeIsolated {
            parent.onSortChange(EntrySort(key: key, ascending: descriptor.ascending))
        }
    }

    /// 모델의 정렬 상태가 바뀌었으면 열 머리글 화살표를 맞춘다. sync에서 호출한다.
    func syncSortDescriptors(_ tableView: NSTableView) {
        let current = tableView.sortDescriptors.first
        if current?.key != parent.sort.key.rawValue || current?.ascending != parent.sort.ascending {
            tableView.sortDescriptors = [
                NSSortDescriptor(key: parent.sort.key.rawValue, ascending: parent.sort.ascending)
            ]
        }
    }
}
