import SwiftUI

/// Quick Look 다운로드 진행률을 보여주는 작은 HUD.
/// 미리보기 대상 파일을 받는 동안에만 표시된다.
struct PreviewHUD: View {
    let preview: PreviewController

    var body: some View {
        if preview.isLoading {
            HStack(spacing: 8) {
                ProgressView()
                    .controlSize(.small)
                if preview.total > 0 {
                    let percent = Int(
                        Double(preview.transferred) / Double(preview.total) * 100)
                    Text("미리보기 받는 중… \(percent)%")
                } else {
                    Text("미리보기 받는 중…")
                }
                Button("취소") {
                    preview.cancelDownload()
                }
                .controlSize(.small)
            }
            .font(.callout)
            .padding(.horizontal, 14)
            .padding(.vertical, 8)
            .background(.regularMaterial)
            .clipShape(RoundedRectangle(cornerRadius: 10))
            .padding(.bottom, 16)
        }
    }
}
