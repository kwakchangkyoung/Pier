import AppKit
import UniformTypeIdentifiers

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
        /// UTType별 시스템 아이콘 캐시. AppKit 콜백은 모두 메인 스레드라 락 없이 쓴다.
        private var iconCache: [String: NSImage] = [:]

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

        /// SwiftUI 상태(entries/selection/정렬)를 테이블에 반영한다.
        func sync(entries newEntries: [PierEntry], selection newSelection: Set<String>, tableView: NSTableView) {
            syncSortDescriptors(tableView)
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

        /// "Presigned URL 복사" 하위 메뉴. representedObject에 만료 초가 들어 있다.
        @objc func copyPresigned(_ sender: NSMenuItem) {
            guard let seconds = sender.representedObject as? UInt64 else { return }
            let selected = selectedEntries()
            guard selected.count == 1, let entry = selected.first, !entry.isDir else { return }
            MainActor.assumeIsolated {
                parent.onPresign(entry, seconds)
            }
        }

        /// "정보 보기". 단일 파일에서만 동작한다.
        @objc func showInfo(_ sender: Any?) {
            let selected = selectedEntries()
            guard selected.count == 1, let entry = selected.first, !entry.isDir else { return }
            MainActor.assumeIsolated {
                parent.onShowInfo(entry)
            }
        }

        /// Return/⌘⌫/스페이스바 키 핸들러를 BrowserTableView에 연결한다.
        func installKeyHandlers(on tableView: BrowserTableView) {
            tableView.onReturnKey = { [weak self] in
                self?.renameSelected(nil)
            }
            tableView.onDeleteKey = { [weak self] in
                self?.deleteSelected(nil)
            }
            tableView.onSpaceKey = { [weak self] in
                guard let self else { return }
                MainActor.assumeIsolated {
                    self.parent.onQuickLook()
                }
            }
        }

        func menuNeedsUpdate(_ menu: NSMenu) {
            // Finder 관례: 선택 밖의 행을 우클릭하면 그 행을 먼저 선택한다.
            if let tableView, tableView.clickedRow >= 0,
                !tableView.selectedRowIndexes.contains(tableView.clickedRow)
            {
                tableView.selectRowIndexes(
                    IndexSet(integer: tableView.clickedRow), byExtendingSelection: false)
            }
            let selected = selectedEntries()
            let hasSelection = !selected.isEmpty
            // presign·정보 보기는 파일 하나가 선택됐을 때만 동작한다.
            let singleFile = selected.count == 1 && selected.first?.isDir == false
            menuItem(.download, in: menu)?.isEnabled = hasSelection
            let presign = menuItem(.presign, in: menu)
            presign?.isEnabled = singleFile
            // 하위 메뉴는 자동 검증을 끄고(autoenablesItems=false) 부모와 같은
            // 상태로 명시 지정한다.
            for sub in presign?.submenu?.items ?? [] {
                sub.isEnabled = singleFile
            }
            menuItem(.info, in: menu)?.isEnabled = singleFile
            menuItem(.rename, in: menu)?.isEnabled = parent.canWrite && selected.count == 1
            menuItem(.delete, in: menu)?.isEnabled = parent.canWrite && hasSelection
        }

        private func menuItem(_ tag: FileTableView.MenuTag, in menu: NSMenu) -> NSMenuItem? {
            menu.items.first { $0.tag == tag.rawValue }
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
                    imageView.image = icon(for: entry)
                }
                // 폴더는 이름을 살짝 굵게 해 파일과 한눈에 구분한다.
                // 셀 재사용을 고려해 매번 폰트를 다시 지정한다.
                textField.font =
                    entry.isDir
                    ? NSFont.systemFont(ofSize: NSFont.systemFontSize, weight: .medium)
                    : NSFont.systemFont(ofSize: NSFont.systemFontSize)
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
            default:
                break
            }
        }

        // MARK: 아이콘

        /// Finder처럼 macOS 시스템 아이콘을 쓴다. 폴더는 파란 폴더,
        /// 파일은 이름 확장자로 구한 UTType의 아이콘(모르면 .data).
        /// 확장자만 보고 판단하므로 네트워크/파일 접근이 없다.
        private func icon(for entry: PierEntry) -> NSImage {
            if entry.isDir {
                return cachedIcon(key: "folder") {
                    NSWorkspace.shared.icon(for: .folder)
                }
            }
            let ext = (entry.name as NSString).pathExtension
            let type = UTType(filenameExtension: ext) ?? .data
            return cachedIcon(key: type.identifier) {
                NSWorkspace.shared.icon(for: type)
            }
        }

        /// 같은 타입의 아이콘을 행마다 새로 만들지 않도록 캐시한다.
        private func cachedIcon(key: String, make: () -> NSImage) -> NSImage {
            if let cached = iconCache[key] { return cached }
            let icon = make()
            icon.size = NSSize(width: 16, height: 16)
            iconCache[key] = icon
            return icon
        }
    }
}
