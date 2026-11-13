//! 폴더 목록 조회와 SQLite 캐시 기반 페이지네이션 (SPEC 4.2, 4.3, 3.5).
//!
//! 원격에서는 폴더 바로 아래 목록을 한 번에 받아 캐시에 저장하고,
//! FFI/CLI에는 캐시에서 키 순으로 PAGE_SIZE개씩 나눈 페이지를 돌려준다.
//! S3 응답 경계를 넘는 부분 읽기는 OpenDAL 목록 순서 때문에 항목이
//! 누락될 수 있어 쓰지 않는다.
//!
//! 캐시가 complete면 list_folder는 캐시로 즉시 응답하고, 백그라운드
//! 갱신은 refresh가 원격 목록을 다시 받아 캐시를 한 트랜잭션으로 교체한다.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;

use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::backend::opendal_s3::OpendalS3;
use crate::backend::Backend;
use crate::config::ConnectionConfig;
use crate::error::Error;
use crate::item::Item;
use crate::key;
pub use crate::metadata::Changes;
use crate::metadata::MetadataStore;
use crate::transfer::{
    CancelToken, FolderDownload, FolderUpload, ProgressSink, TransferOptions, UploadMode,
};

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
    /// Arc인 이유: S3Raw가 미완료 멀티파트(pending_uploads)를
    /// 기록·정리할 때 같은 저장소를 공유한다.
    store: Arc<Mutex<MetadataStore>>,
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
        log::info!("연결을 열었다 (provider: {:?})", config.provider);
        Ok(Self {
            backend,
            store: Arc::new(Mutex::new(store)),
        })
    }
}

impl<B: Backend> Connection<B> {
    /// 임의의 Backend와 MetadataStore를 주입한다. 테스트용.
    pub fn with_backend(backend: B, store: MetadataStore) -> Self {
        Self {
            backend,
            store: Arc::new(Mutex::new(store)),
        }
    }

    /// 메타데이터 캐시 저장소를 공유한다.
    /// S3Raw의 pending_uploads 기록·정리처럼 저장소를 직접 써야 할 때 쓴다.
    pub fn metadata_store(&self) -> Arc<Mutex<MetadataStore>> {
        Arc::clone(&self.store)
    }

    /// 백엔드 접근자. presign처럼 캐시와 무관한 원격 호출에 쓴다.
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// prefix 폴더의 항목을 키 순으로 한 페이지 반환한다.
    ///
    /// 캐시에 complete 목록이 있으면 캐시에서 바로 응답한다(`from_cache=true`).
    /// 아니면 원격에서 폴더 전체를 받아 캐시에 저장한 뒤 캐시에서 페이지를
    /// 잘라 응답한다(`from_cache=false`). 커서가 주어졌는데 캐시가 complete가
    /// 아니면 원격 목록부터 다시 받는다.
    ///
    /// complete 캐시를 그대로 쓰므로 백그라운드 갱신은 호출자가
    /// `refresh`로 따로 실행한다(SPEC 3.5).
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
            // 원격 전체 목록 → 캐시 교체. 잠금은 네트워크 호출 동안 놓는다.
            let items = self.backend.list_folder(&prefix).await?;
            let mut store = self.lock()?;
            store.replace_folder(&prefix, &items, now_ms())?;
            // 키·시크릿은 로그에 남기지 않고 개수만 기록한다(SPEC 4.6).
            log::info!("폴더 목록을 원격에서 받았다 (항목 {}개)", items.len());
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

    /// 폴더를 원격에서 다시 읽어 캐시를 교체하고 변경 사항을 반환한다.
    ///
    /// 잠금은 네트워크 호출 동안 잡지 않으므로 갱신이 진행되는 동안에도
    /// list_folder는 이전 캐시로 즉시 응답한다(SPEC 3.5). 캐시 교체는
    /// `replace_folder`의 한 트랜잭션이라 중간 상태가 보이지 않는다.
    pub async fn refresh(&self, prefix: &str) -> Result<Changes, Error> {
        let prefix = key::normalize_prefix(prefix);
        let items = self.backend.list_folder(&prefix).await?;
        let mut store = self.lock()?;
        let changes = store.replace_folder(&prefix, &items, now_ms())?;
        log::info!(
            "폴더를 갱신했다 (추가·변경 {}개, 삭제 {}개)",
            changes.upserted.len(),
            changes.removed.len()
        );
        Ok(changes)
    }

    /// 원격 루트 목록을 한 번 조회해 연결·인증·버킷 접근을 확인하고,
    /// 이어서 임시 객체 쓰기·삭제로 쓰기 권한을 판정한다(SPEC 3.1).
    /// 쓰기 권한이 없으면 Ok(false)이고, 그 외 오류는 Err로 전파한다.
    /// 캐시에는 아무것도 쓰지 않는다.
    pub async fn test_connection(&self) -> Result<bool, Error> {
        self.backend.list_folder("").await?;
        crate::ops::test_write(&self.backend).await
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

    /// parent 아래 names 중 원격에 이미 있는 항목(파일·폴더 모두,
    /// 이름은 끝 '/' 없이)을 반환한다. 없는 이름은 결과에서 빠진다.
    pub async fn existing(&self, parent: &str, names: &[String]) -> Result<Vec<Item>, Error>
    where
        B: Send + Sync,
    {
        crate::transfer::existing(&self.backend, parent, names).await
    }

    /// parent 아래 name으로 새 폴더를 만들고 캐시에 반영한다.
    /// 대상 이름이 이미 있으면 Error::Conflict.
    pub async fn create_folder(&self, parent: &str, name: &str) -> Result<Item, Error> {
        crate::ops::create_folder(&self.backend, &self.store, parent, name).await
    }

    /// 이름 변경·이동 공통. 대상이 이미 있으면 Error::Conflict.
    /// 폴더 키는 new_name에 '/' 없이 받아 코어가 붙인다.
    pub async fn rename(&self, key: &str, new_parent: &str, new_name: &str) -> Result<Item, Error> {
        crate::ops::rename(&self.backend, &self.store, key, new_parent, new_name).await
    }

    /// 삭제. 폴더 키(끝 '/')면 하위 객체 전체를 배치 삭제한다.
    pub async fn delete(&self, key: &str) -> Result<(), Error> {
        crate::ops::delete(&self.backend, &self.store, key).await
    }

    /// 중단된 폴더 이름 변경을 이어서 완료하고 처리 건수를 반환한다.
    pub async fn resume_pending_ops(&self) -> Result<u32, Error> {
        crate::ops::resume_pending_ops(&self.backend, &self.store).await
    }

    /// 뮤텍스 잠금. 포이즌(잠금 보유 중 패닉)이면 다른 오류로 변환한다.
    fn lock(&self) -> Result<MutexGuard<'_, MetadataStore>, Error> {
        self.store
            .lock()
            .map_err(|_| Error::Other("메타데이터 캐시 잠금이 깨졌다".to_string()))
    }
}

impl<B> Connection<B>
where
    B: Backend + Clone + Send + Sync + 'static,
{
    /// 객체를 dest 파일로 다운로드하고 쓴 바이트 수를 반환한다.
    /// 병렬 범위 다운로드·진행률·취소는 transfer::download가 담당한다(SPEC 4.4).
    pub async fn download(
        &self,
        key: &str,
        dest: &Path,
        opts: &TransferOptions,
        progress: &dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<u64, Error> {
        crate::transfer::download(&self.backend, key, dest, opts, progress, cancel).await
    }

    /// prefix 폴더의 하위 객체를 모두 dest_dir 아래 `<폴더 이름>`으로
    /// 다운로드한다. 루트("")는 거부한다. 세부 규칙은 transfer::download_folder 참조.
    pub async fn download_folder(
        &self,
        prefix: &str,
        dest_dir: &Path,
        opts: &TransferOptions,
        progress: &dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<FolderDownload, Error> {
        crate::transfer::download_folder(&self.backend, prefix, dest_dir, opts, progress, cancel)
            .await
    }

    /// src 파일을 parent 아래 name으로 업로드하고 캐시에 반영한다.
    /// mode·충돌 처리는 transfer::upload와 같다.
    #[allow(clippy::too_many_arguments)] // FFI 계약상 시그니처가 고정돼 있다.
    pub async fn upload(
        &self,
        src: &Path,
        parent: &str,
        name: &str,
        mode: &UploadMode,
        opts: &TransferOptions,
        progress: &dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<Item, Error> {
        let item = crate::transfer::upload(
            &self.backend,
            src,
            parent,
            name,
            mode,
            opts,
            progress,
            cancel,
        )
        .await?;
        self.lock()?.upsert_item(&item, now_ms())?;
        Ok(item)
    }

    /// src_dir를 parent 아래 폴더로 재귀 업로드한다(3.6 제외 규칙 적용).
    /// mode는 대상 폴더 생성과 모든 파일에 적용된다.
    /// 성공 후 대상 폴더의 하위 캐시를 지우고 폴더 항목만 넣는다 —
    /// KeepBoth로 실제 키를 알 수 없어 하위는 다음 열람 때 원격에서
    /// 새로 받는다.
    pub async fn upload_folder(
        &self,
        src_dir: &Path,
        parent: &str,
        mode: &UploadMode,
        opts: &TransferOptions,
        progress: &dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<FolderUpload, Error> {
        let result = crate::transfer::upload_folder(
            &self.backend,
            src_dir,
            parent,
            mode,
            opts,
            progress,
            cancel,
        )
        .await?;
        if let Some(name) = src_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.is_empty())
        {
            let prefix = key::child_key(&key::normalize_prefix(parent), &name, true);
            let mut store = self.lock()?;
            store.remove_item(&prefix)?;
            store.upsert_item(&Item::folder(prefix), now_ms())?;
        }
        Ok(result)
    }

    /// 최근 열어본 폴더의 하위 폴더 목록을 미리 캐시한다 (SPEC Phase 4).
    ///
    /// `prefix`의 캐시된 목록에서 하위 폴더를 골라, 캐시가 complete가 아닌
    /// 것을 최대 `max_folders`개까지 동시 `concurrency`개로 원격에서 받아
    /// 캐시에 넣는다. 반환값은 이번에 새로 받은 폴더 수다.
    /// 취소는 이 future를 drop하는 것으로 한다 — JoinSet이 drop될 때
    /// 남은 목록 태스크를 함께 중단한다.
    pub async fn prefetch(
        &self,
        prefix: &str,
        max_folders: usize,
        concurrency: usize,
    ) -> Result<u32, Error> {
        let prefix = key::normalize_prefix(prefix);
        let targets: Vec<String> = {
            let store = self.lock()?;
            store
                .cached_page(&prefix, None, usize::MAX)?
                .into_iter()
                .filter(|item| item.is_dir)
                .filter(|item| !matches!(store.folder_state(&item.key), Ok(Some(s)) if s.complete))
                .map(|item| item.key)
                .take(max_folders)
                .collect()
        };
        if targets.is_empty() {
            return Ok(0);
        }

        // 목록 요청만 concurrency로 제한하고, 캐시 반영은 모아서 순차로 한다.
        let sem = Arc::new(Semaphore::new(concurrency.max(1)));
        let mut tasks = JoinSet::new();
        for key in targets {
            let backend = self.backend.clone();
            let sem = Arc::clone(&sem);
            tasks.spawn(async move {
                let _permit = sem.acquire_owned().await;
                backend.list_folder(&key).await.map(|items| (key, items))
            });
        }

        let mut fetched = 0u32;
        while let Some(result) = tasks.join_next().await {
            // JoinError는 태스크 panic/cancel뿐이다.
            let (key, items) =
                result.map_err(|e| Error::Other(format!("프리페치 태스크가 실패했다: {e}")))??;
            self.lock()?.replace_folder(&key, &items, now_ms())?;
            fetched += 1;
        }
        log::info!("하위 폴더 {fetched}개를 미리 캐시했다");
        Ok(fetched)
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    /// 목록 내용을 바꿀 수 있는 가짜 백엔드.
    /// `hold` 뮤텍스를 잡으면 list_folder가 그곳에서 멈춰
    /// "갱신 진행 중" 상황을 만들 수 있다.
    #[derive(Clone, Default)]
    struct FakeBackend {
        folders: Arc<Mutex<HashMap<String, Vec<Item>>>>,
        list_calls: Arc<AtomicUsize>,
        auth_fail: Arc<AtomicBool>,
        write_deny: Arc<AtomicBool>,
        writes: Arc<Mutex<Vec<String>>>,
        hold: Arc<tokio::sync::Mutex<()>>,
    }

    impl FakeBackend {
        fn with_folder(self, prefix: &str, items: Vec<Item>) -> Self {
            self.set_folder(prefix, items);
            self
        }

        fn set_folder(&self, prefix: &str, items: Vec<Item>) {
            self.folders
                .lock()
                .unwrap()
                .insert(prefix.to_string(), items);
        }

        fn calls(&self) -> usize {
            self.list_calls.load(Ordering::SeqCst)
        }

        fn fail_auth(&self) {
            self.auth_fail.store(true, Ordering::SeqCst);
        }

        fn deny_write(&self) {
            self.write_deny.store(true, Ordering::SeqCst);
        }

        /// write_bytes로 쓰인 키 목록(삭제된 것은 빠진다).
        fn written(&self) -> Vec<String> {
            self.writes.lock().unwrap().clone()
        }
    }

    impl Backend for FakeBackend {
        async fn list_folder(&self, prefix: &str) -> Result<Vec<Item>, Error> {
            self.list_calls.fetch_add(1, Ordering::SeqCst);
            let _guard = self.hold.lock().await;
            if self.auth_fail.load(Ordering::SeqCst) {
                return Err(Error::Authentication("fake: 인증 실패".to_string()));
            }
            Ok(self
                .folders
                .lock()
                .unwrap()
                .get(prefix)
                .cloned()
                .unwrap_or_default())
        }

        async fn list_recursive(&self, _prefix: &str) -> Result<Vec<Item>, Error> {
            Ok(vec![])
        }

        async fn stat(&self, key: &str) -> Result<Item, Error> {
            Err(Error::NotFound(key.to_string()))
        }

        async fn write_bytes(&self, key: &str, _data: Vec<u8>) -> Result<(), Error> {
            if self.write_deny.load(Ordering::SeqCst) {
                return Err(Error::Authentication("fake: 쓰기 권한 없음".to_string()));
            }
            self.writes.lock().unwrap().push(key.to_string());
            Ok(())
        }

        async fn delete_keys(&self, keys: &[String]) -> Result<(), Error> {
            let mut writes = self.writes.lock().unwrap();
            for key in keys {
                writes.retain(|w| w != key);
            }
            Ok(())
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

    #[tokio::test]
    async fn refresh_reports_added_changed_and_removed() {
        let backend = FakeBackend::default().with_folder(
            "r/",
            vec![
                Item::file("r/a.txt", 1, Some("e1".to_string()), None),
                Item::file("r/b.txt", 2, None, None),
                Item::folder("r/sub/"),
            ],
        );
        let conn =
            Connection::with_backend(backend.clone(), MetadataStore::open_in_memory().unwrap());
        conn.list_folder("r/", None).await.unwrap();

        // 원격 변경: a.txt 메타데이터 변경, b.txt 삭제, c.txt·sub2/ 추가.
        backend.set_folder(
            "r/",
            vec![
                Item::file("r/a.txt", 9, Some("e2".to_string()), None),
                Item::file("r/c.txt", 3, None, None),
                Item::folder("r/sub/"),
                Item::folder("r/sub2/"),
            ],
        );
        let changes = conn.refresh("r/").await.unwrap();
        let upserted: Vec<&str> = changes.upserted.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(upserted, ["r/a.txt", "r/c.txt", "r/sub2/"]);
        assert_eq!(changes.removed, ["r/b.txt"]);

        // 캐시도 새 목록으로 교체됐다.
        let page = conn.list_folder("r/", None).await.unwrap();
        assert!(page.from_cache);
        let keys: Vec<&str> = page.items.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, ["r/a.txt", "r/c.txt", "r/sub/", "r/sub2/"]);
    }

    #[tokio::test]
    async fn refresh_clears_removed_folder_subtree() {
        let backend = FakeBackend::default()
            .with_folder(
                "p/",
                vec![Item::folder("p/sub/"), Item::file("p/a.txt", 1, None, None)],
            )
            .with_folder("p/sub/", vec![Item::file("p/sub/x.txt", 1, None, None)]);
        let conn =
            Connection::with_backend(backend.clone(), MetadataStore::open_in_memory().unwrap());
        conn.list_folder("p/", None).await.unwrap();
        conn.list_folder("p/sub/", None).await.unwrap();
        assert!(conn.stat("p/sub/x.txt").await.is_ok());

        // 원격에서 sub/ 전체가 사라졌다.
        backend.set_folder("p/", vec![Item::file("p/a.txt", 1, None, None)]);
        backend.set_folder("p/sub/", vec![]);
        let changes = conn.refresh("p/").await.unwrap();
        assert_eq!(changes.removed, ["p/sub/"]);

        // 하위 캐시도 지워져 stat은 원격(NotFound)으로 간다.
        let err = conn.stat("p/sub/x.txt").await.unwrap_err();
        assert!(matches!(err, Error::NotFound(_)), "{err:?}");
        // p/sub/ 폴더는 다시 원격 목록을 받는다(캐시가 지워졌다).
        let page = conn.list_folder("p/sub/", None).await.unwrap();
        assert!(!page.from_cache);
        assert!(page.items.is_empty());
    }

    #[tokio::test]
    async fn cache_responds_while_refresh_in_flight() {
        let backend = FakeBackend::default().with_folder("w/", file_items("w/", 3));
        let conn = Arc::new(Connection::with_backend(
            backend.clone(),
            MetadataStore::open_in_memory().unwrap(),
        ));
        conn.list_folder("w/", None).await.unwrap();

        // 원격 내용을 바꾸고 백엔드 호출을 붙잡는다.
        backend.set_folder("w/", file_items("w/", 5));
        let guard = backend.hold.lock().await;
        let conn2 = Arc::clone(&conn);
        let refresh = tokio::spawn(async move { conn2.refresh("w/").await });
        // 백엔드가 호출됐는지 확인할 시간을 준다.
        for _ in 0..100 {
            if backend.calls() >= 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(backend.calls(), 2, "refresh의 원격 호출이 시작되지 않았다");

        // 갱신 진행 중에도 list_folder는 이전 캐시로 즉시 응답한다.
        let page = conn.list_folder("w/", None).await.unwrap();
        assert!(page.from_cache);
        assert_eq!(page.items.len(), 3);

        drop(guard);
        let changes = refresh.await.unwrap().unwrap();
        assert_eq!(changes.upserted.len(), 2, "새 파일 2개만 추가됐다");
        assert!(changes.removed.is_empty());

        // 갱신 완료 후에는 새 캐시가 보인다.
        let page = conn.list_folder("w/", None).await.unwrap();
        assert_eq!(page.items.len(), 5);
    }

    #[tokio::test]
    async fn test_connection_ok_and_error_propagation() {
        let backend = FakeBackend::default();
        let conn =
            Connection::with_backend(backend.clone(), MetadataStore::open_in_memory().unwrap());
        let can_write = conn.test_connection().await.expect("연결 성공이어야 한다");
        assert!(can_write, "쓰기 가능해야 한다");
        // 쓰기 테스트 객체는 쓰고 지워져 남지 않는다.
        assert!(backend.written().is_empty());
        // 캐시에 아무것도 쓰지 않는다.
        assert!(conn.lock().unwrap().folder_state("").unwrap().is_none());

        // 쓰기 권한이 없으면 can_write=false (오류가 아니다).
        backend.deny_write();
        let can_write = conn.test_connection().await.expect("읽기는 성공해야 한다");
        assert!(!can_write);

        backend.fail_auth();
        let err = conn.test_connection().await.expect_err("실패해야 한다");
        assert!(matches!(err, Error::Authentication(_)), "{err:?}");
    }
}
