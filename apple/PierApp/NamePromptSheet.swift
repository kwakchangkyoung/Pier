import SwiftUI

/// 이름 입력 시트. 새 폴더 만들기와 이름 변경에 공통으로 쓴다.
struct NamePromptSheet: View {
    let title: String
    @Binding var name: String
    /// 검증 오류 메시지. nil이면 정상.
    let error: String?
    let onSubmit: () -> Void
    let onCancel: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(title)
                .font(.headline)
            TextField("이름", text: $name)
                .textFieldStyle(.roundedBorder)
                .onSubmit(onSubmit)
            if let error {
                Text(error)
                    .font(.caption)
                    .foregroundStyle(.red)
            }
            HStack {
                Spacer()
                Button("취소", role: .cancel, action: onCancel)
                Button("확인", action: onSubmit)
                    .keyboardShortcut(.defaultAction)
            }
        }
        .padding(20)
        .frame(width: 340)
    }
}
