import Foundation

/// SPEC 4.5: 코어/앱 에러를 사용자 메시지로 변환하는 단일 지점.
/// nil을 반환하면 사용자에게 보여줄 오류가 아니다(취소 등).
enum PierErrorPresenter {
    static func message(for error: Error) -> String? {
        if let pierError = error as? PierError {
            return pierErrorMessage(pierError)
        }
        if let keychainError = error as? Keychain.KeychainError {
            return keychainMessage(keychainError)
        }
        if let sessionError = error as? PierSessionError {
            return sessionMessage(sessionError)
        }
        if error is StoreError {
            return "연결 정보를 찾을 수 없습니다. 목록을 새로고침하세요."
        }
        return "알 수 없는 오류가 발생했습니다."
    }

    /// 로그용 분류값. 에러 본문은 시크릿이 섞일 수 있어 남기지 않는다.
    static func categoryName(of error: Error) -> String {
        guard let pierError = error as? PierError else { return "unknown" }
        switch pierError {
        case .Authentication: return "authentication"
        case .Network: return "network"
        case .NotFound: return "notFound"
        case .Cancelled: return "cancelled"
        case .Other: return "other"
        }
    }

    private static func pierErrorMessage(_ error: PierError) -> String? {
        switch error {
        case .Authentication:
            return "인증에 실패했습니다. Access Key ID와 Secret Access Key를 확인하세요."
        case .Network:
            return "네트워크에 연결할 수 없습니다. 네트워크 연결을 확인하고 다시 시도하세요."
        case .NotFound:
            return "항목을 찾을 수 없습니다. 이미 삭제되었을 수 있으니 목록을 새로고침하세요."
        case .Cancelled:
            return nil
        case .Other(let message):
            return message.isEmpty ? "오류가 발생했습니다." : "오류가 발생했습니다: \(message)"
        }
    }

    private static func keychainMessage(_ error: Keychain.KeychainError) -> String {
        switch error {
        case .itemNotFound:
            return "키체인에 저장된 시크릿이 없습니다. 설정에서 연결을 다시 저장하세요."
        case .failure:
            return "키체인에 접근하지 못했습니다. \(error.detail)"
        }
    }

    private static func sessionMessage(_ error: PierSessionError) -> String {
        switch error {
        case .unavailable:
            return "세션을 사용할 수 없습니다."
        case .invalidConfiguration:
            return "연결 설정이 올바르지 않습니다. 설정을 확인하세요."
        }
    }
}
