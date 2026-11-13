import SwiftUI

/// 업로드 이름 충돌 시트. 대치/둘 다 유지/건너뛰기와 "모두에 적용"을 고른다.
struct UploadConflictSheet: View {
    let conflict: TransferConflict
    /// 선택 확정 시 호출. choice가 nil이면 취소로 본다.
    let onResolve: (TransferConflictChoice?, Bool) -> Void

    @State private var applyToAll = false

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("'\(conflict.name)' 이름이 이미 사용 중입니다.")
                .font(.headline)
            Text(
                conflict.isDir
                    ? "폴더를 업로드하면 안의 파일 전체가 이 모드로 처리됩니다."
                    : "업로드할 파일을 어떻게 처리할지 선택하세요."
            )
            .font(.callout)
            .foregroundStyle(.secondary)
            Toggle("모두에 적용", isOn: $applyToAll)
            HStack {
                Spacer()
                Button("건너뛰기") { onResolve(.skip, applyToAll) }
                Button("둘 다 유지") { onResolve(.keepBoth, applyToAll) }
                Button("대치") { onResolve(.replace, applyToAll) }
                    .keyboardShortcut(.defaultAction)
            }
        }
        .padding(20)
        .frame(width: 400)
    }
}
