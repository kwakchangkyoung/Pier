import Foundation
import Security

/// 로그인 키체인 기반 시크릿 저장소.
/// service = `com.gajago.pier`, account = connection id (UUID 문자열).
enum Keychain {
    static let service = "com.gajago.pier"

    enum KeychainError: Error {
        /// load에서 항목을 찾지 못했을 때.
        case itemNotFound
        /// SecItem* 호출이 errSecSuccess가 아닐 때.
        case failure(operation: String, status: OSStatus)

        /// 사용자에게 보여줄 설명. 최종 문구 선택은 PierErrorPresenter가 한다.
        var detail: String {
            switch self {
            case .itemNotFound:
                return "키체인에 저장된 시크릿이 없습니다."
            case .failure(let operation, let status):
                let reason =
                    SecCopyErrorMessageString(status, nil).map { String($0) }
                    ?? "OSStatus \(status)"
                return "키체인 \(operation) 실패: \(reason)"
            }
        }
    }

    /// 시크릿을 account에 저장한다. 이미 있으면 덮어쓴다.
    static func save(_ secret: String, account: String) throws {
        let baseQuery: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
        // SecItemAdd는 같은 기본 키가 있으면 실패하므로 먼저 지운다.
        SecItemDelete(baseQuery as CFDictionary)

        var addQuery = baseQuery
        addQuery[kSecValueData as String] = Data(secret.utf8)
        addQuery[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlock
        let status = SecItemAdd(addQuery as CFDictionary, nil)
        guard status == errSecSuccess else {
            throw KeychainError.failure(operation: "저장", status: status)
        }
    }

    /// 저장된 시크릿을 읽는다. 항목이 없으면 nil, 그 외 오류는 throw.
    static func load(account: String) throws -> String? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var result: AnyObject?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        if status == errSecItemNotFound {
            return nil
        }
        guard status == errSecSuccess, let data = result as? Data,
            let secret = String(data: data, encoding: .utf8)
        else {
            throw KeychainError.failure(operation: "읽기", status: status)
        }
        return secret
    }

    /// 항목을 지운다. 없으면 아무것도 하지 않는다.
    static func delete(account: String) {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
        SecItemDelete(query as CFDictionary)
    }
}
