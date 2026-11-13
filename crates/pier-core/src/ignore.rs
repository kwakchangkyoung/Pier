//! SPEC 3.6 업로드 제외 규칙.
//!
//! 폴더를 통째로 업로드할 때 macOS가 만드는 메타데이터 파일은 버킷에
//! 올리지 않는다. 기본 목록은 [`is_ignored`]가 항상 검사하고,
//! `extra`로 호출자가 추가 패턴을 넘길 수 있다(설정 UI는 후속 단계).

/// 기본 제외 패턴. 끝이 `*`면 접두사 일치, 아니면 이름 전체 일치다.
const DEFAULT_PATTERNS: &[&str] = &[
    ".DS_Store",
    "._*",
    ".localized",
    "Icon\r",
    ".Spotlight-V100",
    ".Trashes",
];

/// name이 업로드 제외 대상인지 판별한다.
///
/// 기본 목록과 `extra` 목록을 같은 규칙으로 검사한다:
/// 패턴이 `*`로 끝나면 나머지 부분과의 접두사 일치(`._*` → `._`로 시작),
/// 아니면 이름 전체 일치. `*`가 중간에 있거나 패턴 전체가 `*`인 경우는
/// 지원하지 않는다(단순 규칙 유지).
pub fn is_ignored(name: &str, extra: &[String]) -> bool {
    DEFAULT_PATTERNS.iter().any(|p| matches(p, name)) || extra.iter().any(|p| matches(p, name))
}

fn matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extra(patterns: &[&str]) -> Vec<String> {
        patterns.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn default_patterns() {
        for name in [
            ".DS_Store",
            "._foo",
            "._",
            ".localized",
            "Icon\r",
            ".Spotlight-V100",
            ".Trashes",
        ] {
            assert!(is_ignored(name, &[]), "{name:?}가 무시되지 않았다");
        }
    }

    #[test]
    fn similar_names_are_not_ignored() {
        // 접두사 패턴이 아닌 항목은 정확히 일치해야 한다.
        for name in [
            ".DS_Store2",
            "x.DS_Store",
            ".localized2",
            "Icon",
            ".Spotlight-V1000",
            ".Trashes2",
            "normal.txt",
            "..hidden",
        ] {
            assert!(!is_ignored(name, &[]), "{name:?}가 잘못 무시됐다");
        }
    }

    #[test]
    fn extra_patterns() {
        let ex = extra(&["*.tmp", "build-output"]);
        // '*.tmp'는 끝 '*'가 아니라 접두사 규칙이 아니므로 정확 일치만 한다.
        assert!(is_ignored("*.tmp", &ex));
        assert!(!is_ignored("a.tmp", &ex));
        assert!(is_ignored("build-output", &ex));
        assert!(!is_ignored("build-output2", &ex));
        // 끝 '*'는 접두사 일치.
        let ex2 = extra(&["cache-*"]);
        assert!(is_ignored("cache-dir", &ex2));
        assert!(!is_ignored("my-cache-dir", &ex2));
    }
}
