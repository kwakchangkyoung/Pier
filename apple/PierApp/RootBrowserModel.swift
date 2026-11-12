import Foundation
import Observation

/// Table 행 식별용. PierEntry는 생성 코드라 직접 수정하지 않고 여기서 적합성을 부여한다.
extension PierEntry: Identifiable {
    public var id: String { identifier }
}

/// 루트 목록 화면의 상태를 관리하는 뷰 모델.
@Observable
final class RootBrowserModel {
    enum Phase: Equatable {
        case idle
        case loading
        case ready
        case failed(String)
    }

    private(set) var entries: [PierEntry] = []
    private(set) var phase: Phase = .idle
    private var connection: PierConnection?

    /// 루트 목록을 불러온다. 이미 로딩 중이면 무시한다.
    func load() async {
        guard phase != .loading else { return }
        phase = .loading
        do {
            let conn: PierConnection
            if let existing = connection {
                conn = existing
            } else {
                let opened = try PierConnection.open(
                    config: DevConnection.config, secretAccessKey: DevConnection.secretAccessKey)
                connection = opened
                conn = opened
            }
            let result = try await conn.listRoot()
            entries = result
            phase = .ready
            Log.app.info("listRoot 성공: \(result.count)개 항목")
        } catch {
            phase = .failed(Self.userMessage(for: error))
            Log.app.error("listRoot 실패: \(Self.categoryName(of: error))")
        }
    }

    /// SPEC 4.5: 코어 에러 범주를 사용자 메시지로 변환한다.
    private static func userMessage(for error: Error) -> String {
        guard let pierError = error as? PierError else {
            return "알 수 없는 오류가 발생했습니다."
        }
        switch pierError {
        case .Authentication:
            return "인증에 실패했습니다. Access Key ID와 Secret Access Key를 확인하세요."
        case .Network:
            return "네트워크에 연결할 수 없습니다. MinIO 실행 여부와 네트워크 연결을 확인하고 다시 시도하세요."
        case .NotFound:
            return "버킷 또는 객체를 찾을 수 없습니다. 버킷 이름을 확인하세요."
        case .Other(let message):
            return "오류가 발생했습니다: \(message)"
        }
    }

    /// 로그용 오류 범주 이름. 시크릿이 포함되지 않는 분류값만 남긴다.
    private static func categoryName(of error: Error) -> String {
        guard let pierError = error as? PierError else { return "unknown" }
        switch pierError {
        case .Authentication: return "authentication"
        case .Network: return "network"
        case .NotFound: return "notFound"
        case .Other: return "other"
        }
    }
}
