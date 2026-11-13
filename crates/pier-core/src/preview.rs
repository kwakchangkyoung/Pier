//! Quick Look 미리보기용 로컬 캐시 (SPEC 6.4).
//!
//! 원격 객체를 `cache_root/previews/<키+ETag 해시>/<파일 이름>`에 내려받고,
//! 같은 ETag면 다운로드 없이 재사용한다. 접근 시각은 각 디렉터리의
//! `.access` 파일에 기록하고, previews 총 크기가 1GB를 넘으면 오래된
//! 것부터 지운다.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::backend::Backend;
use crate::enumerate::Connection;
use crate::error::Error;
use crate::key;
use crate::transfer::{CancelToken, ProgressSink, TransferOptions};

/// previews 캐시 총 크기 상한(바이트). 1GB.
const MAX_PREVIEW_BYTES: u64 = 1024 * 1024 * 1024;
/// 접근 시각(epoch ms)을 기록하는 파일 이름.
const ACCESS_FILE: &str = ".access";

/// 객체 하나를 미리보기용 로컬 파일로 준비하고 경로를 반환한다.
///
/// - 디렉터리 해시에 키+ETag를 넣어, 내용이 바뀐 객체는 새로 받는다.
/// - 캐시에 파일이 있으면 접근 시각만 갱신하고 그대로 재사용한다.
/// - 저장 후 previews 총합이 1GB를 넘으면 접근 시각이 오래된 것부터
///   지운다(방금 받은 항목은 지우지 않는다).
/// - 폴더 키는 Error::Other다.
pub async fn preview_file<B>(
    conn: &Connection<B>,
    key: &str,
    cache_root: &Path,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<PathBuf, Error>
where
    B: Backend + Clone + Send + Sync + 'static,
{
    if key::is_folder(key) {
        return Err(Error::Other("폴더는 미리보기할 수 없다".to_string()));
    }
    let item = conn.stat(key).await?;
    if item.is_dir {
        return Err(Error::Other("폴더는 미리보기할 수 없다".to_string()));
    }

    let dir = preview_dir(cache_root, key, item.etag.as_deref());
    let path = dir.join(safe_file_name(&item.name));
    if path.is_file() {
        touch_access(&dir)?;
        // 키는 로그에 남기지 않고 크기만 기록한다(SPEC 4.6).
        log::info!("미리보기 캐시를 재사용한다 ({}바이트)", item.size);
        return Ok(path);
    }

    std::fs::create_dir_all(&dir)?;
    conn.download(key, &path, &TransferOptions::default(), progress, cancel)
        .await?;
    touch_access(&dir)?;
    log::info!("미리보기 파일을 받았다 ({}바이트)", item.size);

    let root = previews_root(cache_root);
    let freed = enforce_limit(&root, MAX_PREVIEW_BYTES, &dir)?;
    if freed > 0 {
        log::info!("미리보기 캐시 {freed}바이트를 정리했다");
    }
    Ok(path)
}

/// previews 루트 디렉터리 경로.
fn previews_root(cache_root: &Path) -> PathBuf {
    cache_root.join("previews")
}

/// 키+ETag를 해시한 미리보기 디렉터리 경로.
/// sha 계열을 쓰지 않고 std 해셔로 디렉터리 이름을 만든다 — 충돌이 나면
/// 잘못된 파일을 보여줄 수 있으니 DefaultHasher의 64비트를 쓴다.
fn preview_dir(cache_root: &Path, key: &str, etag: Option<&str>) -> PathBuf {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    etag.unwrap_or_default().hash(&mut hasher);
    previews_root(cache_root).join(format!("{:016x}", hasher.finish()))
}

/// 경로 이탈을 막기 위해 위험한 파일 이름을 치환한다.
fn safe_file_name(name: &str) -> &str {
    match name {
        "" | "." | ".." => "preview",
        _ => name,
    }
}

/// 미리보기 디렉터리의 접근 시각을 지금으로 기록한다.
fn touch_access(dir: &Path) -> Result<(), Error> {
    std::fs::write(dir.join(ACCESS_FILE), now_ms().to_string())?;
    Ok(())
}

/// previews_root 아래 총 크기가 max_bytes를 넘으면 접근 시각이 오래된
/// 디렉터리부터 지운다. keep 디렉터리는 삭제하지 않는다(초과가 남아도
/// 그대로 둔다). 지운 바이트 수를 반환한다.
fn enforce_limit(previews_root: &Path, max_bytes: u64, keep: &Path) -> Result<u64, Error> {
    if !previews_root.is_dir() {
        return Ok(0);
    }
    // (접근 시각, 크기, 경로)
    let mut entries: Vec<(i64, u64, PathBuf)> = Vec::new();
    let mut total = 0u64;
    for entry in std::fs::read_dir(previews_root)? {
        let dir = entry?.path();
        if !dir.is_dir() {
            continue;
        }
        let size = dir_size(&dir)?;
        total += size;
        entries.push((read_access(&dir), size, dir));
    }
    if total <= max_bytes {
        return Ok(0);
    }

    // 접근 시각 오름차순 — 오래 쓰지 않은 것부터 지운다.
    entries.sort_by_key(|(accessed, _, _)| *accessed);
    let mut freed = 0u64;
    for (_, size, dir) in entries {
        if total <= max_bytes {
            break;
        }
        if dir == keep {
            continue;
        }
        std::fs::remove_dir_all(&dir)?;
        total = total.saturating_sub(size);
        freed += size;
    }
    Ok(freed)
}

/// 디렉터리 바로 아래 파일들의 크기 합.
fn dir_size(dir: &Path) -> Result<u64, Error> {
    let mut size = 0u64;
    for entry in std::fs::read_dir(dir)? {
        let meta = entry?.metadata()?;
        if meta.is_file() {
            size += meta.len();
        }
    }
    Ok(size)
}

/// `.access` 파일의 접근 시각. 없거나 깨졌으면 가장 오래된 것(0)으로 취급한다.
fn read_access(dir: &Path) -> i64 {
    std::fs::read_to_string(dir.join(ACCESS_FILE))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// 현재 시각의 Unix epoch 밀리초.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::Item;
    use crate::metadata::MetadataStore;

    /// 최소 가짜 백엔드. stat은 항상 NotFound다.
    #[derive(Clone)]
    struct FakeBackend;

    impl Backend for FakeBackend {
        async fn list_folder(&self, _prefix: &str) -> Result<Vec<Item>, Error> {
            Ok(vec![])
        }

        async fn list_recursive(&self, _prefix: &str) -> Result<Vec<Item>, Error> {
            Ok(vec![])
        }

        async fn stat(&self, key: &str) -> Result<Item, Error> {
            Err(Error::NotFound(key.to_string()))
        }
    }

    struct Nop;
    impl ProgressSink for Nop {
        fn on_progress(&self, _transferred: u64, _total: u64) {}
    }

    /// 테스트용 임시 루트. Drop에서 지운다.
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(tag: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("pier-preview-ut-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// size 바이트 파일과 주어진 접근 시각의 .access를 가진 항목을 만든다.
    /// 각 항목의 디스크 사용량은 size + access 문자열 길이다.
    fn make_entry(previews: &Path, name: &str, size: u64, accessed_ms: i64) -> PathBuf {
        let dir = previews.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f"), vec![0u8; size as usize]).unwrap();
        std::fs::write(dir.join(ACCESS_FILE), accessed_ms.to_string()).unwrap();
        dir
    }

    #[test]
    fn preview_dir_is_deterministic_and_etag_sensitive() {
        let root = Path::new("/cache");
        let a = preview_dir(root, "docs/a.jpg", Some("e1"));
        // 같은 입력은 같은 경로다.
        assert_eq!(a, preview_dir(root, "docs/a.jpg", Some("e1")));
        // ETag가 바뀌면 다른 디렉터리다(내용이 바뀐 객체는 새로 받는다).
        assert_ne!(a, preview_dir(root, "docs/a.jpg", Some("e2")));
        // 다른 키는 다른 디렉터리다.
        assert_ne!(a, preview_dir(root, "docs/b.jpg", Some("e1")));
        // previews 아래에 만든다.
        assert!(a.starts_with(root.join("previews")));
    }

    #[test]
    fn safe_file_name_cases() {
        assert_eq!(safe_file_name("a.jpg"), "a.jpg");
        assert_eq!(safe_file_name("한글 파일.txt"), "한글 파일.txt");
        // 경로 이탈 위험이 있는 이름은 치환한다.
        assert_eq!(safe_file_name(".."), "preview");
        assert_eq!(safe_file_name("."), "preview");
        assert_eq!(safe_file_name(""), "preview");
    }

    #[tokio::test]
    async fn folder_key_is_rejected() {
        let conn = Connection::with_backend(FakeBackend, MetadataStore::open_in_memory().unwrap());
        let err = preview_file(&conn, "dir/", Path::new("/tmp"), &Nop, &CancelToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Other(_)), "{err:?}");
    }

    #[tokio::test]
    async fn missing_key_propagates_not_found() {
        let conn = Connection::with_backend(FakeBackend, MetadataStore::open_in_memory().unwrap());
        let err = preview_file(
            &conn,
            "missing.txt",
            Path::new("/tmp"),
            &Nop,
            &CancelToken::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, Error::NotFound(_)), "{err:?}");
    }

    #[test]
    fn enforce_limit_evicts_oldest_first() {
        let tmp = TempRoot::new("evict");
        let previews = tmp.0.join("previews");
        std::fs::create_dir_all(&previews).unwrap();
        // 각 항목 크기 = 파일 60 + .access 3 = 63바이트.
        let keep = make_entry(&previews, "c", 60, 300);
        make_entry(&previews, "b", 60, 200);
        make_entry(&previews, "a", 60, 100);

        // 총 189 > 120 → 가장 오래된 a(63)만 지우면 126 > 120이라
        // b까지 지워 63이 된다.
        let freed = enforce_limit(&previews, 120, &keep).unwrap();
        assert_eq!(freed, 126);
        assert!(!previews.join("a").exists());
        assert!(!previews.join("b").exists());
        assert!(keep.exists());
    }

    #[test]
    fn enforce_limit_keeps_just_downloaded() {
        let tmp = TempRoot::new("keep");
        let previews = tmp.0.join("previews");
        std::fs::create_dir_all(&previews).unwrap();
        let keep = make_entry(&previews, "new", 60, 300);
        make_entry(&previews, "old1", 60, 100);
        make_entry(&previews, "old2", 60, 200);

        // 상한 0 → keep을 빼고 다 지운다(초과가 남아도 keep은 살아 있다).
        let freed = enforce_limit(&previews, 0, &keep).unwrap();
        assert_eq!(freed, 126);
        assert!(!previews.join("old1").exists());
        assert!(!previews.join("old2").exists());
        assert!(keep.exists());
    }

    #[test]
    fn enforce_limit_noop_when_under() {
        let tmp = TempRoot::new("under");
        let previews = tmp.0.join("previews");
        std::fs::create_dir_all(&previews).unwrap();
        make_entry(&previews, "a", 10, 100);
        let freed = enforce_limit(&previews, 1_000_000, &previews.join("a")).unwrap();
        assert_eq!(freed, 0);
        assert!(previews.join("a").exists());
        // previews 루트 자체가 없어도 오류가 아니다.
        assert_eq!(
            enforce_limit(&tmp.0.join("none"), 0, Path::new("")).unwrap(),
            0
        );
    }
}
