import SwiftUI

/// 연결 추가·편집 폼. draft/secret 상태는 SettingsView가 소유한다.
struct ConnectionFormView: View {
    @Binding var draft: SavedConnection
    @Binding var secret: String
    let isEditing: Bool
    let testState: TestState
    let actionError: String?
    let canSave: Bool
    let onTest: () -> Void
    let onSave: () -> Void
    let onDelete: () -> Void

    /// 필드별 검증 오류. draft/secret이 바뀔 때마다 다시 계산된다.
    private var errors: [ConnectionValidation.Field: String] {
        ConnectionValidation.errors(draft: draft, secret: secret, isEditing: isEditing)
    }

    var body: some View {
        Form {
            Section("연결") {
                VStack(alignment: .leading, spacing: 2) {
                    TextField("표시 이름", text: $draft.displayName)
                    fieldError(.displayName)
                }
                Picker("서비스", selection: $draft.provider) {
                    ForEach(SavedConnection.ProviderKind.allCases, id: \.self) { provider in
                        Text(provider.label).tag(provider)
                    }
                }
                if draft.provider != .aws {
                    VStack(alignment: .leading, spacing: 2) {
                        TextField("엔드포인트 (예: http://127.0.0.1:9000)", text: endpointBinding)
                        fieldError(.endpoint)
                    }
                    Toggle("Path-style 사용", isOn: $draft.pathStyle)
                }
                VStack(alignment: .leading, spacing: 2) {
                    TextField("리전", text: $draft.region)
                    fieldError(.region)
                }
                VStack(alignment: .leading, spacing: 2) {
                    TextField("버킷 이름 (예: my-bucket)", text: $draft.bucket)
                    bucketGuide
                }
                TextField("접두사 (선택)", text: prefixBinding)
            }
            Section("자격 증명") {
                VStack(alignment: .leading, spacing: 2) {
                    TextField("Access Key ID", text: $draft.accessKeyId)
                    fieldError(.accessKeyId)
                }
                VStack(alignment: .leading, spacing: 2) {
                    SecureField("Secret Access Key", text: $secret)
                    fieldError(.secret)
                }
                if isEditing {
                    Text("비워 두면 기존 키체인 값을 유지합니다.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
            Section {
                HStack(spacing: 8) {
                    Button("연결 테스트") { onTest() }
                        .disabled(testState == .testing || !errors.isEmpty)
                    if testState == .testing {
                        ProgressView()
                            .controlSize(.small)
                    }
                    testStatusLabel
                    Spacer()
                    if isEditing {
                        Button("삭제", role: .destructive) { onDelete() }
                    }
                    Button("저장") { onSave() }
                        .keyboardShortcut(.defaultAction)
                        .disabled(!canSave)
                }
                if let actionError {
                    Text(actionError)
                        .font(.callout)
                        .foregroundStyle(.red)
                }
            }
        }
        .formStyle(.grouped)
        .onChange(of: draft.provider) {
            draft.applyProviderDefaults()
        }
    }

    /// 필드 아래에 표시할 짧은 오류 안내.
    @ViewBuilder
    private func fieldError(_ field: ConnectionValidation.Field) -> some View {
        if let message = errors[field] {
            Text(message)
                .font(.caption)
                .foregroundStyle(.red)
        }
    }

    /// 버킷 필드 아래 안내. 오류가 있으면 오류를, 없으면 입력 요령을 보여준다.
    @ViewBuilder
    private var bucketGuide: some View {
        if let message = errors[.bucket] {
            Text(message)
                .font(.caption)
                .foregroundStyle(.red)
        } else {
            Text("버킷 이름 하나만 입력하세요. 하위 경로는 '접두사'에 입력합니다.")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
    }

    @ViewBuilder
    private var testStatusLabel: some View {
        switch testState {
        case .idle:
            EmptyView()
        case .testing:
            EmptyView()
        case .succeeded(let canWrite):
            Label(
                canWrite ? "테스트 성공" : "테스트 성공 — 읽기 전용 연결",
                systemImage: canWrite ? "checkmark.circle.fill" : "eye.fill"
            )
            .font(.caption)
            .foregroundStyle(canWrite ? .green : .orange)
        case .failed(let message):
            Text(message)
                .font(.caption)
                .foregroundStyle(.red)
                .lineLimit(2)
        }
    }

    /// 빈 문자열과 nil을 오가는 엔드포인트 바인딩.
    private var endpointBinding: Binding<String> {
        Binding(
            get: { draft.endpoint ?? "" },
            set: { draft.endpoint = $0.isEmpty ? nil : $0 }
        )
    }

    /// 빈 문자열과 nil을 오가는 접두사 바인딩.
    private var prefixBinding: Binding<String> {
        Binding(
            get: { draft.prefix ?? "" },
            set: { draft.prefix = $0.isEmpty ? nil : $0 }
        )
    }
}
