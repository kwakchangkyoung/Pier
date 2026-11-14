import Foundation

/// `~/Library/Application Support/com.gajago.pier/connections.json`에 저장되는 연결 정보.
/// 시크릿 Access Key는 이 구조체에 넣지 않고 키체인에 별도로 저장한다(SPEC 6.1).
struct SavedConnection: Codable, Identifiable, Hashable {
    var id: UUID
    var displayName: String
    /// 서비스 프리셋 id ("aws", "r2", "wasabi", "minio", "custom").
    /// 이전 버전의 provider 문자열과 같은 값이라 그대로 읽힌다.
    var provider: String
    /// R2처럼 엔드포인트 계산에 필요한 계정 식별자.
    var accountId: String?
    var endpoint: String?
    var region: String
    var bucket: String
    var prefix: String?
    var pathStyle: Bool
    var accessKeyId: String
    /// 연결 테스트에서 확인한 쓰기 권한. nil은 미확인(이전 버전 연결 등)으로 쓰기 허용으로 본다.
    var canWrite: Bool?
    /// 전송 청크 크기(MB)·동시성. nil이면 프리셋 기본값을 쓴다.
    var chunkSizeMb: UInt32?
    var concurrency: UInt32?

    /// provider id에 대응하는 코어 프리셋. 알 수 없는 id면 nil(사용자 지정처럼 취급).
    var preset: PierPreset? {
        presets().first { $0.id == provider }
    }

    /// 목록·사이드바에 보여줄 서비스 이름.
    var providerLabel: String {
        preset?.displayName ?? provider
    }

    /// 아직 저장되지 않은 새 연결의 초깃값.
    static func emptyDraft() -> SavedConnection {
        SavedConnection(
            id: UUID(),
            displayName: "",
            provider: "aws",
            accountId: nil,
            endpoint: nil,
            region: "us-east-1",
            bucket: "",
            prefix: nil,
            pathStyle: false,
            accessKeyId: "",
            canWrite: nil,
            chunkSizeMb: nil,
            concurrency: nil
        )
    }

    /// 프리셋에 맞는 필드 기본값을 채운다. 서비스가 바뀔 때 호출한다.
    mutating func applyPresetDefaults() {
        guard let preset else { return }
        region = preset.defaultRegion
        pathStyle = preset.pathStyle
        if preset.needsAccountId {
            // 엔드포인트는 계정 ID로 계산하므로 직접 입력값은 지운다.
            endpoint = nil
        } else if !preset.needsEndpoint {
            endpoint = nil
        } else if preset.id == "minio",
            endpoint?.trimmingCharacters(in: .whitespaces).isEmpty ?? true
        {
            // MinIO 로컬 개발용 기본값.
            endpoint = "http://127.0.0.1:9000"
        }
    }

    /// FFI `PierConfig`로 변환한다. 필수값이 비어 있으면 nil.
    /// 엔드포인트는 프리셋 규칙으로 정한다: 계정 ID로 계산 / 직접 입력 / 없음.
    var pierConfig: PierConfig? {
        guard let bucketName = Self.nonEmpty(bucket),
            let regionName = Self.nonEmpty(region),
            let accessKey = Self.nonEmpty(accessKeyId)
        else { return nil }

        let endpointValue: String?
        if let preset, preset.needsAccountId {
            endpointValue = presetEndpoint(
                presetId: preset.id,
                accountId: Self.nonEmpty(accountId),
                region: regionName
            )
            guard endpointValue != nil else { return nil }
        } else if let preset, !preset.needsEndpoint {
            // AWS 등 엔드포인트를 쓰지 않는 프리셋.
            endpointValue = nil
        } else {
            // MinIO/사용자 지정/알 수 없는 프리셋: 직접 입력이 필요하다.
            endpointValue = Self.nonEmpty(endpoint)
            guard endpointValue != nil else { return nil }
        }

        return PierConfig(
            provider: provider,
            endpoint: endpointValue,
            region: regionName,
            bucket: bucketName,
            prefix: Self.nonEmpty(prefix),
            pathStyle: pathStyle,
            accessKeyId: accessKey,
            chunkSizeMb: chunkSizeMb,
            concurrency: concurrency
        )
    }

    private static func nonEmpty(_ value: String?) -> String? {
        guard let value else { return nil }
        let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? nil : trimmed
    }
}
