//! SQLite 메타데이터 캐시 (SPEC 4.2).
//!
//! 폴더 목록을 캐시해 두 번째 조회부터는 네트워크 없이 응답한다.
//! `replace_folder`는 원격에서 받은 폴더 전체 목록을 한 트랜잭션으로
//! 반영하므로 목록이 미완료로 보이는 중간 상태가 없다. 갱신 중에도
//! 캐시 읽기는 이전 내용을 그대로 반환한다(SPEC 3.5).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension, Row, Transaction};

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

/// `replace_folder`가 반환하는 폴더 목록 변경 사항.
/// FFI의 PierChanges(upserted/removed)에 대응한다.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Changes {
    /// 새로 생겼거나 is_dir/size/etag/last_modified_ms/storage_class가
    /// 바뀐 항목. 순서는 목록 순서(키 오름차순)를 따른다.
    pub upserted: Vec<Item>,
    /// `parent = prefix` 중 이번 목록에 없는 직계 자식의 key.
    /// 사라진 폴더의 하위 캐시도 함께 지워지지만 여기에는 직계 자식만
    /// 들어간다(정렬됨).
    pub removed: Vec<String>,
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

    /// 폴더의 전체 목록을 한 트랜잭션으로 캐시에 반영하고 변경 사항을
    /// 반환한다. `items`는 backend.list_folder가 반환한 폴더 직계 자식
    /// 전체다(빈 폴더면 빈 슬라이스).
    ///
    /// upsert·삭제·complete 표시를 한 트랜잭션에서 끝내므로 폴더가
    /// 미완료로 보이는 중간 상태가 없다. 갱신 중에도 캐시 읽기는 이전
    /// 내용을 반환한다(SPEC 3.5). `now_ms`는 last_listed_at과 각 행의
    /// seen_at에 기록한다.
    pub fn replace_folder(
        &mut self,
        prefix: &str,
        items: &[Item],
        now_ms: i64,
    ) -> Result<Changes, Error> {
        let prefix = key::normalize_prefix(prefix);
        let tx = self.conn.transaction()?;

        // 변경 계산을 위해 기존 직계 자식을 읽어 둔다.
        let existing = read_children(&tx, &prefix)?;
        let new_keys: HashSet<&str> = items.iter().map(|item| item.key.as_str()).collect();

        let mut changes = Changes::default();
        let mut seen: HashSet<&str> = HashSet::with_capacity(items.len());
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
                    now_ms,
                ])?;
                // 같은 키가 중복으로 와도 upserted에는 한 번만 넣는다.
                if seen.insert(item.key.as_str())
                    && existing.get(&item.key).is_none_or(|old| !old.matches(item))
                {
                    changes.upserted.push(item.clone());
                }
            }
        }

        // 목록에서 사라진 직계 자식(키 순 정렬).
        let mut removed: Vec<&CachedRow> = existing
            .values()
            .filter(|row| !new_keys.contains(row.key.as_str()))
            .collect();
        removed.sort_by(|a, b| a.key.cmp(&b.key));
        for row in &removed {
            changes.removed.push(row.key.clone());
        }
        {
            let mut stmt = tx.prepare("DELETE FROM items WHERE key = ?1")?;
            for row in &removed {
                stmt.execute(params![row.key])?;
            }
        }

        // 사라진 폴더의 하위 캐시(items·folders 행)를 함께 지운다.
        for row in removed.iter().filter(|row| row.is_dir) {
            let upper = prefix_upper_bound(&row.key);
            tx.execute(
                "DELETE FROM items WHERE key >= ?1 AND key < ?2",
                params![row.key, upper],
            )?;
            tx.execute(
                "DELETE FROM folders WHERE prefix >= ?1 AND prefix < ?2",
                params![row.key, upper],
            )?;
        }

        // 항목이 하나도 없는 빈 폴더도 folders 행이 생기도록 upsert한다.
        // last_opened_at은 기존 값을 유지한다.
        tx.execute(
            "INSERT INTO folders (prefix, last_listed_at, last_opened_at, complete)
             VALUES (?1, ?2, NULL, 1)
             ON CONFLICT(prefix) DO UPDATE SET
                complete = 1,
                last_listed_at = excluded.last_listed_at",
            params![prefix, now_ms],
        )?;
        tx.commit()?;
        Ok(changes)
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

/// 캐시 items 행의 비교용 필드. `name`은 key에서 계산하므로 저장하지 않는다.
struct CachedRow {
    key: String,
    is_dir: bool,
    size: i64,
    etag: Option<String>,
    last_modified_ms: Option<i64>,
    storage_class: Option<String>,
}

impl CachedRow {
    /// 새 목록의 Item과 표시용 메타데이터가 같은지 비교한다.
    /// 다르면 upserted로 보고한다.
    fn matches(&self, item: &Item) -> bool {
        // S3 객체 크기는 최대 5TB라 i64에 항상 들어간다.
        self.is_dir == item.is_dir
            && self.size == item.size as i64
            && self.etag == item.etag
            && self.last_modified_ms == item.last_modified_ms
            && self.storage_class == item.storage_class
    }
}

/// `parent = prefix`인 캐시 행을 key → CachedRow 맵으로 읽는다.
fn read_children(tx: &Transaction<'_>, prefix: &str) -> Result<HashMap<String, CachedRow>, Error> {
    let mut stmt = tx.prepare(
        "SELECT key, is_dir, size, etag, last_modified, storage_class
         FROM items WHERE parent = ?1",
    )?;
    let rows = stmt.query_map(params![prefix], |row| {
        Ok(CachedRow {
            key: row.get(0)?,
            is_dir: row.get(1)?,
            size: row.get(2)?,
            etag: row.get(3)?,
            last_modified_ms: row.get(4)?,
            storage_class: row.get(5)?,
        })
    })?;
    let mut map = HashMap::new();
    for row in rows {
        let row = row?;
        map.insert(row.key.clone(), row);
    }
    Ok(map)
}

/// `prefix`로 시작하는 모든 키의 배타적 상한을 반환한다.
///
/// 폴더 키는 '/'로 끝나므로 마지막 '/'(0x2F) 다음 바이트 문자인 '0'(0x30)으로
/// 바꾼 문자열이 상한이다. `key >= prefix AND key < upper`는
/// `key LIKE prefix || '%'`와 같은 집합이지만, S3 키에는 '%'와 '_'가 올 수
/// 있어 LIKE 패턴은 와일드카드로 해석돼 엉뚱한 키를 지울 수 있다.
/// 범위 비교는 정확하고 items.key/folders.prefix의 기본키 인덱스도 탄다.
fn prefix_upper_bound(prefix: &str) -> String {
    debug_assert!(prefix.ends_with('/'), "폴더 키는 '/'로 끝난다");
    match prefix.strip_suffix('/') {
        Some(head) => format!("{head}0"),
        // 방어: '/'로 끝나지 않는 키가 들어와도 아무것도 지우지 않는 상한을 둔다.
        None => format!("{prefix}\u{10FFFF}"),
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
                .replace_folder("", &[Item::file("a.txt", 1, None, None)], 100)
                .unwrap();
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
    fn replace_folder_paginates_in_byte_order() {
        let mut store = MetadataStore::open_in_memory().unwrap();
        let items = vec![
            Item::file("photos/Z.txt", 1, None, Some(1)),
            Item::file("photos/a.txt", 2, None, Some(2)),
            Item::file("photos/b.txt", 3, None, Some(3)),
            Item::folder("photos/sub/"),
            Item::file("photos/한글.txt", 4, None, Some(4)),
            Item::file("photos/📷.jpg", 5, None, Some(5)),
        ];
        store.replace_folder("photos/", &items, 1000).unwrap();

        // 한 트랜잭션 교체이므로 바로 complete다.
        let state = store.folder_state("photos/").unwrap().unwrap();
        assert!(state.complete);
        assert_eq!(state.last_listed_at_ms, Some(1000));
        // 끝 '/' 없는 prefix도 같은 행을 가리킨다.
        assert_eq!(store.folder_state("photos").unwrap(), Some(state.clone()));

        // key 오름차순 = UTF-8 바이트 순서. Rust의 str 정렬과 비교해 검증한다.
        let all = store.cached_page("photos/", None, 100).unwrap();
        let keys: Vec<&str> = all.iter().map(|i| i.key.as_str()).collect();
        let mut expected: Vec<&str> = items.iter().map(|i| i.key.as_str()).collect();
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
    fn replace_folder_reports_upserted_and_removed() {
        let mut store = MetadataStore::open_in_memory().unwrap();

        // 다른 parent는 이 테스트의 재목록 영향을 받지 않아야 한다.
        store
            .replace_folder("other/", &[Item::file("other/x.txt", 9, None, None)], 500)
            .unwrap();

        let first = store
            .replace_folder(
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
        // 첫 목록은 전부 새 항목이다.
        assert_eq!(first.upserted.len(), 4);
        assert!(first.removed.is_empty());

        // 두 번째 목록: a.txt는 etag/size가 바뀌고, b/c는 사라지고,
        // d.txt가 새로 생긴다. sub/는 그대로라 보고되지 않는다.
        let second = store
            .replace_folder(
                "photos/",
                &[
                    Item::file("photos/a.txt", 99, Some("e2".to_string()), Some(22)),
                    Item::file("photos/d.txt", 40, None, None),
                    Item::folder("photos/sub/"),
                ],
                2000,
            )
            .unwrap();
        let upserted_keys: Vec<&str> = second.upserted.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(upserted_keys, ["photos/a.txt", "photos/d.txt"]);
        assert_eq!(second.removed, ["photos/b.txt", "photos/c.txt"]);

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
            .replace_folder(
                "",
                &[Item::file("a.txt", 1, None, None), Item::folder("dir/")],
                100,
            )
            .unwrap();

        let items = store.cached_page("", None, 10).unwrap();
        let keys: Vec<&str> = items.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, ["a.txt", "dir/"]);
        assert!(store.folder_state("").unwrap().unwrap().complete);
    }

    #[test]
    fn replace_folder_identical_listing_reports_nothing() {
        let mut store = MetadataStore::open_in_memory().unwrap();
        let items = vec![
            Item::file("p/a.txt", 1, Some("e".to_string()), Some(7)),
            Item::folder("p/sub/"),
        ];
        store.replace_folder("p/", &items, 100).unwrap();
        let changes = store.replace_folder("p/", &items, 200).unwrap();
        assert!(changes.upserted.is_empty());
        assert!(changes.removed.is_empty());
        // last_listed_at은 갱신된다.
        assert_eq!(
            store.folder_state("p/").unwrap().unwrap().last_listed_at_ms,
            Some(200)
        );
    }

    #[test]
    fn replace_folder_reports_each_metadata_change() {
        let mut store = MetadataStore::open_in_memory().unwrap();
        store
            .replace_folder(
                "p/",
                &[
                    Item::file("p/size.txt", 1, None, None),
                    Item::file("p/etag.txt", 1, Some("e1".to_string()), None),
                    Item::file("p/dir.txt", 1, None, None),
                    Item::file("p/class.txt", 1, None, None),
                ],
                100,
            )
            .unwrap();
        // 네 항목 각각 size/etag/is_dir/storage_class만 다르게 바꾼다.
        // (backend가 주는 목록 순서 = 키 오름차순)
        let mut dir_as_dir = Item::file("p/dir.txt", 1, None, None);
        dir_as_dir.is_dir = true;
        let mut classed = Item::file("p/class.txt", 1, None, None);
        classed.storage_class = Some("GLACIER".to_string());
        let changes = store
            .replace_folder(
                "p/",
                &[
                    classed,
                    dir_as_dir,
                    Item::file("p/etag.txt", 1, Some("e2".to_string()), None),
                    Item::file("p/size.txt", 2, None, None),
                ],
                200,
            )
            .unwrap();
        let keys: Vec<&str> = changes.upserted.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(
            keys,
            ["p/class.txt", "p/dir.txt", "p/etag.txt", "p/size.txt"]
        );
        assert!(changes.removed.is_empty());
    }

    #[test]
    fn removed_folder_clears_descendant_cache() {
        let mut store = MetadataStore::open_in_memory().unwrap();
        // p/ 아래 폴더 두 개와 각각의 자식을 캐시한다.
        store
            .replace_folder("p/", &[Item::folder("p/a/"), Item::folder("p/b/")], 100)
            .unwrap();
        store
            .replace_folder("p/a/", &[Item::file("p/a/x.txt", 1, None, None)], 100)
            .unwrap();
        store
            .replace_folder(
                "p/b/deep/",
                &[Item::file("p/b/deep/y.txt", 1, None, None)],
                100,
            )
            .unwrap();
        assert!(store.folder_state("p/a/").unwrap().unwrap().complete);
        assert!(store.folder_state("p/b/deep/").unwrap().unwrap().complete);

        // p/ 재목록에서 a/가 사라졌다.
        let changes = store
            .replace_folder("p/", &[Item::folder("p/b/")], 200)
            .unwrap();
        assert_eq!(changes.removed, ["p/a/"]);

        // a/의 items와 folders 행(자손 포함)이 모두 지워진다.
        assert!(store.get_item("p/a/").unwrap().is_none());
        assert!(store.get_item("p/a/x.txt").unwrap().is_none());
        assert!(store.folder_state("p/a/").unwrap().is_none());
        // b/ 쪽 캐시는 그대로다.
        assert!(store.get_item("p/b/deep/y.txt").unwrap().is_some());
        assert!(store.folder_state("p/b/deep/").unwrap().unwrap().complete);
    }

    #[test]
    fn removed_folder_cleanup_ignores_like_wildcards() {
        // 키에 '%'와 '_'가 있어도 범위 비교는 LIKE처럼 오인하지 않는다.
        // `p/a%b_c/`를 지울 때 LIKE `p/a%b_c/%`는 `p/aXb_c/`까지 매칭한다.
        let mut store = MetadataStore::open_in_memory().unwrap();
        store
            .replace_folder(
                "p/",
                &[Item::folder("p/a%b_c/"), Item::folder("p/aXb_c/")],
                100,
            )
            .unwrap();
        store
            .replace_folder(
                "p/a%b_c/",
                &[Item::file("p/a%b_c/f%1.txt", 1, None, None)],
                100,
            )
            .unwrap();
        store
            .replace_folder(
                "p/aXb_c/",
                &[Item::file("p/aXb_c/g_2.txt", 1, None, None)],
                100,
            )
            .unwrap();

        let changes = store
            .replace_folder("p/", &[Item::folder("p/aXb_c/")], 200)
            .unwrap();
        assert_eq!(changes.removed, ["p/a%b_c/"]);

        // 지워져야 할 것: p/a%b_c/ 폴더와 그 자식.
        assert!(store.get_item("p/a%b_c/").unwrap().is_none());
        assert!(store.get_item("p/a%b_c/f%1.txt").unwrap().is_none());
        assert!(store.folder_state("p/a%b_c/").unwrap().is_none());
        // 남아야 할 것: LIKE라면 함께 지워졌을 p/aXb_c/ 쪽.
        assert!(store.get_item("p/aXb_c/").unwrap().is_some());
        assert!(store.get_item("p/aXb_c/g_2.txt").unwrap().is_some());
        assert!(store.folder_state("p/aXb_c/").unwrap().unwrap().complete);
    }

    #[test]
    fn mark_opened_records_time_without_touching_listing_state() {
        let mut store = MetadataStore::open_in_memory().unwrap();
        store
            .replace_folder(
                "photos/",
                &[Item::file("photos/a.txt", 1, None, None)],
                1000,
            )
            .unwrap();

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
