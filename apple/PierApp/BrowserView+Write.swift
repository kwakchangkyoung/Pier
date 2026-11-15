import AppKit
import SwiftUI

/// BrowserView의 툴바와 쓰기/다운로드 동작.
extension BrowserView {
    @ToolbarContentBuilder
    var toolbarItems: some ToolbarContent {
        ToolbarItemGroup(placement: .navigation) {
            Button {
                model.goBack()
            } label: {
                Label("뒤로", systemImage: "chevron.left")
            }
            .disabled(!model.canGoBack)
            Button {
                model.goForward()
            } label: {
                Label("앞으로", systemImage: "chevron.right")
            }
            .disabled(!model.canGoForward)
        }
        ToolbarItemGroup(placement: .primaryAction) {
            if !model.canWrite {
                Text("읽기 전용")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            if model.phase == .loading || model.isRefreshing {
                ProgressView()
                    .controlSize(.small)
            }
            Button {
                model.reload()
            } label: {
                Label("새로고침", systemImage: "arrow.clockwise")
            }
            .keyboardShortcut("r")
            Button {
                beginCreateFolder()
            } label: {
                Label("새 폴더", systemImage: "folder.badge.plus")
            }
            .keyboardShortcut("n", modifiers: [.command, .shift])
            .disabled(!model.canWrite)
            Button {
                chooseUploadItems()
            } label: {
                Label("업로드", systemImage: "arrow.up.to.line")
            }
            .disabled(!model.canWrite)
            Button {
                downloadSelection()
            } label: {
                Label("다운로드", systemImage: "arrow.down.to.line")
            }
            .disabled(!canDownload)
            Button {
                transfers.panelExpanded.toggle()
            } label: {
                Label("전송", systemImage: "arrow.up.arrow.down")
            }
            .keyboardShortcut("l", modifiers: [.option, .command])
        }
    }

    /// 선택 항목을 표시 배열 기준으로 푼다. 필터로 숨겨진 항목은 동작 대상에서 빠진다.
    var selectedEntries: [PierEntry] {
        model.displayedEntries.filter { selection.contains($0.identifier) }
    }

    /// 선택 항목이 하나라도 있으면 다운로드할 수 있다(파일·폴더 모두 지원).
    var canDownload: Bool {
        !selectedEntries.isEmpty
    }

    func downloadSelection() {
        downloadEntries(selectedEntries)
    }

    /// 선택한 파일·폴더를 한 번의 패널로 저장 위치를 고른 뒤 TransferManager에 넘긴다.
    func downloadEntries(_ entries: [PierEntry]) {
        guard !entries.isEmpty, let connection = model.connection else { return }
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        panel.prompt = "다운로드"
        panel.message = "항목을 저장할 폴더를 선택하세요."
        guard panel.runModal() == .OK, let directory = panel.url else { return }
        transfers.startDownloads(connection: connection, entries: entries, into: directory)
    }

    /// 행을 Finder로 끌 때 앱→Finder 다운로드용 프로미스 핸들러를 만든다.
    func makePromiseHandler(for entry: PierEntry) -> PromiseDownloadHandler? {
        guard let connection = model.connection else { return nil }
        return PromiseDownloadHandler(
            entry: entry,
            connection: connection,
            connectionId: model.connectionId,
            transfers: transfers
        )
    }

    /// 툴바 업로드: 파일·폴더 다중 선택 후 현재 폴더에 올린다.
    func chooseUploadItems() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = true
        panel.allowsMultipleSelection = true
        panel.prompt = "업로드"
        panel.message = "업로드할 파일이나 폴더를 선택하세요."
        guard panel.runModal() == .OK, !panel.urls.isEmpty else { return }
        startUploads(panel.urls, parent: model.currentFolder)
    }

    /// 로컬 URL들을 parent 폴더에 업로드한다. 충돌 확인·모드 결정은 UploadController가 한다.
    func startUploads(_ urls: [URL], parent: String) {
        guard let connection = model.connection else { return }
        uploads.start(
            urls: urls,
            parent: parent,
            connection: connection,
            connectionId: model.connectionId,
            transfers: transfers
        ) { entry in
            model.noteRemoteChange(entry: entry)
        }
    }

    // MARK: - Quick Look / presign / 정보 보기

    /// 스페이스바 미리보기. 선택 중 첫 번째 파일을 대상으로 한다(폴더는 무시).
    func quickLookSelection() {
        guard let entry = selectedEntries.first(where: { !$0.isDir }),
            let connection = model.connection
        else { return }
        preview.toggle(entry: entry, connection: connection)
    }

    /// presigned URL을 만들어 클립보드에 복사한다. 서명이 든 URL이므로 로그에 남기지 않는다.
    func copyPresignedURL(entry: PierEntry, expiresSecs: UInt64) {
        guard let connection = model.connection else { return }
        Task {
            do {
                let url = try await connection.presign(
                    key: entry.identifier, expiresSecs: expiresSecs)
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(url, forType: .string)
                showToast("복사됨")
            } catch {
                actionError =
                    PierErrorPresenter.message(for: error) ?? "URL을 만들지 못했습니다."
            }
        }
    }

    /// 객체 정보를 받아 시트로 연다.
    func showObjectInfo(entry: PierEntry) {
        guard let connection = model.connection else { return }
        Task {
            do {
                objectInfo = ObjectInfoSheetData(
                    info: try await connection.info(key: entry.identifier))
            } catch {
                actionError =
                    PierErrorPresenter.message(for: error) ?? "정보를 가져오지 못했습니다."
            }
        }
    }

    /// 짧은 토스트를 띄운다. 같은 메시지가 다시 뜨면 타이머만 연장한다.
    func showToast(_ message: String) {
        toastMessage = message
        toastGeneration += 1
        let generation = toastGeneration
        Task { @MainActor in
            try? await Task.sleep(for: .seconds(1.5))
            if toastGeneration == generation {
                toastMessage = nil
            }
        }
    }

    // MARK: - 이름 입력 시트

    func beginCreateFolder() {
        nameDraft = ""
        nameError = nil
        nameAction = .createFolder
    }

    func beginRename(_ entry: PierEntry) {
        nameDraft = entry.name
        nameError = nil
        nameAction = .rename(entry)
    }

    func nameSheetTitle(for action: NameAction) -> String {
        switch action {
        case .createFolder: return "새 폴더"
        case .rename: return "이름 변경"
        }
    }

    func submitName(_ action: NameAction) {
        let name = nameDraft.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !name.isEmpty, !name.contains("/") else {
            nameError = "이름은 비워 둘 수 없고 '/'를 포함할 수 없습니다."
            return
        }
        nameAction = nil
        switch action {
        case .createFolder:
            Task {
                do {
                    _ = try await model.createFolder(named: name)
                } catch {
                    actionError = PierErrorPresenter.message(for: error) ?? "폴더를 만들지 못했습니다."
                }
            }
        case .rename(let entry):
            let newParent = RemoteKey.parent(of: entry.identifier)
            Task {
                do {
                    _ = try await model.renameEntry(
                        key: entry.identifier,
                        newParent: newParent,
                        newName: name
                    )
                } catch {
                    actionError = PierErrorPresenter.message(for: error) ?? "이름을 변경하지 못했습니다."
                }
            }
        }
    }

    // MARK: - 삭제

    func deleteMessage(for entries: [PierEntry]) -> String {
        var message = "\(entries.count)개 항목을 영구 삭제합니다. 휴지통으로 가지 않습니다."
        if entries.contains(where: { $0.isDir }) {
            message += " 폴더 안 모든 객체가 삭제됩니다."
        }
        return message
    }

    func confirmDelete(_ entries: [PierEntry]) {
        pendingDelete = nil
        let keys = entries.map(\.identifier)
        Task {
            do {
                try await model.deleteEntries(keys: keys)
            } catch {
                actionError = PierErrorPresenter.message(for: error) ?? "삭제하지 못했습니다."
            }
        }
    }
}
