//! SQLite 메타데이터 캐시 (SPEC 4.2).
//!
//! 폴더 목록을 캐시해 두 번째 조회부터는 네트워크 없이 응답한다.
//! 각 항목의 `seen_at`에는 목록을 시작한 시각(`listing_started_at_ms`)을
//! 기록하고, `finish_listing`에서 그 시각보다 오래된 항목을 삭제해
//! 원격에서 사라진 객체를 반영한다.

use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::error::Error;
use crate::item::Item;
use crate::key;

/// 스키마 버전. `PRAGMA user_version`에 기록한다.
const SCHEMA_VERSION: i64 = 1;

/// 폴더 단위 목록 상태.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderState {
    /// 마지막 목록이 끝까지 완료됐는지. 진행 중이거나 한 번도 안 끝났으면 false.
    pub complete: bool,
    pub last_listed_at_ms: Option<i64>,
    pub last_opened_at_ms: Option<i64>,
}

/// SQLite 메타데이터 캐시.
/// 모든 시각 값은 호출자가 Unix epoch 밀리초로 넘긴다.
pub struct MetadataStore {
    conn: Connection,
}

impl MetadataStore {
    /// 파일 DB를 연다. 부모 디렉터리를 만들고, 파일 DB에 한해 WAL 모드를 켠 뒤
    /// 마이그레이션한다.
    pub fn open(path: &Path) -> Result<Self, Error> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(path)?;
        // 인메모리 DB는 journal_mode가 memory로 고정되므로 파일 DB에만 켠다.
        conn.execute_batch("PRAGMA journal_mode = WAL")?;
        let mut store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    /// 인메모리 DB를 연다. 테스트용.
    pub fn open_in_memory() -> Result<Self, Error> {
        let mut store = Self {
            conn: Connection::open_in_memory()?,
        };
        store.migrate()?;
        Ok(store)
    }

    /// 원격 목록의 한 페이지를 저장한다(upsert).
    /// `seen_at`은 `listing_started_at_ms`로 기록한다. 페이지마다 folders 행을
    /// complete=0으로 유지해 목록이 아직 끝나지 않았음을 표시한다.
    pub fn store_page(
        &mut self,
        prefix: &str,
        items: &[Item],
        listing_started_at_ms: i64,
    ) -> Result<(), Error> {
        let prefix = key::normalize_prefix(prefix);
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO folders (prefix, last_listed_at, last_opened_at, complete)
             VALUES (?1, NULL, NULL, 0)
             ON CONFLICT(prefix) DO UPDATE SET complete = 0",
            params![prefix],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO items
                    (key, parent, is_dir, size, etag, last_modified, storage_class, seen_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(key) DO UPDATE SET
                    parent = excluded.parent,
                    is_dir = excluded.is_dir,
                    size = excluded.size,
                    etag = excluded.etag,
                    last_modified = excluded.last_modified,
                    storage_class = excluded.storage_class,
                    seen_at = excluded.seen_at",
            )?;
            for item in items {
                // S3 객체 크기는 최대 5TB라 i64에 항상 들어간다.
                stmt.execute(params![
                    item.key,
                    item.parent,
                    item.is_dir,
                    item.size as i64,
                    item.etag,
                    item.last_modified_ms,
                    item.storage_class,
                    listing_started_at_ms,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// 마지막 페이지까지 받은 뒤 호출한다.
    /// `parent = prefix` 중 `seen_at < listing_started_at_ms`인 항목(사라진
    /// 객체)을 삭제하고, folders를 complete=1·last_listed_at으로 갱신한다.
    pub fn finish_listing(
        &mut self,
        prefix: &str,
        listing_started_at_ms: i64,
    ) -> Result<(), Error> {
        let prefix = key::normalize_prefix(prefix);
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM items WHERE parent = ?1 AND seen_at < ?2",
            params![prefix, listing_started_at_ms],
        )?;
        // 항목이 하나도 없는 빈 폴더도 folders 행이 생기도록 upsert한다.
        tx.execute(
            "INSERT INTO folders (prefix, last_listed_at, last_opened_at, complete)
             VALUES (?1, ?2, NULL, 1)
             ON CONFLICT(prefix) DO UPDATE SET
                complete = 1,
                last_listed_at = excluded.last_listed_at",
            params![prefix, listing_started_at_ms],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// 폴더의 목록 상태를 반환한다. 한 번도 목록/오픈된 적 없으면 None.
    pub fn folder_state(&self, prefix: &str) -> Result<Option<FolderState>, Error> {
        let prefix = key::normalize_prefix(prefix);
        let state = self
            .conn
            .query_row(
                "SELECT last_listed_at, last_opened_at, complete
                 FROM folders WHERE prefix = ?1",
                params![prefix],
                |row| {
                    Ok(FolderState {
                        last_listed_at_ms: row.get(0)?,
                        last_opened_at_ms: row.get(1)?,
                        complete: row.get::<_, i64>(2)? != 0,
                    })
                },
            )
            .optional()?;
        Ok(state)
    }

    /// `parent = prefix` 항목을 key 오름차순으로 `after`(배타) 다음부터
    /// `limit`개 반환한다. 정렬은 SQLite BINARY collation(UTF-8 바이트 순서)으로
    /// S3와 같다.
    pub fn cached_page(
        &self,
        prefix: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Item>, Error> {
        let prefix = key::normalize_prefix(prefix);
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut stmt = self.conn.prepare(
            "SELECT key, parent, is_dir, size, etag, last_modified, storage_class
             FROM items
             WHERE parent = ?1 AND (?2 IS NULL OR key > ?2)
             ORDER BY key ASC
             LIMIT ?3",
        )?;
        let items = stmt
            .query_map(params![prefix, after, limit], row_to_item)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(items)
    }

    /// 키로 단일 항목을 조회한다.
    pub fn get_item(&self, key: &str) -> Result<Option<Item>, Error> {
        let item = self
            .conn
            .query_row(
                "SELECT key, parent, is_dir, size, etag, last_modified, storage_class
                 FROM items WHERE key = ?1",
                params![key],
                row_to_item,
            )
            .optional()?;
        Ok(item)
    }

    /// 폴더를 열었다는 시각을 기록한다. folders 행이 없으면 만든다
    /// (complete=false, last_listed_at=NULL).
    pub fn mark_opened(&mut self, prefix: &str, now_ms: i64) -> Result<(), Error> {
        let prefix = key::normalize_prefix(prefix);
        self.conn.execute(
            "INSERT INTO folders (prefix, last_listed_at, last_opened_at, complete)
             VALUES (?1, NULL, ?2, 0)
             ON CONFLICT(prefix) DO UPDATE SET last_opened_at = excluded.last_opened_at",
            params![prefix, now_ms],
        )?;
        Ok(())
    }

    /// `PRAGMA user_version` 기반 마이그레이션. 0 → 1.
    fn migrate(&mut self) -> Result<(), Error> {
        let version: i64 = self
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version >= SCHEMA_VERSION {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        tx.execute_batch(
            "CREATE TABLE items (
                key TEXT PRIMARY KEY,
                parent TEXT NOT NULL,
                is_dir INTEGER NOT NULL,
                size INTEGER NOT NULL,
                etag TEXT,
                last_modified INTEGER,
                storage_class TEXT,
                seen_at INTEGER NOT NULL
            );
            CREATE INDEX items_parent ON items(parent);
            CREATE TABLE folders (
                prefix TEXT PRIMARY KEY,
                last_listed_at INTEGER,
                last_opened_at INTEGER,
                complete INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE state (
                name TEXT PRIMARY KEY,
                value TEXT
            );
            CREATE TABLE pending_ops (
                id INTEGER PRIMARY KEY,
                kind TEXT NOT NULL,
                payload TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );",
        )?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit()?;
        Ok(())
    }
}

/// items 행을 Item으로 변환한다. `name`은 저장하지 않고 key에서 계산한다.
fn row_to_item(row: &Row<'_>) -> rusqlite::Result<Item> {
    let key: String = row.get(0)?;
    let parent: String = row.get(1)?;
    let is_dir: bool = row.get(2)?;
    let size: i64 = row.get(3)?;
    let etag: Option<String> = row.get(4)?;
    let last_modified_ms: Option<i64> = row.get(5)?;
    let storage_class: Option<String> = row.get(6)?;
    Ok(Item {
        name: key::name(&key).to_string(),
        key,
        parent,
        is_dir,
        size: u64::try_from(size).unwrap_or_default(),
        etag,
        last_modified_ms,
        storage_class,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 테스트마다 고유한 임시 디렉터리 경로를 만든다.
    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("pier-metadata-test-{}-{tag}", std::process::id()))
    }

    #[test]
    fn migration_sets_user_version_to_one() {
        let store = MetadataStore::open_in_memory().unwrap();
        let version: i64 = store
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn file_db_uses_wal_and_persists_across_reopen() {
        let dir = temp_dir("wal");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("metadata.sqlite");

        {
            let mut store = MetadataStore::open(&path).unwrap();
            let journal: String = store
                .conn
                .query_row("PRAGMA journal_mode", [], |row| row.get(0))
                .unwrap();
            assert_eq!(journal, "wal");

            store
                .store_page("", &[Item::file("a.txt", 1, None, None)], 100)
                .unwrap();
            store.finish_listing("", 100).unwrap();
        }

        // 닫았다 다시 열어도 데이터가 남아 있어야 한다.
        let store = MetadataStore::open(&path).unwrap();
        let item = store.get_item("a.txt").unwrap().unwrap();
        assert_eq!(item.size, 1);
        assert!(store.folder_state("").unwrap().unwrap().complete);
        drop(store);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn store_pages_then_cached_page_paginates_in_byte_order() {
        let mut store = MetadataStore::open_in_memory().unwrap();
        let page1 = vec![
            Item::file("photos/Z.txt", 1, None, Some(1)),
            Item::file("photos/a.txt", 2, None, Some(2)),
            Item::file("photos/b.txt", 3, None, Some(3)),
        ];
        let page2 = vec![
            Item::folder("photos/sub/"),
            Item::file("photos/한글.txt", 4, None, Some(4)),
            Item::file("photos/📷.jpg", 5, None, Some(5)),
        ];
        store.store_page("photos/", &page1, 1000).unwrap();
        store.store_page("photos/", &page2, 1000).unwrap();

        // 목록이 끝나기 전까지는 complete=false다.
        let state = store.folder_state("photos/").unwrap().unwrap();
        assert!(!state.complete);
        assert_eq!(state.last_listed_at_ms, None);

        store.finish_listing("photos/", 1000).unwrap();
        let state = store.folder_state("photos/").unwrap().unwrap();
        assert!(state.complete);
        assert_eq!(state.last_listed_at_ms, Some(1000));
        // 끝 '/' 없는 prefix도 같은 행을 가리킨다.
        assert_eq!(store.folder_state("photos").unwrap(), Some(state.clone()));

        // key 오름차순 = UTF-8 바이트 순서. Rust의 str 정렬과 비교해 검증한다.
        let all = store.cached_page("photos/", None, 100).unwrap();
        let keys: Vec<&str> = all.iter().map(|i| i.key.as_str()).collect();
        let mut expected: Vec<&str> = page1
            .iter()
            .chain(page2.iter())
            .map(|i| i.key.as_str())
            .collect();
        expected.sort_unstable();
        assert_eq!(keys, expected);

        // after는 배타적이고 limit은 페이지 크기다.
        let p1 = store.cached_page("photos/", None, 2).unwrap();
        assert_eq!(p1[0].key, "photos/Z.txt");
        assert_eq!(p1[1].key, "photos/a.txt");

        let p2 = store
            .cached_page("photos/", Some("photos/a.txt"), 2)
            .unwrap();
        assert_eq!(p2[0].key, "photos/b.txt");
        assert_eq!(p2[1].key, "photos/sub/");
        // 폴더 항목도 파일과 같은 규칙으로 돌아온다.
        assert!(p2[1].is_dir);
        assert_eq!(p2[1].name, "sub");
        assert_eq!(p2[1].parent, "photos/");
        assert_eq!(p2[1].size, 0);

        let p3 = store
            .cached_page("photos/", Some("photos/sub/"), 2)
            .unwrap();
        assert_eq!(p3.len(), 2);
        assert_eq!(p3[0].key, "photos/한글.txt");
        assert_eq!(p3[1].key, "photos/📷.jpg");
        // last_modified이 복원된다.
        assert_eq!(p3[0].last_modified_ms, Some(4));

        let p4 = store
            .cached_page("photos/", Some("photos/📷.jpg"), 2)
            .unwrap();
        assert!(p4.is_empty());
    }

    #[test]
    fn relist_updates_changed_and_removes_missing() {
        let mut store = MetadataStore::open_in_memory().unwrap();

        // 다른 parent는 이 테스트의 재목록 영향을 받지 않아야 한다.
        store
            .store_page("other/", &[Item::file("other/x.txt", 9, None, None)], 500)
            .unwrap();
        store.finish_listing("other/", 500).unwrap();

        store
            .store_page(
                "photos/",
                &[
                    Item::file("photos/a.txt", 10, Some("e1".to_string()), Some(11)),
                    Item::file("photos/b.txt", 20, None, None),
                    Item::file("photos/c.txt", 30, None, None),
                    Item::folder("photos/sub/"),
                ],
                1000,
            )
            .unwrap();
        store.finish_listing("photos/", 1000).unwrap();

        // 두 번째 목록: a.txt는 내용이 바뀌고, b/c는 사라지고, d.txt가 새로 생겼다.
        // sub/는 목록에 다시 포함됐으므로 유지된다.
        store
            .store_page(
                "photos/",
                &[
                    Item::file("photos/a.txt", 99, Some("e2".to_string()), Some(22)),
                    Item::file("photos/d.txt", 40, None, None),
                    Item::folder("photos/sub/"),
                ],
                2000,
            )
            .unwrap();
        store.finish_listing("photos/", 2000).unwrap();

        let a = store.get_item("photos/a.txt").unwrap().unwrap();
        assert_eq!(a.size, 99);
        assert_eq!(a.etag.as_deref(), Some("e2"));
        assert_eq!(a.last_modified_ms, Some(22));

        assert!(store.get_item("photos/b.txt").unwrap().is_none());
        assert!(store.get_item("photos/c.txt").unwrap().is_none());
        assert!(store.get_item("photos/d.txt").unwrap().is_some());
        assert!(store.get_item("photos/sub/").unwrap().is_some());

        let keys: Vec<String> = store
            .cached_page("photos/", None, 100)
            .unwrap()
            .into_iter()
            .map(|i| i.key)
            .collect();
        assert_eq!(keys, ["photos/a.txt", "photos/d.txt", "photos/sub/"]);

        // 다른 parent는 그대로다.
        let other = store.folder_state("other/").unwrap().unwrap();
        assert!(other.complete);
        assert_eq!(other.last_listed_at_ms, Some(500));
        assert!(store.get_item("other/x.txt").unwrap().is_some());

        let photos = store.folder_state("photos/").unwrap().unwrap();
        assert_eq!(photos.last_listed_at_ms, Some(2000));
        assert!(photos.complete);
    }

    #[test]
    fn root_prefix_listing_works() {
        let mut store = MetadataStore::open_in_memory().unwrap();
        store
            .store_page(
                "",
                &[Item::file("a.txt", 1, None, None), Item::folder("dir/")],
                100,
            )
            .unwrap();
        store.finish_listing("", 100).unwrap();

        let items = store.cached_page("", None, 10).unwrap();
        let keys: Vec<&str> = items.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, ["a.txt", "dir/"]);
        assert!(store.folder_state("").unwrap().unwrap().complete);
    }

    #[test]
    fn mark_opened_records_time_without_touching_listing_state() {
        let mut store = MetadataStore::open_in_memory().unwrap();
        store
            .store_page(
                "photos/",
                &[Item::file("photos/a.txt", 1, None, None)],
                1000,
            )
            .unwrap();
        store.finish_listing("photos/", 1000).unwrap();

        store.mark_opened("photos/", 5000).unwrap();
        let state = store.folder_state("photos/").unwrap().unwrap();
        assert_eq!(state.last_opened_at_ms, Some(5000));
        assert_eq!(state.last_listed_at_ms, Some(1000));
        assert!(state.complete);

        // 목록한 적 없는 폴더도 행이 생기고 complete=false다.
        store.mark_opened("new/", 6000).unwrap();
        let state = store.folder_state("new/").unwrap().unwrap();
        assert_eq!(state.last_opened_at_ms, Some(6000));
        assert_eq!(state.last_listed_at_ms, None);
        assert!(!state.complete);

        // 없는 폴더는 None, 없는 키는 None.
        assert!(store.folder_state("missing/").unwrap().is_none());
        assert!(store.get_item("missing.txt").unwrap().is_none());
    }
}
