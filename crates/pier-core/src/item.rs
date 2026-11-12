//! 버킷 안 아이템(파일/폴더)의 공통 표현.
//! parent/name은 항상 key에서 계산하며, 이후 단계(목록·캐시·전송)가 이 타입을 공유한다.

use crate::key;

/// 목록·캐시·전송이 공유하는 아이템 모델.
/// key는 루트 기준 상대 S3 키이며, 폴더는 끝에 '/'가 붙는다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// 루트 기준 상대 키. 폴더는 끝 '/', 루트는 "".
    pub key: String,
    /// `key::parent(key)`와 같다.
    pub parent: String,
    /// `key::name(key)`와 같다. 끝 '/' 제외.
    pub name: String,
    pub is_dir: bool,
    /// 바이트 크기. 폴더는 0.
    pub size: u64,
    /// 불투명 문자열. 따옴표 포함 여부는 백엔드 원문 그대로.
    pub etag: Option<String>,
    /// Unix epoch 밀리초.
    pub last_modified_ms: Option<i64>,
    /// Phase 1에서는 항상 None.
    pub storage_class: Option<String>,
}

impl Item {
    /// 파일 아이템을 만든다. parent/name은 key에서 자동 계산한다.
    pub fn file(
        key: impl Into<String>,
        size: u64,
        etag: Option<String>,
        last_modified_ms: Option<i64>,
    ) -> Item {
        build(key.into(), false, size, etag, last_modified_ms, None)
    }

    /// 폴더 아이템을 만든다. key는 '/'로 끝나야 한다(루트 ""도 허용).
    /// 크기는 0이고 etag/last_modified는 설정하지 않는다.
    pub fn folder(key: impl Into<String>) -> Item {
        let key = key.into();
        debug_assert!(
            key::is_folder(&key),
            "폴더 키는 '/'로 끝나거나 루트(\"\")여야 한다"
        );
        build(key, true, 0, None, None, None)
    }

    /// 콘텐츠 버전. ETag를 그대로 반환한다(불투명 문자열, MD5로 해석하지 않음).
    pub fn content_version(&self) -> Option<&str> {
        self.etag.as_deref()
    }

    /// 메타데이터 버전. `etag|last_modified_ms` 형식이며, 없는 값은 빈칸으로 둔다.
    /// 같은 키가 다른 클라이언트에 의해 바뀌었는지 감지하는 데 쓴다(SPEC 3.4).
    pub fn metadata_version(&self) -> String {
        let etag = self.etag.as_deref().unwrap_or("");
        let last_modified = self
            .last_modified_ms
            .map(|ms| ms.to_string())
            .unwrap_or_default();
        format!("{etag}|{last_modified}")
    }
}

/// 공통 필드를 채우는 내부 생성자. parent/name은 항상 key에서 계산한다.
fn build(
    key: String,
    is_dir: bool,
    size: u64,
    etag: Option<String>,
    last_modified_ms: Option<i64>,
    storage_class: Option<String>,
) -> Item {
    Item {
        parent: key::parent(&key).to_string(),
        name: key::name(&key).to_string(),
        key,
        is_dir,
        size,
        etag,
        last_modified_ms,
        storage_class,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_computes_parent_and_name() {
        let item = Item::file(
            "photos/2024/a.jpg",
            100,
            Some("\"abc123\"".to_string()),
            Some(1_700_000_000_000),
        );
        assert_eq!(item.key, "photos/2024/a.jpg");
        assert_eq!(item.parent, "photos/2024/");
        assert_eq!(item.name, "a.jpg");
        assert!(!item.is_dir);
        assert_eq!(item.size, 100);
        assert_eq!(item.etag.as_deref(), Some("\"abc123\""));
        assert_eq!(item.last_modified_ms, Some(1_700_000_000_000));
        assert_eq!(item.storage_class, None);
    }

    #[test]
    fn file_at_root_and_special_names() {
        let root_file = Item::file("a.jpg", 0, None, None);
        assert_eq!(root_file.parent, "");
        assert_eq!(root_file.name, "a.jpg");

        let korean = Item::file("사진/2024/한글 파일.txt", 10, None, None);
        assert_eq!(korean.parent, "사진/2024/");
        assert_eq!(korean.name, "한글 파일.txt");

        let hidden = Item::file(".hidden", 1, None, None);
        assert_eq!(hidden.parent, "");
        assert_eq!(hidden.name, ".hidden");
    }

    #[test]
    fn folder_has_zero_size_and_empty_version_fields() {
        let item = Item::folder("photos/2024/");
        assert_eq!(item.key, "photos/2024/");
        assert_eq!(item.parent, "photos/");
        assert_eq!(item.name, "2024");
        assert!(item.is_dir);
        assert_eq!(item.size, 0);
        assert_eq!(item.etag, None);
        assert_eq!(item.last_modified_ms, None);
        assert_eq!(item.storage_class, None);
    }

    #[test]
    fn folder_root() {
        let root = Item::folder("");
        assert_eq!(root.key, "");
        assert_eq!(root.parent, "");
        assert_eq!(root.name, "");
        assert!(root.is_dir);
    }

    #[test]
    fn folder_with_empty_name_segment() {
        // `a//`는 `a/` 안의 빈 이름 폴더다.
        let item = Item::folder("a//");
        assert_eq!(item.parent, "a/");
        assert_eq!(item.name, "");
        assert!(item.is_dir);
    }

    #[test]
    fn content_version_is_etag() {
        let item = Item::file("a.jpg", 1, Some("etag1".to_string()), None);
        assert_eq!(item.content_version(), Some("etag1"));

        let no_etag = Item::file("b.jpg", 1, None, None);
        assert_eq!(no_etag.content_version(), None);
    }

    #[test]
    fn metadata_version_format() {
        let both = Item::file("a.jpg", 1, Some("etag1".to_string()), Some(1234));
        assert_eq!(both.metadata_version(), "etag1|1234");

        let no_modified = Item::file("b.jpg", 1, Some("etag2".to_string()), None);
        assert_eq!(no_modified.metadata_version(), "etag2|");

        let only_modified = Item::file("c.jpg", 1, None, Some(99));
        assert_eq!(only_modified.metadata_version(), "|99");

        let neither = Item::file("d.jpg", 1, None, None);
        assert_eq!(neither.metadata_version(), "|");
    }
}
