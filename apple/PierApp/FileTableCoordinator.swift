import AppKit

/// FileTableView.Coordinator의 핵심: 데이터 소스/델리게이트, 선택 동기화,
/// 셀 구성, 컨텍스트 메뉴 동작, 키보드 단축키 연결.
/// 드래그 앤 드롭은 FileTableDragDrop.swift의 extension이 담당한다.
extension FileTableView {
    /// NSTableView 데이터 소스/델리게이트. AppKit 콜백은 모두 메인 스레드에서 온다.
    final class Coordinator: NSObject, NSTableViewDataSource, NSTableViewDelegate, NSMenuDelegate {
        var parent: FileTableView
        weak var tableView: NSTableView?
        /// 드래그 세션 동안 살아 있어야 하는 프로미스 핸들러들.
        var promiseHandlers: [PromiseDownloadHandler] = []
        /// FileTableDragDrop.swift(extension)에서도 읽으므로 internal이다.
        var entries: [PierEntry] = []
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

        // MARK: 더블클릭/메뉴/키

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

        @objc func renameSelected(_ sender: Any?) {
            let selected = selectedEntries()
            guard selected.count == 1, let entry = selected.first else { return }
            MainActor.assumeIsolated {
                parent.onRename(entry)
            }
        }

        @objc func deleteSelected(_ sender: Any?) {
            let entries = selectedEntries()
            guard !entries.isEmpty else { return }
            MainActor.assumeIsolated {
                parent.onDelete(entries)
            }
        }

        /// Return/⌘⌫ 키 핸들러를 BrowserTableView에 연결한다.
        func installKeyHandlers(on tableView: BrowserTableView) {
            tableView.onReturnKey = { [weak self] in
                self?.renameSelected(nil)
            }
            tableView.onDeleteKey = { [weak self] in
                self?.deleteSelected(nil)
            }
        }

        func menuNeedsUpdate(_ menu: NSMenu) {
            let selected = selectedEntries()
            let hasSelection = !selected.isEmpty
            // 메뉴 순서: 다운로드 / 구분선 / 이름 변경 / 삭제
            menu.items[safe: 0]?.isEnabled = hasSelection
            menu.items[safe: 2]?.isEnabled = parent.canWrite && selected.count == 1
            menu.items[safe: 3]?.isEnabled = parent.canWrite && hasSelection
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
            if column.identifier.rawValue == FileTableView.ColumnID.name.rawValue {
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
            case FileTableView.ColumnID.name.rawValue:
                if let imageView = cell.viewWithTag(1) as? NSImageView {
                    let symbol = entry.isDir ? "folder" : "doc"
                    imageView.image = NSImage(systemSymbolName: symbol, accessibilityDescription: nil)
                }
                textField.stringValue = entry.name
                cell.toolTip = entry.identifier
            case FileTableView.ColumnID.size.rawValue:
                textField.stringValue =
                    entry.isDir
                    ? "—"
                    : ByteCountFormatter.string(fromByteCount: Int64(entry.size), countStyle: .file)
                cell.toolTip = nil
            case FileTableView.ColumnID.modified.rawValue:
                if let ms = entry.lastModifiedMs {
                    let date = Date(timeIntervalSince1970: Double(ms) / 1000)
                    textField.stringValue = date.formatted(date: .abbreviated, time: .shortened)
                } else {
                    textField.stringValue = "—"
                }
                cell.toolTip = nil
            case FileTableView.ColumnID.storageClass.rawValue:
                textField.stringValue = entry.storageClass ?? "—"
                cell.toolTip = nil
            default:
                break
            }
        }
    }
}

extension Array where Element == NSMenuItem {
    fileprivate subscript(safe index: Int) -> NSMenuItem? {
        indices.contains(index) ? self[index] : nil
    }
}
