import Foundation

/// 파일 목록 정렬 키. 테이블 열 식별자(FileTableView.ColumnID)와 같은 raw 값을 쓴다.
enum EntrySortKey: String, CaseIterable {
    case name
    case size
    case modified
}

/// 정렬 키와 방향을 함께 묶은 값.
struct EntrySort: Equatable {
    var key: EntrySortKey
    var ascending: Bool
}

/// 정렬 비교에 필요한 항목 값. PierEntry에서 뽑아 순수 함수로 계산한다.
/// PierEntry를 직접 쓰지 않아 이 파일은 앱 없이도 컴파일·검증할 수 있다.
struct EntrySortFields: Equatable {
    var name: String
    var isDir: Bool
    var size: UInt64
    var lastModifiedMs: Int64?
}

/// 파일 목록의 정렬·이름 필터 계산.
/// 네트워크나 코어와 무관한 순수 함수만 둔다.
enum EntryOrdering {
    /// 검색어가 이름에 포함되면 true. 앞뒤 공백은 무시하고 빈 검색어는 전체에 일치한다.
    /// 비교 전 양쪽을 NFC로 정규화해 NFD(분해형) 한글 이름도 찾는다.
    /// localizedStandardContains는 대소문자·폭을 무시한다.
    static func nameMatches(_ name: String, query: String) -> Bool {
        let needle = normalized(query.trimmingCharacters(in: .whitespacesAndNewlines))
        if needle.isEmpty { return true }
        return normalized(name).localizedStandardContains(needle)
    }

    /// sorted(by:)용 비교기. a가 b보다 앞에 와야 하면 true.
    /// 폴더는 정렬 방향과 무관하게 항상 파일보다 위에 둔다.
    static func isOrderedBefore(
        _ a: EntrySortFields, _ b: EntrySortFields, sort: EntrySort
    ) -> Bool {
        if a.isDir != b.isDir { return a.isDir }
        // 수정 시각이 없는 항목은 방향과 무관하게 맨 뒤에 둔다.
        if sort.key == .modified, (a.lastModifiedMs != nil) != (b.lastModifiedMs != nil) {
            return a.lastModifiedMs != nil
        }
        let result = compare(a, b, key: sort.key)
        if result != .orderedSame {
            return sort.ascending ? result == .orderedAscending : result == .orderedDescending
        }
        // 동률이면 이름 오름차순으로 결정적 순서를 만든다.
        return compareNames(a.name, b.name) == .orderedAscending
    }

    /// 키별 오름차순 비교 결과.
    private static func compare(
        _ a: EntrySortFields, _ b: EntrySortFields, key: EntrySortKey
    ) -> ComparisonResult {
        switch key {
        case .name:
            return compareNames(a.name, b.name)
        case .size:
            // 폴더는 크기가 없어 폴더끼리는 동률로 두고 이름 2차 정렬에 맡긴다.
            if a.isDir { return .orderedSame }
            if a.size != b.size {
                return a.size < b.size ? .orderedAscending : .orderedDescending
            }
            return .orderedSame
        case .modified:
            guard let aMs = a.lastModifiedMs, let bMs = b.lastModifiedMs else {
                return .orderedSame
            }
            if aMs != bMs {
                return aMs < bMs ? .orderedAscending : .orderedDescending
            }
            return .orderedSame
        }
    }

    /// Finder식 자연 정렬: 숫자를 값으로 인식하고 대소문자를 무시한다.
    static func compareNames(_ a: String, _ b: String) -> ComparisonResult {
        normalized(a).localizedStandardCompare(normalized(b))
    }

    /// NFC로 정규화한다. Finder에서 올린 한글 파일명은 NFD일 수 있어
    /// 정규화 없이 비교하면 보이는 같은 문자열도 다르게 처리된다.
    private static func normalized(_ string: String) -> String {
        string.precomposedStringWithCanonicalMapping
    }
}
