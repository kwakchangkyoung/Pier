import SwiftUI

/// "연결 테스트" 버튼의 상태.
enum TestState: Equatable {
    case idle
    case testing
    /// 성공. canWrite는 코어의 쓰기 권한 판정 결과다.
    case succeeded(canWrite: Bool)
    case failed(String)
}

/// 연결 추가·편집·삭제를 위한 설정 창.
struct SettingsView: View {
    let store: ConnectionStore
    let sessions: PierSessions

    @State private var selection: UUID?
    @State private var draft = SavedConnection.emptyDraft()
    /// 편집 중인 연결 id. nil이면 새 연결 작성 중.
    @State private var editingId: UUID?
    @State private var secret = ""
    @State private var testState: TestState = .idle
    @State private var actionError: String?
    @State private var confirmDelete = false

    var body: some View {
        HStack(spacing: 0) {
            connectionList
                .frame(width: 230)
            Divider()
            ConnectionFormView(
                draft: $draft,
                secret: $secret,
                isEditing: editingId != nil,
                testState: testState,
                actionError: actionError,
                canSave: testSucceeded,
                onTest: { testConnection() },
                onSave: { save() },
                onDelete: { confirmDelete = true }
            )
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .frame(minWidth: 680, minHeight: 440)
        .onAppear { loadSelectionIfNeeded() }
        .onChange(of: selection) { _, id in loadDraft(for: id) }
        // 폼이 바뀌면 이전 테스트 결과는 무효다. 다시 테스트해야 저장할 수 있다.
        .onChange(of: draft) { resetFeedback() }
        .onChange(of: secret) { resetFeedback() }
        .confirmationDialog(
            "연결을 삭제할까요?",
            isPresented: $confirmDelete,
            titleVisibility: .visible
        ) {
            Button("삭제", role: .destructive) { deleteSelected() }
            Button("취소", role: .cancel) {}
        } message: {
            Text("키체인에 저장된 시크릿과 캐시도 함께 삭제됩니다.")
        }
    }

    // MARK: - 왼쪽 목록

    private var connectionList: some View {
        VStack(spacing: 0) {
            List(selection: $selection) {
                ForEach(store.connections) { connection in
                    VStack(alignment: .leading, spacing: 1) {
                        Text(connection.displayName)
                        Text(connection.bucket)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                    .tag(connection.id)
                }
            }
            Divider()
            HStack {
                Button {
                    startNewDraft()
                } label: {
                    Label("새 연결", systemImage: "plus")
                }
                Spacer()
            }
            .padding(8)
        }
    }

    // MARK: - draft 관리

    private func loadSelectionIfNeeded() {
        if selection == nil {
            selection = store.connections.first?.id
        }
    }

    private func loadDraft(for id: UUID?) {
        guard let id, let record = store.connection(withId: id) else {
            editingId = nil
            draft = .emptyDraft()
            secret = ""
            resetFeedback()
            return
        }
        editingId = id
        draft = record
        secret = ""
        resetFeedback()
    }

    private func startNewDraft() {
        editingId = nil
        draft = .emptyDraft()
        secret = ""
        selection = nil
        resetFeedback()
    }

    private func resetFeedback() {
        testState = .idle
        actionError = nil
    }

    /// 테스트가 성공했는지 여부. canSave 조건으로 쓴다.
    private var testSucceeded: Bool {
        if case .succeeded = testState { return true }
        return false
    }

    // MARK: - 동작

    /// 임시 캐시 디렉터리로 연결을 열어 testConnection()을 호출한다.
    /// 성공해야 저장 버튼이 활성화된다.
    private func testConnection() {
        actionError = nil
        // 저장·테스트 전에 앞뒤 공백을 제거한 값을 사용한다.
        let cleanDraft = ConnectionValidation.sanitized(draft)
        let trimmedSecret = secret.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let config = cleanDraft.pierConfig else {
            testState = .failed("버킷, 리전, Access Key ID와 엔드포인트를 확인하세요.")
            return
        }
        let secretKey: String
        if !trimmedSecret.isEmpty {
            secretKey = trimmedSecret
        } else if let editingId {
            do {
                guard let stored = try Keychain.load(account: editingId.uuidString) else {
                    testState = .failed("키체인에 저장된 시크릿이 없습니다. Secret Access Key를 입력하세요.")
                    return
                }
                secretKey = stored
            } catch {
                testState = .failed(PierErrorPresenter.message(for: error) ?? "키체인에 접근하지 못했습니다.")
                return
            }
        } else {
            testState = .failed("Secret Access Key를 입력하세요.")
            return
        }

        testState = .testing
        Task {
            do {
                let cacheDir = FileManager.default.temporaryDirectory
                    .appendingPathComponent("pier-connection-test", isDirectory: true)
                try FileManager.default.createDirectory(at: cacheDir, withIntermediateDirectories: true)
                let connection = try PierConnection.open(
                    config: config,
                    secretAccessKey: secretKey,
                    cacheDir: cacheDir.path
                )
                // 목록 조회 후 쓰기 테스트 객체를 쓰고 지워 can_write를 판정한다.
                let result = try await connection.testConnection()
                testState = .succeeded(canWrite: result.canWrite)
            } catch {
                testState = .failed(PierErrorPresenter.message(for: error) ?? "연결 테스트에 실패했습니다.")
            }
        }
    }

    private func save() {
        actionError = nil
        // 앞뒤 공백을 제거한 값으로 저장한다.
        var cleanDraft = ConnectionValidation.sanitized(draft)
        // 테스트에서 확인한 쓰기 권한을 함께 저장한다(읽기 전용 표시용).
        if case .succeeded(let canWrite) = testState {
            cleanDraft.canWrite = canWrite
        }
        let trimmedSecret = secret.trimmingCharacters(in: .whitespacesAndNewlines)
        do {
            if let editingId {
                var record = cleanDraft
                record.id = editingId
                // 시크릿이 비어 있으면 키체인의 기존 값을 유지한다.
                try store.update(record, secret: trimmedSecret.isEmpty ? nil : trimmedSecret)
                sessions.connectionDidChange(record)
            } else {
                try store.add(cleanDraft, secret: trimmedSecret)
            }
            selection = editingId ?? cleanDraft.id
        } catch {
            actionError = PierErrorPresenter.message(for: error) ?? "저장에 실패했습니다."
        }
    }

    private func deleteSelected() {
        guard let editingId, let record = store.connection(withId: editingId) else { return }
        do {
            try store.delete(record)
            sessions.connectionDidDelete(editingId)
            selection = nil
        } catch {
            actionError = PierErrorPresenter.message(for: error) ?? "삭제에 실패했습니다."
        }
    }
}
