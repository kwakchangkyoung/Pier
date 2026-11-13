import Foundation

/// 연결 추가·편집 폼의 입력값 검증. 순수 함수 — UI 상태 없이 draft/secret만 검사한다.
/// 저장·테스트 전에 `sanitized`로 앞뒤 공백을 제거한 값을 사용한다.
enum ConnectionValidation {
    /// 오류를 표시할 필드.
    enum Field: Hashable {
        case displayName
        case endpoint
        case accountId
        case region
        case bucket
        case accessKeyId
        case secret
    }

    /// S3 버킷 이름에 허용되는 문자(ASCII 소문자·숫자·'.'·'-').
    private static let bucketAllowedCharacters = Set("abcdefghijklmnopqrstuvwxyz0123456789.-")

    /// 앞뒤 공백을 제거한 draft 사본. 저장·테스트 전에 사용한다.
    static func sanitized(_ draft: SavedConnection) -> SavedConnection {
        var copy = draft
        copy.displayName = draft.displayName.trimmingCharacters(in: .whitespacesAndNewlines)
        copy.region = draft.region.trimmingCharacters(in: .whitespacesAndNewlines)
        copy.bucket = draft.bucket.trimmingCharacters(in: .whitespacesAndNewlines)
        copy.accessKeyId = draft.accessKeyId.trimmingCharacters(in: .whitespacesAndNewlines)
        copy.accountId = trimOptional(draft.accountId)
        copy.endpoint = trimOptional(draft.endpoint)
        copy.prefix = trimOptional(draft.prefix)
        return copy
    }

    /// 필드별 오류 메시지. 모든 값이 유효하면 빈 딕셔너리.
    /// draft는 내부에서 공백을 제거한 뒤 검사하므로 "  bucket  "도 유효로 판정된다.
    static func errors(draft: SavedConnection, secret: String, isEditing: Bool) -> [Field: String] {
        let draft = sanitized(draft)
        let trimmedSecret = secret.trimmingCharacters(in: .whitespacesAndNewlines)
        var result: [Field: String] = [:]

        if draft.displayName.isEmpty {
            result[.displayName] = "표시 이름을 입력하세요."
        }
        if draft.region.isEmpty {
            result[.region] = "리전을 입력하세요. (예: us-east-1)"
        }
        validateBucket(draft.bucket, into: &result)
        if let preset = draft.preset {
            if preset.needsAccountId, draft.accountId == nil {
                result[.accountId] = "계정 ID를 입력하세요."
            }
            if preset.needsEndpoint {
                validateEndpoint(draft.endpoint, into: &result)
            }
        } else {
            // 알 수 없는 프리셋 id는 사용자 지정처럼 엔드포인트가 필요하다.
            validateEndpoint(draft.endpoint, into: &result)
        }
        if draft.accessKeyId.isEmpty {
            result[.accessKeyId] = "Access Key ID를 입력하세요."
        }
        // 편집 시 빈 시크릿은 "기존 키체인 값 유지"이므로 유효하다.
        if !isEditing && trimmedSecret.isEmpty {
            result[.secret] = "Secret Access Key를 입력하세요."
        }
        return result
    }

    /// 모든 필드가 유효한지 여부.
    static func isValid(draft: SavedConnection, secret: String, isEditing: Bool) -> Bool {
        errors(draft: draft, secret: secret, isEditing: isEditing).isEmpty
    }

    // MARK: - 필드별 규칙

    /// S3 버킷 이름 규칙: 3~63자, 소문자·숫자·'.'·'-', 첫/끝 영숫자,
    /// '..' 금지, IP 주소 형태 금지.
    private static func validateBucket(_ bucket: String, into result: inout [Field: String]) {
        guard !bucket.isEmpty else {
            result[.bucket] = "버킷 이름을 입력하세요."
            return
        }
        guard (3...63).contains(bucket.count) else {
            result[.bucket] = "버킷 이름은 3~63자여야 합니다."
            return
        }
        guard bucket.allSatisfy({ $0.isASCII && bucketAllowedCharacters.contains($0) }) else {
            result[.bucket] = "소문자, 숫자, '.', '-'만 사용할 수 있습니다."
            return
        }
        guard let first = bucket.first, first.isLetter || first.isNumber,
            let last = bucket.last, last.isLetter || last.isNumber
        else {
            result[.bucket] = "첫 글자와 마지막 글자는 영문자 또는 숫자여야 합니다."
            return
        }
        if bucket.contains("..") {
            result[.bucket] = "'.'를 연속으로 사용할 수 없습니다."
            return
        }
        if isIPFormatted(bucket) {
            result[.bucket] = "IP 주소 형태의 이름은 사용할 수 없습니다."
        }
    }

    /// http:// 또는 https:// 스킴과 호스트가 있는지 검사한다.
    private static func validateEndpoint(_ endpoint: String?, into result: inout [Field: String]) {
        guard let endpoint else {
            result[.endpoint] = "엔드포인트를 입력하세요. (예: http://127.0.0.1:9000)"
            return
        }
        let trimmed = endpoint.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else {
            result[.endpoint] = "엔드포인트를 입력하세요. (예: http://127.0.0.1:9000)"
            return
        }
        guard let components = URLComponents(string: trimmed),
            let scheme = components.scheme?.lowercased(),
            scheme == "http" || scheme == "https",
            let host = components.host,
            !host.isEmpty,
            !host.contains(where: { $0.isWhitespace })
        else {
            result[.endpoint] = "http:// 또는 https:// 로 시작하는 주소를 입력하세요."
            return
        }
    }

    /// 숫자와 점만으로 이뤄어진 IP 주소 형태인지 검사한다(예: 192.168.5.4).
    private static func isIPFormatted(_ name: String) -> Bool {
        name.contains(".") && name.allSatisfy { $0.isNumber || $0 == "." }
    }

    private static func trimOptional(_ value: String?) -> String? {
        guard let value else { return nil }
        let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? nil : trimmed
    }
}
