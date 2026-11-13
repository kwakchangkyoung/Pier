import SwiftUI

/// 연결 폼의 "고급" 섹션: 전송 청크 크기와 동시성.
/// nil이면 프리셋 기본값을 쓰고, 토글을 끄면 프리셋 값으로 채워 수정할 수 있다.
struct TransferSettingsSection: View {
    @Binding var draft: SavedConnection
    let preset: PierPreset?

    var body: some View {
        Section("고급") {
            Toggle("프리셋 기본값 사용", isOn: usesPresetDefaults)
            Stepper(
                "청크 크기: \(chunkSize)MB",
                value: chunkSizeBinding,
                in: 5...512
            )
            .disabled(usesPresetDefaults.wrappedValue)
            Stepper(
                "동시 전송 수: \(concurrency)",
                value: concurrencyBinding,
                in: 1...32
            )
            .disabled(usesPresetDefaults.wrappedValue)
        }
    }

    /// 표시용 현재값. 저장값이 nil이면 프리셋 기본값을 보여준다.
    private var chunkSize: Int {
        Int(draft.chunkSizeMb ?? preset?.chunkSizeMb ?? 8)
    }

    private var concurrency: Int {
        Int(draft.concurrency ?? preset?.concurrency ?? 8)
    }

    /// 둘 다 nil이면 프리셋 기본값 사용. 끄면 프리셋 값으로 채워 커스텀 모드로 전환한다.
    private var usesPresetDefaults: Binding<Bool> {
        Binding(
            get: { draft.chunkSizeMb == nil && draft.concurrency == nil },
            set: { useDefaults in
                if useDefaults {
                    draft.chunkSizeMb = nil
                    draft.concurrency = nil
                } else {
                    draft.chunkSizeMb = draft.chunkSizeMb ?? preset?.chunkSizeMb ?? 8
                    draft.concurrency = draft.concurrency ?? preset?.concurrency ?? 8
                }
            }
        )
    }

    private var chunkSizeBinding: Binding<Int> {
        Binding(
            get: { chunkSize },
            set: { draft.chunkSizeMb = UInt32(clamping: $0) }
        )
    }

    private var concurrencyBinding: Binding<Int> {
        Binding(
            get: { concurrency },
            set: { draft.concurrency = UInt32(clamping: $0) }
        )
    }
}
