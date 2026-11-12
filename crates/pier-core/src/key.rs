//! S3 키 ↔ 경로/부모/이름 변환 유틸.
//!
//! S3 키는 '/'로 구분되며, 연속 슬래시(`a//b`)나 점으로 시작하는 이름, 비ASCII
//! 문자도 모두 유효한 키이므로 절대 정규화하거나 축약하지 않는다.

/// 폴더 키인지 판별한다. 폴더는 끝이 '/'이거나 루트(빈 문자열)다.
pub fn is_folder(key: &str) -> bool {
    key.is_empty() || key.ends_with('/')
}

/// key의 부모 키를 반환한다. 반환값도 폴더 키(끝 '/') 또는 루트("")다.
///
/// - `photos/2024/a.jpg` → `photos/2024/`
/// - `photos/` → ``
pub fn parent(key: &str) -> &str {
    // 끝의 폴더 마커 '/'는 하나만 제거한다. `a//`는 `a/` 안의 빈 이름 폴더이므로
    // 전부 지우면(`trim_end_matches`) 부모가 ""로 잘못 계산된다.
    // 이렇게 해야 `photos/2024/`의 부모가 자기 자신이 아니라 `photos/`가 된다.
    let trimmed = key.strip_suffix('/').unwrap_or(key);
    match trimmed.rfind('/') {
        Some(i) => &trimmed[..=i],
        None => "",
    }
}

/// key의 마지막 구간(이름)을 반환한다. 끝 '/'는 제외한다. 루트는 빈 문자열.
pub fn name(key: &str) -> &str {
    let trimmed = key.strip_suffix('/').unwrap_or(key);
    match trimmed.rfind('/') {
        Some(i) => &trimmed[i + 1..],
        None => trimmed,
    }
}

/// parent 아래에 name을 붙인 자식 키를 만든다.
/// parent는 폴더 키(끝 '/' 또는 루트 "")여야 하고, name에 '/'가 없어야 한다.
pub fn child_key(parent: &str, name: &str, is_dir: bool) -> String {
    debug_assert!(is_folder(parent), "parent는 폴더 키여야 한다");
    debug_assert!(!name.contains('/'), "name에 '/'가 있으면 안 된다");
    let mut key = String::with_capacity(parent.len() + name.len() + 1);
    key.push_str(parent);
    key.push_str(name);
    if is_dir {
        key.push('/');
    }
    key
}

/// 접두사를 정규화한다: 앞의 '/'를 제거하고, 비어 있지 않으면 끝에 '/'를 보장한다.
///
/// - `"photos"` → `"photos/"`
/// - `"/a/b"` → `"a/b/"`
/// - `""` → `""`
pub fn normalize_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim_start_matches('/');
    if trimmed.is_empty() || trimmed.ends_with('/') {
        trimmed.to_string()
    } else {
        format!("{trimmed}/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_folder_cases() {
        assert!(is_folder(""));
        assert!(is_folder("photos/"));
        assert!(is_folder("a//"));
        assert!(!is_folder("a.jpg"));
        assert!(!is_folder("photos/2024"));
    }

    #[test]
    fn parent_cases() {
        assert_eq!(parent("photos/2024/a.jpg"), "photos/2024/");
        assert_eq!(parent("photos/"), "");
        assert_eq!(parent("photos/2024/"), "photos/");
        assert_eq!(parent("a.jpg"), "");
        assert_eq!(parent(""), "");
        // 한글·공백이 섞인 키도 그대로 다룬다.
        assert_eq!(parent("사진/2024/한글 파일.txt"), "사진/2024/");
        // 연속 슬래시는 S3에서 유효한 키이므로 그대로 보존한다.
        assert_eq!(parent("a//b.txt"), "a//");
        // `a//`는 `a/` 안의 빈 이름 폴더다. 끝 '/'는 하나만 제거한다.
        assert_eq!(parent("a//"), "a/");
        assert_eq!(parent("a///"), "a//");
        assert_eq!(parent("a///b.txt"), "a///");
    }

    #[test]
    fn name_cases() {
        assert_eq!(name("photos/2024/a.jpg"), "a.jpg");
        assert_eq!(name("photos/"), "photos");
        assert_eq!(name("photos/2024/"), "2024");
        assert_eq!(name(""), "");
        assert_eq!(name("a.jpg"), "a.jpg");
        assert_eq!(name("사진/2024/한글 파일.txt"), "한글 파일.txt");
        assert_eq!(name("a//b.txt"), "b.txt");
        assert_eq!(name("a//"), "");
        assert_eq!(name("a///"), "");
        assert_eq!(name("a///b.txt"), "b.txt");
    }

    #[test]
    fn name_special_char_cases() {
        assert_eq!(
            name("with space & special(1).txt"),
            "with space & special(1).txt"
        );
        assert_eq!(name("a+b=c.txt"), "a+b=c.txt");
        assert_eq!(name("%20"), "%20");
        assert_eq!(name("사진/📷.jpg"), "📷.jpg");
        assert_eq!(name(".hidden"), ".hidden");
        assert_eq!(parent(".hidden"), "");
        assert_eq!(parent("사진/📷.jpg"), "사진/");
    }

    #[test]
    fn child_key_cases() {
        assert_eq!(child_key("", "a.jpg", false), "a.jpg");
        assert_eq!(child_key("photos/", "a.jpg", false), "photos/a.jpg");
        assert_eq!(child_key("photos/", "2024", true), "photos/2024/");
        assert_eq!(child_key("", "사진", true), "사진/");
        assert_eq!(child_key("a//", "b.txt", false), "a//b.txt");
    }

    #[test]
    fn normalize_prefix_cases() {
        assert_eq!(normalize_prefix(""), "");
        assert_eq!(normalize_prefix("/"), "");
        assert_eq!(normalize_prefix("photos"), "photos/");
        assert_eq!(normalize_prefix("photos/"), "photos/");
        assert_eq!(normalize_prefix("/a/b"), "a/b/");
        assert_eq!(normalize_prefix("a/b/"), "a/b/");
        assert_eq!(normalize_prefix("///a///b"), "a///b/");
    }
}
