import AppKit

/// FileTableView.Coordinator의 드래그 앤 드롭.
/// - 소스: 행을 끌면 파일 프로미스(Finder 다운로드) + 내부 이동 마커를 함께 올린다.
/// - 대상: 내부 마커는 이동, Finder 파일 URL은 업로드로 처리한다.
extension FileTableView.Coordinator: NSDraggingSource, NSDraggingDestination {
    // MARK: - 드래그 소스 (앱 → Finder 다운로드 / 앱 내부 이동)

    /// 소스가 지원하는 드래그 작업. 앱 내부는 .move(이동),
    /// 앱 밖(Finder 등)으로는 .copy(파일 프로미스 다운로드)다.
    func draggingSession(
        _ session: NSDraggingSession,
        sourceOperationMaskFor context: NSDraggingContext
    ) -> NSDragOperation {
        context == .withinApplication ? .move : .copy
    }

    /// 행마다 호출된다. 파일 프로미스 프로바이더를 반환하고 핸들러를 보관한다.
    /// 프로바이더의 delegate가 weak라 세션 동안 핸들러를 붙잡아 둬야 한다.
    func tableView(_ tableView: NSTableView, pasteboardWriterForRow row: Int) -> NSPasteboardWriting? {
        guard row >= 0, row < entries.count else { return nil }
        // makePromiseHandler는 @MainActor 클로저다. 이 콜백은 메인 스레드에서 온다.
        return MainActor.assumeIsolated { () -> NSPasteboardWriting? in
            guard let handler = parent.makePromiseHandler(entries[row]) else { return nil }
            promiseHandlers.append(handler)
            return handler.makeProvider()
        }
    }

    /// 드래그가 시작될 때 모든 dragged row의 키를 JSON으로 마커에 쓴다.
    /// 내부 이동 판별은 이 마커로 하고, Finder 파일 URL과 구분한다.
    func tableView(
        _ tableView: NSTableView,
        draggingSession session: NSDraggingSession,
        willBeginAtPoint screenPoint: NSPoint,
        forRowIndexes rowIndexes: IndexSet
    ) {
        // 완료된 프로미스 핸들러를 정리하고 새 세션을 시작한다.
        // didFinish는 @MainActor 프로퍼티라 assumeIsolated로 읽는다(콜백은 메인 스레드).
        MainActor.assumeIsolated {
            promiseHandlers.removeAll { $0.didFinish }
        }
        let keys = rowIndexes.compactMap { index -> String? in
            guard index >= 0, index < entries.count else { return nil }
            return entries[index].identifier
        }
        guard !keys.isEmpty,
            let data = try? JSONEncoder().encode(keys)
        else { return }
        let item = NSPasteboardItem()
        item.setString(String(decoding: data, as: UTF8.self), forType: PierDragTypes.entryKeys)
        session.draggingPasteboard.writeObjects([item])
    }

    /// 드래그가 끝나면 완료된 프로미스 핸들러만 정리한다.
    /// writePromise가 아직 안 끝난 핸들러는 프로미스가 나중에 쓰일 수 있어 남긴다.
    func tableView(
        _ tableView: NSTableView,
        draggingSession session: NSDraggingSession,
        endedAtPoint screenPoint: NSPoint,
        operation: NSDragOperation
    ) {
        MainActor.assumeIsolated {
            promiseHandlers.removeAll { $0.didFinish }
        }
    }

    // MARK: - 드래그 대상 (Finder → 앱 업로드 / 앱 내부 이동)

    /// 드롭 위치의 부모 폴더 키. 폴더 행 위면 그 폴더, 아니면 현재 폴더.
    private func targetParent(forRow row: Int) -> String {
        guard row >= 0, row < entries.count, entries[row].isDir else {
            return parent.currentParent
        }
        return entries[row].identifier
    }

    func tableView(
        _ tableView: NSTableView,
        validateDrop info: any NSDraggingInfo,
        proposedRow row: Int,
        proposedDropOperation dropOperation: NSTableView.DropOperation
    ) -> Bool {
        // 테이블에의 드롭은 모두 쓰기(업로드/이동)이므로 읽기 전용이면 거부한다.
        guard parent.canWrite else { return false }
        let pasteboard = info.draggingPasteboard
        return pasteboard.availableType(from: [PierDragTypes.entryKeys]) != nil
            || pasteboard.availableType(from: [.fileURL]) != nil
    }

    func tableView(
        _ tableView: NSTableView,
        acceptDrop info: any NSDraggingInfo,
        row: Int,
        dropOperation: NSTableView.DropOperation
    ) -> Bool {
        guard parent.canWrite else { return false }
        let pasteboard = info.draggingPasteboard
        let parentKey = targetParent(forRow: row)
        var handled = false

        // 내부 이동 마커가 있으면 이동으로 처리한다.
        if let json = pasteboard.string(forType: PierDragTypes.entryKeys),
            let data = json.data(using: .utf8),
            let keys = try? JSONDecoder().decode([String].self, from: data),
            !keys.isEmpty
        {
            MainActor.assumeIsolated {
                parent.onMoveKeys(keys, parentKey)
            }
            handled = true
        }

        // Finder 등 외부 파일 URL은 업로드로 처리한다.
        if let urls = pasteboard.readObjects(
            forClasses: [NSURL.self],
            options: [.urlReadingFileURLsOnly: true]
        ) as? [NSURL], !urls.isEmpty {
            let fileURLs = urls.map { $0 as URL }
            MainActor.assumeIsolated {
                parent.onUploadURLs(fileURLs, parentKey)
            }
            handled = true
        }
        return handled
    }
}
