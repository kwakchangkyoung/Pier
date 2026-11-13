import Foundation

/// 코어 대기열 작업을 앱 전체 목록에서 구분하는 키.
/// 작업 id는 연결 안에서만 유일하므로 연결 id와 쌍을 이룬다.
struct JobKey: Hashable {
    let connectionId: UUID
    let jobId: UInt64
}

/// 화면에 표시하는 대기열 항목. 코어가 보낸 가장 최근 스냅샷을 들고 있다.
struct QueueItem: Identifiable {
    let key: JobKey
    var job: PierJob

    var id: JobKey { key }
}

extension PierJobState {
    /// 아직 끝나지 않은 상태(대기·실행·일시정지).
    var isActive: Bool {
        switch self {
        case .queued, .running, .paused: return true
        case .completed, .failed, .cancelled: return false
        }
    }

    /// 더 이상 진행되지 않는 종료 상태.
    var isTerminal: Bool { !isActive }
}
