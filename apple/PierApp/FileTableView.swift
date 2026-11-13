import AppKit
import SwiftUI

/// S3 목록을 보여주는 NSTableView 래퍼.
/// 다중 선택, 폴더 더블클릭 진입, 우클릭 메뉴를 지원한다.
struct FileTableView: NSViewRepresentable {
    let entries: [PierEntry]
    @Binding var selection: Set<String>
    /// 폴더 더블클릭 시 호출.
    let onOpenFolder: @MainActor (PierEntry) -> Void
    /// 우클릭 메뉴에서 선택 항목을 넘길 때 호출.
    let onDownload: @MainActor ([PierEntry]) -> Void

    func makeCoordinator() -> Coordinator {
        Coordinator(parent: self)
    }

    func makeNSView(context: Context) -> NSScrollView {
        let scrollView = NSScrollView()
        scrollView.hasVerticalScroller = true
        scrollView.autohidesScrollers = true
        scrollView.drawsBackground = false

        let tableView = NSTableView()
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

        let menu = NSMenu()
        let downloadItem = NSMenuItem(
            title: "다운로드",
            action: #selector(Coordinator.downloadSelected(_:)),
            keyEquivalent: ""
        )
        downloadItem.target = context.coordinator
        menu.addItem(downloadItem)
        menu.delegate = context.coordinator
        tableView.menu = menu

        scrollView.documentView = tableView
        context.coordinator.tableView = tableView
        return scrollView
    }

    func updateNSView(_ scrollView: NSScrollView, context: Context) {
        context.coordinator.parent = self
        guard let tableView = scrollView.documentView as? NSTableView else { return }
        context.coordinator.sync(entries: entries, selection: selection, tableView: tableView)
    }

    // MARK: - 컬럼

    private enum ColumnID: String {
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

    // MARK: - Coordinator

    /// NSTableView 데이터 소스/델리게이트. AppKit 콜백은 모두 메인 스레드에서 온다.
    final class Coordinator: NSObject, NSTableViewDataSource, NSTableViewDelegate, NSMenuDelegate {
        var parent: FileTableView
        weak var tableView: NSTableView?
        private var entries: [PierEntry] = []
        private var lastSelection: Set<String> = []
        /// reloadData/프로그램적 선택 변경 중에는 selectionDidChange를 무시한다.
        private var isSyncingSelection = false

        init(parent: FileTableView) {
            self.parent = parent
        }

        // MARK: 데이터 소스

        func numberOfRows(in tableView: NSTableView) -> Int {
            entries.count
        }

        func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
            guard let tableColumn, row < entries.count else { return nil }
            let entry = entries[row]
            let cellID = NSUserInterfaceItemIdentifier("cell-\(tableColumn.identifier.rawValue)")
            var cell = tableView.makeView(withIdentifier: cellID, owner: self) as? NSTableCellView
            if cell == nil {
                cell = makeCell(for: tableColumn)
                cell?.identifier = cellID
            }
            configure(cell, column: tableColumn.identifier.rawValue, entry: entry)
            return cell
        }

        // MARK: 선택 동기화

        /// SwiftUI 상태(entries/selection)를 테이블에 반영한다.
        func sync(entries newEntries: [PierEntry], selection newSelection: Set<String>, tableView: NSTableView) {
            let entriesChanged = newEntries != entries
            entries = newEntries
            if entriesChanged {
                isSyncingSelection = true
                tableView.reloadData()
                applySelection(newSelection, to: tableView)
                isSyncingSelection = false
            } else if newSelection != lastSelection {
                isSyncingSelection = true
                applySelection(newSelection, to: tableView)
                isSyncingSelection = false
            }
            lastSelection = newSelection
        }

        private func applySelection(_ ids: Set<String>, to tableView: NSTableView) {
            var rows = IndexSet()
            for (index, entry) in entries.enumerated() where ids.contains(entry.identifier) {
                rows.insert(index)
            }
            tableView.selectRowIndexes(rows, byExtendingSelection: false)
        }

        func tableViewSelectionDidChange(_ notification: Notification) {
            guard !isSyncingSelection, let tableView = notification.object as? NSTableView else { return }
            let ids = Set(
                tableView.selectedRowIndexes.compactMap { row -> String? in
                    guard row >= 0, row < entries.count else { return nil }
                    return entries[row].identifier
                })
            lastSelection = ids
            if ids != parent.selection {
                parent.selection = ids
            }
        }

        // MARK: 더블클릭/컨텍스트 메뉴

        @objc func handleDoubleClick(_ sender: NSTableView) {
            let row = sender.clickedRow
            guard row >= 0, row < entries.count else { return }
            let entry = entries[row]
            guard entry.isDir else { return }
            MainActor.assumeIsolated {
                parent.onOpenFolder(entry)
            }
        }

        @objc func downloadSelected(_ sender: Any?) {
            let entries = selectedEntries()
            guard !entries.isEmpty else { return }
            MainActor.assumeIsolated {
                parent.onDownload(entries)
            }
        }

        func menuNeedsUpdate(_ menu: NSMenu) {
            menu.items.first?.isEnabled = !selectedEntries().isEmpty
        }

        private func selectedEntries() -> [PierEntry] {
            guard let tableView else { return [] }
            return tableView.selectedRowIndexes.compactMap { row in
                guard row >= 0, row < entries.count else { return nil }
                return entries[row]
            }
        }

        // MARK: 셀 구성

        private func makeCell(for column: NSTableColumn) -> NSTableCellView {
            let cell = NSTableCellView()
            if column.identifier.rawValue == ColumnID.name.rawValue {
                let imageView = NSImageView()
                imageView.translatesAutoresizingMaskIntoConstraints = false
                imageView.imageScaling = .scaleProportionallyDown
                imageView.setContentHuggingPriority(.required, for: .horizontal)
                let textField = NSTextField(labelWithString: "")
                textField.translatesAutoresizingMaskIntoConstraints = false
                textField.lineBreakMode = .byTruncatingMiddle
                cell.addSubview(imageView)
                cell.addSubview(textField)
                NSLayoutConstraint.activate([
                    imageView.leadingAnchor.constraint(equalTo: cell.leadingAnchor, constant: 6),
                    imageView.centerYAnchor.constraint(equalTo: cell.centerYAnchor),
                    imageView.widthAnchor.constraint(equalToConstant: 16),
                    imageView.heightAnchor.constraint(equalToConstant: 16),
                    textField.leadingAnchor.constraint(equalTo: imageView.trailingAnchor, constant: 6),
                    textField.trailingAnchor.constraint(equalTo: cell.trailingAnchor, constant: -6),
                    textField.centerYAnchor.constraint(equalTo: cell.centerYAnchor),
                ])
                imageView.tag = 1
                textField.tag = 2
            } else {
                let textField = NSTextField(labelWithString: "")
                textField.translatesAutoresizingMaskIntoConstraints = false
                textField.lineBreakMode = .byTruncatingTail
                cell.addSubview(textField)
                NSLayoutConstraint.activate([
                    textField.leadingAnchor.constraint(equalTo: cell.leadingAnchor, constant: 6),
                    textField.trailingAnchor.constraint(equalTo: cell.trailingAnchor, constant: -6),
                    textField.centerYAnchor.constraint(equalTo: cell.centerYAnchor),
                ])
                textField.tag = 2
            }
            return cell
        }

        private func configure(_ cell: NSTableCellView?, column: String, entry: PierEntry) {
            guard let cell, let textField = cell.viewWithTag(2) as? NSTextField else { return }
            switch column {
            case ColumnID.name.rawValue:
                if let imageView = cell.viewWithTag(1) as? NSImageView {
                    let symbol = entry.isDir ? "folder" : "doc"
                    imageView.image = NSImage(systemSymbolName: symbol, accessibilityDescription: nil)
                }
                textField.stringValue = entry.name
                cell.toolTip = entry.identifier
            case ColumnID.size.rawValue:
                textField.stringValue =
                    entry.isDir
                    ? "—"
                    : ByteCountFormatter.string(fromByteCount: Int64(entry.size), countStyle: .file)
                cell.toolTip = nil
            case ColumnID.modified.rawValue:
                if let ms = entry.lastModifiedMs {
                    let date = Date(timeIntervalSince1970: Double(ms) / 1000)
                    textField.stringValue = date.formatted(date: .abbreviated, time: .shortened)
                } else {
                    textField.stringValue = "—"
                }
                cell.toolTip = nil
            case ColumnID.storageClass.rawValue:
                textField.stringValue = entry.storageClass ?? "—"
                cell.toolTip = nil
            default:
                break
            }
        }
    }
}
