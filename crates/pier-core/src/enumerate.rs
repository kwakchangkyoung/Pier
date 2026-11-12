//! 폴더 목록 조회와 SQLite 캐시 기반 페이지네이션 (SPEC 4.2, 4.3).
//!
//! 원격에서는 폴더 바로 아래 목록을 한 번에 받아 캐시에 저장하고,
//! FFI/CLI에는 캐시에서 키 순으로 PAGE_SIZE개씩 나눈 페이지를 돌려준다.
//! S3 응답 경계를 넘는 부분 읽기는 OpenDAL 목록 순서 때문에 항목이
//! 누락될 수 있어 쓰지 않는다.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::SystemTime;

use crate::backend::opendal_s3::OpendalS3;
use crate::backend::Backend;
use crate::config::ConnectionConfig;
use crate::error::Error;
use crate::item::Item;
use crate::key;
use crate::metadata::MetadataStore;

/// 캐시에서 한 페이지에 담는 최대 항목 수(SPEC 4.3).
pub const PAGE_SIZE: usize = 1000;

/// list_folder 한 페이지 결과.
/// `from_cache`는 이번 호출이 캐시에서 응답했는지 여부다.
/// `next_cursor`는 마지막으로 반환한 항목의 key이며 다음 호출에 그대로 넘긴다.
#[derive(Debug)]
pub struct ListResult {
    pub items: Vec<Item>,
    pub next_cursor: Option<String>,
    pub from_cache: bool,
}

/// 연결 하나의 목록/메타데이터 접근점.
/// 캐시 사용 여부 판단과 페이지 분할은 여기서, 네트워크는 Backend가 담당한다.
pub struct Connection<B: Backend = OpendalS3> {
    backend: B,
    store: Mutex<MetadataStore>,
}

impl Connection<OpendalS3> {
    /// cache_dir은 연결별 디렉터리. DB 경로는 cache_dir/metadata.sqlite.
    pub fn open(
        config: &ConnectionConfig,
        secret_access_key: &str,
        cache_dir: &Path,
    ) -> Result<Self, Error> {
        let backend = OpendalS3::new(config, secret_access_key)?;
        let store = MetadataStore::open(&cache_dir.join("metadata.sqlite"))?;
        Ok(Self {
            backend,
            store: Mutex::new(store),
        })
    }
}

impl<B: Backend> Connection<B> {
    /// 임의의 Backend와 MetadataStore를 주입한다. 테스트용.
    pub fn with_backend(backend: B, store: MetadataStore) -> Self {
        Self {
            backend,
            store: Mutex::new(store),
        }
    }

    /// prefix 폴더의 항목을 키 순으로 한 페이지 반환한다.
    ///
    /// 캐시에 complete 목록이 있으면 캐시에서 바로 응답한다(`from_cache=true`).
    /// 아니면 원격에서 폴더 전체를 받아 캐시에 저장한 뒤 캐시에서 페이지를
    /// 잘라 응답한다(`from_cache=false`). 커서가 주어졌는데 캐시가 complete가
    /// 아니면 원격 목록부터 다시 받는다.
    ///
    /// Phase 1은 캐시가 있으면 갱신하지 않는다(백그라운드 갱신은 Phase 2).
    pub async fn list_folder(
        &self,
        prefix: &str,
        cursor: Option<&str>,
    ) -> Result<ListResult, Error> {
        let prefix = key::normalize_prefix(prefix);
        let opened_at_ms = now_ms();

        // complete 목록이 캐시에 있는지 짧게 잠가 확인한다.
        let complete = {
            let store = self.lock()?;
            store
                .folder_state(&prefix)?
                .is_some_and(|state| state.complete)
        };

        if !complete {
            // 원격 전체 목록 → 캐시 저장. 잠금은 네트워크 호출 동안 놓는다.
            let items = self.backend.list_folder(&prefix).await?;
            let listing_started_at_ms = now_ms();
            let mut store = self.lock()?;
            store.store_page(&prefix, &items, listing_started_at_ms)?;
            store.finish_listing(&prefix, listing_started_at_ms)?;
        }

        // 페이지는 캐시에서 로컬로 나눈다. 다음 페이지 존재 여부를 알기 위해
        // PAGE_SIZE + 1개를 받는다.
        let mut store = self.lock()?;
        store.mark_opened(&prefix, opened_at_ms)?;
        let page = store.cached_page(&prefix, cursor, PAGE_SIZE + 1)?;
        let has_more = page.len() > PAGE_SIZE;
        let items: Vec<Item> = page.into_iter().take(PAGE_SIZE).collect();
        let next_cursor = if has_more {
            items.last().map(|item| item.key.clone())
        } else {
            None
        };
        Ok(ListResult {
            items,
            next_cursor,
            from_cache: complete,
        })
    }

    /// 단일 아이템 메타데이터. 캐시에 있으면 캐시, 없으면 원격.
    pub async fn stat(&self, key: &str) -> Result<Item, Error> {
        {
            let store = self.lock()?;
            if let Some(item) = store.get_item(key)? {
                return Ok(item);
            }
        }
        self.backend.stat(key).await
    }

    /// 객체를 dest 파일로 다운로드하고 쓴 바이트 수를 반환한다.
    pub async fn download(&self, key: &str, dest: &Path) -> Result<u64, Error> {
        self.backend.read_to_file(key, dest).await
    }

    /// 뮤텍스 잠금. 포이즌(잠금 보유 중 패닉)이면 다른 오류로 변환한다.
    fn lock(&self) -> Result<MutexGuard<'_, MetadataStore>, Error> {
        self.store
            .lock()
            .map_err(|_| Error::Other("메타데이터 캐시 잠금이 깨졌다".to_string()))
    }
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
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// list_folder 호출 횟수를 세는 가짜 백엔드.
    #[derive(Clone, Default)]
    struct FakeBackend {
        folders: Arc<HashMap<String, Vec<Item>>>,
        list_calls: Arc<AtomicUsize>,
    }

    impl FakeBackend {
        fn with_folder(mut self, prefix: &str, items: Vec<Item>) -> Self {
            Arc::make_mut(&mut self.folders).insert(prefix.to_string(), items);
            self
        }

        fn calls(&self) -> usize {
            self.list_calls.load(Ordering::SeqCst)
        }
    }

    impl Backend for FakeBackend {
        async fn list_folder(&self, prefix: &str) -> Result<Vec<Item>, Error> {
            self.list_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.folders.get(prefix).cloned().unwrap_or_default())
        }

        async fn stat(&self, key: &str) -> Result<Item, Error> {
            Err(Error::NotFound(key.to_string()))
        }

        async fn read_to_file(&self, _key: &str, _dest: &Path) -> Result<u64, Error> {
            Err(Error::Other("fake backend".to_string()))
        }
    }

    fn file_items(prefix: &str, count: usize) -> Vec<Item> {
        (0..count)
            .map(|i| Item::file(format!("{prefix}f{i:04}.txt"), i as u64, None, None))
            .collect()
    }

    /// prefix 아래 모든 페이지를 순회해 (페이지 크기 목록, 전체 키)를 모은다.
    async fn collect_all<B: Backend>(
        conn: &Connection<B>,
        prefix: &str,
    ) -> (Vec<usize>, Vec<String>, Vec<bool>) {
        let mut page_sizes = Vec::new();
        let mut keys = Vec::new();
        let mut cache_flags = Vec::new();
        let mut cursor = None;
        loop {
            let page = conn.list_folder(prefix, cursor.as_deref()).await.unwrap();
            page_sizes.push(page.items.len());
            keys.extend(page.items.iter().map(|i| i.key.clone()));
            cache_flags.push(page.from_cache);
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
            assert!(page_sizes.len() < 20, "페이지가 너무 많다 — 커서 진행 이상");
        }
        (page_sizes, keys, cache_flags)
    }

    #[tokio::test]
    async fn pages_2503_items_in_1000_1000_503() {
        let backend = FakeBackend::default().with_folder("big/", file_items("big/", 2503));
        let probe = backend.clone();
        let conn = Connection::with_backend(backend, MetadataStore::open_in_memory().unwrap());

        let (sizes, keys, flags) = collect_all(&conn, "big/").await;
        assert_eq!(sizes, [1000, 1000, 503]);
        assert_eq!(keys.len(), 2503);
        assert_eq!(flags, [false, true, true]);

        // 중복 없이 키 오름차순.
        let unique: std::collections::HashSet<&String> = keys.iter().collect();
        assert_eq!(unique.len(), keys.len());
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(keys, sorted);

        // 첫 호출 이후 백엔드는 한 번만 불렸다.
        assert_eq!(probe.calls(), 1);
    }

    #[tokio::test]
    async fn second_traversal_is_from_cache() {
        let backend = FakeBackend::default().with_folder("d/", file_items("d/", 5));
        let probe = backend.clone();
        let conn = Connection::with_backend(backend, MetadataStore::open_in_memory().unwrap());

        let (_, keys1, _) = collect_all(&conn, "d/").await;
        let (sizes2, keys2, flags2) = collect_all(&conn, "d/").await;
        assert_eq!(keys1, keys2);
        assert_eq!(sizes2, [5]);
        assert_eq!(flags2, [true]);
        assert_eq!(probe.calls(), 1);
    }

    #[tokio::test]
    async fn cursor_boundary_exactly_1000() {
        let backend = FakeBackend::default().with_folder("e/", file_items("e/", 1000));
        let conn = Connection::with_backend(backend, MetadataStore::open_in_memory().unwrap());
        let (sizes, keys, _) = collect_all(&conn, "e/").await;
        // 정확히 1000개면 다음 페이지가 없으므로 한 페이지로 끝난다.
        assert_eq!(sizes, [1000]);
        assert_eq!(keys.len(), 1000);
    }

    #[tokio::test]
    async fn cursor_boundary_1001() {
        let backend = FakeBackend::default().with_folder("f/", file_items("f/", 1001));
        let conn = Connection::with_backend(backend, MetadataStore::open_in_memory().unwrap());
        let (sizes, keys, _) = collect_all(&conn, "f/").await;
        assert_eq!(sizes, [1000, 1]);
        assert_eq!(keys.len(), 1001);
    }

    #[tokio::test]
    async fn cursor_boundary_empty_folder() {
        let backend = FakeBackend::default();
        let conn = Connection::with_backend(backend, MetadataStore::open_in_memory().unwrap());
        let page = conn.list_folder("empty/", None).await.unwrap();
        assert!(page.items.is_empty());
        assert!(page.next_cursor.is_none());
        assert!(!page.from_cache);
        // 두 번째 호출은 캐시에서.
        let page = conn.list_folder("empty/", None).await.unwrap();
        assert!(page.from_cache);
    }

    #[tokio::test]
    async fn korean_and_special_keys_sorted() {
        let items = vec![
            Item::file("dir/📷.jpg", 1, None, None),
            Item::file("dir/with space & special(1).txt", 1, None, None),
            Item::file("dir/한글 파일.txt", 1, None, None),
            Item::file("dir/a+b=c.txt", 1, None, None),
            Item::file("dir/%20", 1, None, None),
            Item::file("dir/.hidden", 1, None, None),
            Item::folder("dir/sub/"),
        ];
        let backend = FakeBackend::default().with_folder("dir/", items);
        let conn = Connection::with_backend(backend, MetadataStore::open_in_memory().unwrap());
        let page = conn.list_folder("dir/", None).await.unwrap();
        let keys: Vec<&str> = page.items.iter().map(|i| i.key.as_str()).collect();
        let mut expected: Vec<String> = [
            "dir/.hidden",
            "dir/%20",
            "dir/a+b=c.txt",
            "dir/sub/",
            "dir/with space & special(1).txt",
            "dir/한글 파일.txt",
            "dir/📷.jpg",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        expected.sort_unstable();
        assert_eq!(
            keys,
            expected.iter().map(String::as_str).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn stat_uses_cache_after_listing() {
        let items = file_items("s/", 3);
        let backend = FakeBackend::default().with_folder("s/", items);
        let conn = Connection::with_backend(backend, MetadataStore::open_in_memory().unwrap());
        conn.list_folder("s/", None).await.unwrap();
        // 목록으로 채워진 캐시에서 stat이 응답한다(가짜 stat은 항상 NotFound).
        let item = conn.stat("s/f0001.txt").await.unwrap();
        assert_eq!(item.key, "s/f0001.txt");
        assert_eq!(item.size, 1);
    }

    #[tokio::test]
    async fn prefix_is_normalized() {
        // 끝 '/' 없는 prefix도 같은 폴더를 가리킨다.
        let backend = FakeBackend::default().with_folder("n/", file_items("n/", 2));
        let conn = Connection::with_backend(backend, MetadataStore::open_in_memory().unwrap());
        let page = conn.list_folder("n", None).await.unwrap();
        assert_eq!(page.items.len(), 2);
    }
}
