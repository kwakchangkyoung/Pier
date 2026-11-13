import Foundation

/// 코어 전송 대기열의 스냅샷을 연결별로 모아 앱 전체 목록으로 보여준다.
/// 실제 전송은 코어 대기열이 실행하고, 여기서는 등록·제어 호출·스냅샷 반영만 한다.
@MainActor @Observable
final class TransferManager {
    private var jobMap: [JobKey: PierJob] = [:]
    /// 등록된 순서. 목록 표시 순서로 쓴다.
    private var order: [JobKey] = []
    /// 작업이 속한 연결의 코어 객체. pause/resume 등 제어 호출을 라우팅한다.
    private var connections: [UUID: PierConnection] = [:]
    /// 작업별 후속 동작(완료 콜백, 충돌 재질문, 프로미스 완료). TransferEnqueue.swift가 채운다.
    var contexts: [JobKey: JobContext] = [:]

    /// 대기열 패널 펼침 상태. 메뉴바 "전송 창 열기"와 연동한다.
    var panelExpanded = false

    /// 전체 작업을 등록 순서로 반환한다.
    var items: [QueueItem] {
        order.compactMap { key in
            jobMap[key].map { QueueItem(key: key, job: $0) }
        }
    }

    /// 진행 중(대기·실행·일시정지) 작업 수.
    var activeCount: Int {
        jobMap.values.count { $0.state.isActive }
    }

    /// 실행 중인 작업의 합산 속도(초당 바이트).
    var aggregateSpeed: UInt64 {
        jobMap.values.reduce(0) { $1.state == .running ? $0 + $1.bytesPerSec : $0 }
    }

    /// 진행 중 작업이 있는지. 메뉴바 아이콘 전환에 쓴다.
    var hasActiveJobs: Bool { activeCount > 0 }

    /// 실행 중인 작업을 등록 순으로 최대 limit개 반환한다(메뉴바 요약용).
    func runningItems(limit: Int) -> [QueueItem] {
        Array(items.lazy.filter { $0.job.state == .running }.prefix(limit))
    }

    // MARK: - 연결 등록

    /// 열린 연결을 대기열 집계에 등록한다. 연결마다 한 번만 호출한다.
    func register(connection: PierConnection, connectionId: UUID) {
        connections[connectionId] = connection
        connection.setJobListener(listener: JobListenerBridge(connectionId: connectionId, manager: self))
        // 이미 큐에 들어 있는 작업(있으면)을 반영한다.
        for job in connection.jobs() {
            apply(job: job, connectionId: connectionId)
        }
    }

    /// 연결이 폐기될 때 호출한다. 스냅샷도 지운다(큐는 메모리에만 존재).
    func unregister(connectionId: UUID) {
        connections.removeValue(forKey: connectionId)
        for key in order where key.connectionId == connectionId {
            contexts.removeValue(forKey: key)
            jobMap.removeValue(forKey: key)
        }
        order.removeAll { $0.connectionId == connectionId }
    }

    // MARK: - 작업 제어

    func pause(_ item: QueueItem) {
        connections[item.key.connectionId]?.pauseJob(id: item.job.id)
    }

    func resume(_ item: QueueItem) {
        connections[item.key.connectionId]?.resumeJob(id: item.job.id)
    }

    func cancel(_ item: QueueItem) {
        connections[item.key.connectionId]?.cancelJob(id: item.job.id)
    }

    func retry(_ item: QueueItem) {
        connections[item.key.connectionId]?.retryJob(id: item.job.id)
    }

    /// 종료 상태의 작업을 목록에서 지운다.
    func remove(_ item: QueueItem) {
        removeByKey(item.key)
    }

    /// 완료된 작업을 모두 지운다.
    func clearCompleted() {
        for item in items where item.job.state == .completed {
            removeByKey(item.key)
        }
    }

    // MARK: - 리스너 반영

    /// 리스너(메인 스레드로 전달된) 스냅샷을 반영한다.
    func apply(job: PierJob, connectionId: UUID) {
        let key = JobKey(connectionId: connectionId, jobId: job.id)
        if jobMap[key] == nil {
            order.append(key)
        }
        jobMap[key] = job
        if job.state.isTerminal {
            finishJob(key: key, job: job)
        }
    }

    // MARK: - 내부

    /// 코어와 로컬 목록에서 작업을 지운다.
    func removeByKey(_ key: JobKey) {
        connections[key.connectionId]?.removeJob(id: key.jobId)
        contexts.removeValue(forKey: key)
        jobMap.removeValue(forKey: key)
        order.removeAll { $0 == key }
    }
}

/// 코어 대기열 리스너 구현. 백그라운드 스레드에서 호출되므로
/// DispatchQueue.main으로 넘겨 스냅샷 순서대로 반영한다.
final class JobListenerBridge: PierJobListener, @unchecked Sendable {
    private let connectionId: UUID
    private let manager: TransferManager

    init(connectionId: UUID, manager: TransferManager) {
        self.connectionId = connectionId
        self.manager = manager
    }

    func onJobUpdate(job: PierJob) {
        let connectionId = self.connectionId
        let manager = self.manager
        DispatchQueue.main.async {
            MainActor.assumeIsolated {
                manager.apply(job: job, connectionId: connectionId)
            }
        }
    }
}
