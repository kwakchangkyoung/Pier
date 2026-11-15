import Foundation

/// 연결 하나의 폴더 탐색 상태.
/// PierSessions가 connection id별로 하나씩 만들고, 앱 수명 동안 유지한다.
@MainActor @Observable
final class BrowserModel {
    enum Phase: Equatable {
        case idle
        case loading
        case ready
        case failed(String)
    }

    let connectionId: UUID
    /// breadcrumb 맨 앞에 표시하는 연결 이름.
    let title: String

    /// 다운로드에 사용하는 열려 있는 코어 연결. open 성공 후에만 non-nil.
    private(set) var connection: PierConnection?

    /// 표시 상태(정렬·이름 필터와 계산된 표시 배열). 계산은 BrowserDisplay가 한다.
    let display = BrowserDisplay()

    /// 연결 설정의 쓰기 권한. false면 업로드·이름 변경·이동·삭제 UI를 비활성화한다.
    let canWrite: Bool

    /// 현재 폴더의 항목. 키 순(코어가 주는 순서)으로 정렬돼 있다.
    /// 화면에 보이는 배열은 이 원본에 정렬·필터를 적용한 display.entries다.
    private(set) var entries: [PierEntry] = [] {
        didSet { display.update(entries) }
    }
    private(set) var phase: Phase = .idle
    /// 백그라운드 refresh 진행 중 여부(툴바 스피너 표시용).
    private(set) var isRefreshing = false

    /// 루트("")부터 현재 폴더까지의 경로. 원소는 '/'로 끝나는 폴더 키.
    var path: [String] { backStack + [currentFolder] }
    private(set) var currentFolder = ""
    var canGoBack: Bool { !backStack.isEmpty }
    var canGoForward: Bool { !forwardStack.isEmpty }

    private var backStack: [String] = []
    private var forwardStack: [String] = []
    private let connectionProvider: () throws -> PierConnection
    private var loadTask: Task<Void, Never>?
    private var refreshTask: Task<Void, Never>?
    /// 현재 폴더 하위 목록 프리페치 작업. 폴더를 떠나면 취소한다.
    private var prefetchTask: Task<Void, Never>?
    private var didStart = false
    /// 백그라운드 refresh 세대 번호. 취소된 이전 작업이 플래그를 건드리지 않게 한다.
    private var refreshGeneration = 0

    init(
        connectionId: UUID,
        title: String,
        canWrite: Bool = true,
        connectionProvider: @escaping () throws -> PierConnection
    ) {
        self.connectionId = connectionId
        self.title = title
        self.canWrite = canWrite
        self.connectionProvider = connectionProvider
    }

    /// 최초 1회만 초기 로드를 시작한다.
    func activate() {
        guard !didStart else { return }
        didStart = true
        loadTask = Task { await openConnectionAndLoad() }
    }

    /// 새로고침(툴바/⌘R/메뉴바). 캐시가 있으면 즉시 보여주고 백그라운드로 갱신한다.
    func reload() {
        cancelTasks()
        if connection == nil {
            loadTask = Task { await openConnectionAndLoad() }
        } else {
            loadTask = Task { await loadPages() }
        }
    }

    /// 폴더로 이동한다. identifier는 '/'로 끝나는 폴더 키.
    func navigate(toFolder folder: String) {
        guard folder != currentFolder else { return }
        backStack.append(currentFolder)
        forwardStack.removeAll()
        currentFolder = folder
        moveAndReload()
    }

    func goBack() {
        guard let previous = backStack.popLast() else { return }
        forwardStack.append(currentFolder)
        currentFolder = previous
        moveAndReload()
    }

    func goForward() {
        guard let next = forwardStack.popLast() else { return }
        backStack.append(currentFolder)
        currentFolder = next
        moveAndReload()
    }

    /// 목록이 있는 상태에서 표시 중인 오류 배너를 닫는다.
    func clearError() {
        if case .failed = phase {
            phase = .ready
        }
    }

    /// breadcrumb의 index 위치로 점프한다. path[index]가 대상 폴더.
    func jumpToBreadcrumb(_ index: Int) {
        let target = path
        guard target.indices.contains(index), target[index] != currentFolder else { return }
        forwardStack = Array(target[(index + 1)...].reversed())
        backStack = Array(target[..<index])
        currentFolder = target[index]
        moveAndReload()
    }

    // MARK: - 내부

    /// 폴더가 바뀌는 이동: 검색어와 이전 목록을 지우고 로드를 시작한다.
    private func moveAndReload() {
        display.searchQuery = ""
        entries = []
        phase = .loading
        reload()
    }

    private func cancelTasks() {
        loadTask?.cancel()
        refreshTask?.cancel()
        prefetchTask?.cancel()
        refreshGeneration += 1
        isRefreshing = false
    }

    private func openConnectionAndLoad() async {
        do {
            connection = try connectionProvider()
        } catch {
            phase = .failed(PierErrorPresenter.message(for: error) ?? "연결을 열 수 없습니다.")
            return
        }
        await resumePendingOps()
        await loadPages()
    }

    /// 중단됐던 폴더 이름 변경 등 보류 작업을 재개한다(SPEC 3.4).
    /// 처리된 건수가 있으면 로그를 남기고, 목록은 이어지는 loadPages가 새로 가져온다.
    private func resumePendingOps() async {
        guard let conn = connection else { return }
        do {
            let resumed = try await conn.resumePendingOps()
            if resumed > 0 {
                Log.app.info("보류 작업 \(resumed)건을 재개했습니다")
            }
        } catch {
            Log.app.error("resumePendingOps 실패: \(PierErrorPresenter.categoryName(of: error))")
        }
    }

    /// listPage를 커서 끝까지 따라가며 전체 목록을 모은다.
    /// 첫 페이지가 캐시에서 왔으면 백그라운드 refresh를 시작한다.
    private func loadPages() async {
        guard let conn = connection else { return }
        let folder = currentFolder
        if entries.isEmpty {
            phase = .loading
        }
        do {
            var items: [PierEntry] = []
            var cursor: String?
            var firstPageFromCache = false
            var isFirstPage = true
            repeat {
                try Task.checkCancellation()
                let page = try await conn.listPage(folder: folder, cursor: cursor)
                if isFirstPage {
                    firstPageFromCache = page.fromCache
                    isFirstPage = false
                }
                items.append(contentsOf: page.items)
                cursor = page.nextCursor
            } while cursor != nil
            try Task.checkCancellation()
            // 갱신 반영(applyChanges) 후와 순서를 맞추기 위해 여기서도 정렬한다.
            items.sort { $0.identifier < $1.identifier }
            entries = items
            phase = .ready
            startPrefetch(folder: folder, connection: conn)
            if firstPageFromCache {
                startRefresh(folder: folder, connection: conn)
            }
        } catch is CancellationError {
            // 다른 탐색이 이 작업을 취소한 경우 — 무시한다.
        } catch {
            guard !Task.isCancelled else { return }
            if let message = PierErrorPresenter.message(for: error) {
                phase = .failed(message)
            }
        }
    }

    /// 목록 표시 직후 하위 폴더를 백그라운드로 미리 읽어 둔다(Phase 4 프리페치).
    /// 폴더를 떠나면 cancelTasks가 이 작업을 취소한다. 실패는 조용히 무시한다.
    private func startPrefetch(folder: String, connection conn: PierConnection) {
        prefetchTask?.cancel()
        prefetchTask = Task {
            do {
                let prefetched = try await conn.prefetch(folder: folder)
                if prefetched > 0 {
                    Log.app.debug("prefetch 완료: \(prefetched)개 폴더")
                }
            } catch {
                if !Task.isCancelled {
                    Log.app.error("prefetch 실패: \(PierErrorPresenter.categoryName(of: error))")
                }
            }
        }
    }

    /// 캐시된 목록을 보여준 뒤 백그라운드로 원격 갱신하고 변경을 반영한다.
    private func startRefresh(folder: String, connection conn: PierConnection) {
        refreshGeneration += 1
        let generation = refreshGeneration
        isRefreshing = true
        refreshTask = Task {
            defer {
                if generation == refreshGeneration {
                    isRefreshing = false
                }
            }
            do {
                let changes = try await conn.refresh(folder: folder)
                try Task.checkCancellation()
                guard folder == currentFolder else { return }
                apply(changes)
            } catch {
                // 백그라운드 갱신 실패는 목록을 그대로 두고 로그만 남긴다.
                if !Task.isCancelled {
                    Log.app.error("refresh 실패: \(PierErrorPresenter.categoryName(of: error))")
                }
            }
        }
    }

    /// refresh 결과를 현재 목록에 반영한다. 식별자 기준 upsert/remove 후 재정렬.
    private func apply(_ changes: PierChanges) {
        var byId = Dictionary(uniqueKeysWithValues: entries.map { ($0.identifier, $0) })
        for identifier in changes.removed {
            byId[identifier] = nil
        }
        for entry in changes.upserted {
            byId[entry.identifier] = entry
        }
        entries = byId.values.sorted { $0.identifier < $1.identifier }
    }

    // MARK: - 쓰기 결과 반영 (BrowserModel+Write.swift에서 사용)

    /// 쓰기 작업 결과를 현재 목록에 즉시 반영하고 백그라운드 refresh를 시작한다.
    /// upserted 항목은 현재 폴더 것이 아니면 무시한다(다른 폴더에 올린 경우 등).
    func applyWriteResults(removed identifiers: [String] = [], upserted newEntries: [PierEntry] = []) {
        if !identifiers.isEmpty {
            entries.removeAll { identifiers.contains($0.identifier) }
        }
        for entry in newEntries where belongsToCurrentFolder(entry) {
            upsert(entry)
        }
        entries.sort { $0.identifier < $1.identifier }
        if let conn = connection {
            startRefresh(folder: currentFolder, connection: conn)
        }
    }

    /// 항목의 부모 폴더가 현재 폴더인지 여부.
    func belongsToCurrentFolder(_ entry: PierEntry) -> Bool {
        RemoteKey.parent(of: entry.identifier) == currentFolder
    }

    private func upsert(_ entry: PierEntry) {
        if let index = entries.firstIndex(where: { $0.identifier == entry.identifier }) {
            entries[index] = entry
        } else {
            entries.append(entry)
        }
    }
}
