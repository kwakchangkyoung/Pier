import Foundation

/// 브라우저의 표시 상태: 정렬, 이름 필터와 그 결과로 캐시한 표시 배열.
/// 원본 목록(entries)은 BrowserModel이 들고, 여기서는 표시 계산만 한다.
/// 정렬·필터 계산은 원본이나 입력이 바뀔 때 한 번씩만 하고 결과를 entries에 캐시한다.
@MainActor @Observable
final class BrowserDisplay {
    /// 화면에 표시하는 배열. 원본에 정렬과 이름 필터를 적용한 결과다.
    /// 테이블 표시·선택·드래그 등 행 기준 동작은 모두 이 배열을 따른다.
    private(set) var entries: [PierEntry] = []
    /// 이름 필터 검색어. 앞뒤 공백을 제외한 빈 문자열이면 필터를 끈 것으로 본다.
    var searchQuery = "" {
        didSet { recompute() }
    }
    /// 정렬 키와 방향. UserDefaults에 저장해 앱 재실행 후에도 유지한다(앱 공통 1개 설정).
    private(set) var sort: EntrySort {
        didSet {
            saveSort()
            recompute()
        }
    }

    /// 마지막으로 계산에 쓴 원본 목록. 정렬·검색어만 바뀌어도 재계산할 수 있게 둔다.
    private var source: [PierEntry] = []
    private let defaults: UserDefaults

    /// 정렬 설정을 저장하는 UserDefaults 키.
    private static let sortKeyDefaultsKey = "browser.sortKey"
    private static let sortAscendingDefaultsKey = "browser.sortAscending"

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        var restored = EntrySort(key: .name, ascending: true)
        if let raw = defaults.string(forKey: Self.sortKeyDefaultsKey),
            let key = EntrySortKey(rawValue: raw)
        {
            restored.key = key
        }
        if let stored = defaults.object(forKey: Self.sortAscendingDefaultsKey) as? Bool {
            restored.ascending = stored
        }
        sort = restored
    }

    /// 원본 목록이 바뀌었을 때 BrowserModel이 호출한다.
    /// 현재 정렬·필터를 유지한 채 표시 배열을 다시 계산한다.
    func update(_ newSource: [PierEntry]) {
        source = newSource
        recompute()
    }

    /// 열 머리글 정렬 클릭에 연결한다. 같은 값이면 아무것도 하지 않는다.
    func applySort(_ newSort: EntrySort) {
        guard newSort != sort else { return }
        sort = newSort
    }

    /// 공백을 제외한 실제 검색어.
    var trimmedQuery: String {
        searchQuery.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    /// 이름 필터가 켜져 있는지.
    var isFiltering: Bool { !trimmedQuery.isEmpty }

    /// 원본 목록에 정렬과 이름 필터를 적용해 표시 배열을 다시 계산한다.
    private func recompute() {
        let query = trimmedQuery
        var result = source
        if !query.isEmpty {
            result = result.filter { EntryOrdering.nameMatches($0.name, query: query) }
        }
        let sort = self.sort
        result.sort {
            EntryOrdering.isOrderedBefore($0.sortFields, $1.sortFields, sort: sort)
        }
        entries = result
    }

    private func saveSort() {
        defaults.set(sort.key.rawValue, forKey: Self.sortKeyDefaultsKey)
        defaults.set(sort.ascending, forKey: Self.sortAscendingDefaultsKey)
    }
}

extension PierEntry {
    /// 정렬 비교에 넘기는 값만 뽑은 것.
    var sortFields: EntrySortFields {
        EntrySortFields(name: name, isDir: isDir, size: size, lastModifiedMs: lastModifiedMs)
    }
}

/// 표시 상태 접근용 짧은 경로. 뷰는 model.display를 직접 건드리지 않고 이 경로를 쓴다.
extension BrowserModel {
    /// 화면에 표시하는 배열(정렬·필터 적용 결과).
    var displayedEntries: [PierEntry] { display.entries }
    /// 이름 필터 검색어.
    var searchQuery: String {
        get { display.searchQuery }
        set { display.searchQuery = newValue }
    }
    /// 공백을 제외한 실제 검색어.
    var trimmedQuery: String { display.trimmedQuery }
    /// 이름 필터가 켜져 있는지.
    var isFiltering: Bool { display.isFiltering }
    /// 표시 정렬(키·방향).
    var sort: EntrySort { display.sort }
    /// 열 머리글 정렬 클릭을 표시 상태에 반영한다.
    func applySort(_ newSort: EntrySort) {
        display.applySort(newSort)
    }
}
