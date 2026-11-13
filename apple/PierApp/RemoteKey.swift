import Foundation

/// S3 키와 표시 이름/부모 폴더 키 사이의 변환 유틸.
/// 아이템 식별자는 루트 기준 상대 키이며 폴더는 '/'로 끝난다(SPEC 3.1).
enum RemoteKey {
    /// 키가 가리키는 항목 이름. 폴더 키("a/b/")는 "b", 파일("a/b/c")은 "c", 루트("")는 "".
    static func name(of key: String) -> String {
        var trimmed = key
        if trimmed.hasSuffix("/") {
            trimmed.removeLast()
        }
        return trimmed.split(separator: "/").last.map(String.init) ?? ""
    }

    /// 키의 부모 폴더 키. '/'로 끝나고 루트는 ""다.
    /// "a/b/c" → "a/b/", "a/b/" → "a/", "c" → "", "a/" → "".
    static func parent(of key: String) -> String {
        var trimmed = key
        if trimmed.hasSuffix("/") {
            trimmed.removeLast()
        }
        guard let index = trimmed.lastIndex(of: "/") else { return "" }
        // index는 '/'의 위치이므로 그 앞까지가 부모 경로 + 구분자다.
        return String(trimmed[...index])
    }
}
