import Foundation

/// Phase 0 전용 개발용 연결 설정.
/// 로컬 MinIO(docker/compose.yml) 자격증명을 하드코딩한다.
/// Phase 2에서 키체인 + 설정 기반 연결 관리로 대체한다.
enum DevConnection {
    static let config = PierConfig(
        endpoint: "http://127.0.0.1:9000",
        region: "us-east-1",
        bucket: "pier-test",
        prefix: nil,
        pathStyle: true,
        accessKeyId: "minioadmin"
    )

    // Phase 0 테스트 전용 자격증명. Phase 2에서 키체인으로 대체한다.
    static let secretAccessKey = "minioadmin"
}
