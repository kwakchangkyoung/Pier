import SwiftUI

/// 연결 추가·편집 폼. draft/secret 상태는 SettingsView가 소유한다.
/// 서비스 선택은 코어의 presets()를 기준으로 한다.
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

    /// 현재 선택된 프리셋. 알 수 없는 provider id면 nil.
    private var preset: PierPreset? {
        draft.preset
    }

    var body: some View {
        Form {
            Section("연결") {
                VStack(alignment: .leading, spacing: 2) {
                    TextField("표시 이름", text: $draft.displayName)
                    fieldError(.displayName)
                }
                Picker("서비스", selection: $draft.provider) {
                    ForEach(presets(), id: \.id) { preset in
                        Text(preset.displayName).tag(preset.id)
                    }
                    // 저장된 값이 프리셋에 없는 구형 데이터면 그대로 보여준다.
                    if preset == nil {
                        Text(draft.providerLabel).tag(draft.provider)
                    }
                }
                serviceFields
                regionField
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
            TransferSettingsSection(draft: $draft, preset: preset)
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
            draft.applyPresetDefaults()
        }
    }

    // MARK: - 프리셋별 필드

    /// 계정 ID(엔드포인트 자동 계산) 또는 직접 엔드포인트 입력 필드.
    @ViewBuilder
    private var serviceFields: some View {
        if let preset, preset.needsAccountId {
            VStack(alignment: .leading, spacing: 2) {
                TextField("계정 ID", text: accountIdBinding)
                fieldError(.accountId)
            }
            LabeledContent("엔드포인트") {
                Text(computedEndpoint ?? "계정 ID를 입력하면 자동으로 정해집니다")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
        }
        if preset?.needsEndpoint ?? true {
            VStack(alignment: .leading, spacing: 2) {
                TextField("엔드포인트 (예: http://127.0.0.1:9000)", text: endpointBinding)
                fieldError(.endpoint)
            }
            Toggle("Path-style 사용", isOn: $draft.pathStyle)
        }
    }

    /// 프리셋의 리전 선택지에 따라 Picker 또는 텍스트 필드를 보여준다.
    /// 리전이 하나뿐이면(예: R2 "auto") 필드를 숨긴다.
    @ViewBuilder
    private var regionField: some View {
        if let preset, !preset.regions.isEmpty {
            if preset.regions.count > 1 {
                Picker("리전", selection: $draft.region) {
                    ForEach(preset.regions, id: \.self) { region in
                        Text(region).tag(region)
                    }
                    if !preset.regions.contains(draft.region) {
                        Text(draft.region).tag(draft.region)
                    }
                }
            }
        } else {
            VStack(alignment: .leading, spacing: 2) {
                TextField("리전", text: $draft.region)
                fieldError(.region)
            }
        }
    }

    /// 계정 ID·리전으로 계산된 엔드포인트(읽기 전용 표시용).
    private var computedEndpoint: String? {
        guard let preset else { return nil }
        return presetEndpoint(
            presetId: preset.id,
            accountId: Self.nonEmpty(draft.accountId),
            region: draft.region
        )
    }

    // MARK: - 공통 부분

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

    /// 빈 문자열과 nil을 오가는 계정 ID 바인딩.
    private var accountIdBinding: Binding<String> {
        Binding(
            get: { draft.accountId ?? "" },
            set: { draft.accountId = $0.isEmpty ? nil : $0 }
        )
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

    private static func nonEmpty(_ value: String?) -> String? {
        guard let value else { return nil }
        let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? nil : trimmed
    }
}
