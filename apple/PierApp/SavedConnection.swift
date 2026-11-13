import Foundation

/// `~/Library/Application Support/com.yourname.pier/connections.json`에 저장되는 연결 정보.
/// 시크릿 Access Key는 이 구조체에 넣지 않고 키체인에 별도로 저장한다(SPEC 6.1).
struct SavedConnection: Codable, Identifiable, Hashable {
    /// UI에 노출하는 서비스 종류. Phase 2는 AWS/MinIO/사용자 지정만 지원한다.
    enum ProviderKind: String, Codable, CaseIterable {
        case aws
        case minio
        case custom

        var label: String {
            switch self {
            case .aws: return "AWS S3"
            case .minio: return "MinIO"
            case .custom: return "사용자 지정"
            }
        }
    }

    var id: UUID
    var displayName: String
    var provider: ProviderKind
    var endpoint: String?
    var region: String
    var bucket: String
    var prefix: String?
    var pathStyle: Bool
    var accessKeyId: String
    /// 연결 테스트에서 확인한 쓰기 권한. nil은 미확인(이전 버전 연결 등)으로 쓰기 허용으로 본다.
    var canWrite: Bool?

    /// 아직 저장되지 않은 새 연결의 초깃값.
    static func emptyDraft() -> SavedConnection {
        SavedConnection(
            id: UUID(),
            displayName: "",
            provider: .aws,
            endpoint: nil,
            region: "us-east-1",
            bucket: "",
            prefix: nil,
            pathStyle: false,
            accessKeyId: "",
            canWrite: nil
        )
    }

    /// provider에 맞는 필드 기본값을 채운다. 이미 입력된 값은 유지한다.
    mutating func applyProviderDefaults() {
        if region.trimmingCharacters(in: .whitespaces).isEmpty {
            region = "us-east-1"
        }
        switch provider {
        case .aws:
            pathStyle = false
        case .minio:
            pathStyle = true
            if endpoint?.trimmingCharacters(in: .whitespaces).isEmpty ?? true {
                endpoint = "http://127.0.0.1:9000"
            }
        case .custom:
            pathStyle = true
        }
    }

    /// FFI `PierConfig`로 변환한다. 필수값이 비어 있으면 nil.
    /// endpoint는 aws가 아니면 필수이고, aws면 항상 nil로 보낸다.
    var pierConfig: PierConfig? {
        guard let bucketName = Self.nonEmpty(bucket),
            let regionName = Self.nonEmpty(region),
            let accessKey = Self.nonEmpty(accessKeyId)
        else { return nil }

        var endpointValue = Self.nonEmpty(endpoint)
        if provider == .aws {
            endpointValue = nil
        } else if endpointValue == nil {
            // MinIO/사용자 지정은 엔드포인트 없이는 연결할 수 없다.
            return nil
        }

        return PierConfig(
            endpoint: endpointValue,
            region: regionName,
            bucket: bucketName,
            prefix: Self.nonEmpty(prefix),
            pathStyle: provider == .aws ? false : pathStyle,
            accessKeyId: accessKey
        )
    }

    private static func nonEmpty(_ value: String?) -> String? {
        guard let value else { return nil }
        let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? nil : trimmed
    }
}
