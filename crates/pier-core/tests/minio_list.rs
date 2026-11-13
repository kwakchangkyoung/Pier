use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

use opendal::services::S3;
use opendal::Operator;
use pier_core::config::{ConnectionConfig, Provider};
use pier_core::enumerate::Connection;
use pier_core::error::Error;
use pier_core::transfer::{CancelToken, ProgressSink, TransferOptions};
use pier_core::Item;

/// 진행률을 무시하는 테스트용 싱크.
struct NoopProgress;

impl ProgressSink for NoopProgress {
    fn on_progress(&self, _transferred: u64, _total: u64) {}
}

fn minio_enabled() -> bool {
    std::env::var("PIER_TEST_MINIO").as_deref() == Ok("1")
}

fn minio_config() -> ConnectionConfig {
    ConnectionConfig {
        id: String::new(),
        display_name: "pier-test".to_string(),
        provider: Provider::MinIO,
        endpoint: Some("http://127.0.0.1:9000".to_string()),
        region: "us-east-1".to_string(),
        bucket: "pier-test".to_string(),
        prefix: None,
        path_style: true,
        access_key_id: "minioadmin".to_string(),
        chunk_size_mb: None,
        concurrency: None,
    }
}

/// 시드용 Operator. 백엔드와 달리 루트가 버킷 루트다.
fn minio_operator() -> Operator {
    let builder = S3::default()
        .bucket("pier-test")
        .region("us-east-1")
        .access_key_id("minioadmin")
        .secret_access_key("minioadmin")
        .disable_config_load()
        .disable_ec2_metadata()
        .root("/")
        .endpoint("http://127.0.0.1:9000");
    opendal::install_default();
    Operator::new(builder).expect("테스트 Operator 생성 실패")
}

/// 모든 MinIO 테스트가 공유하는 단일 런타임.
/// #[tokio::test]는 테스트마다 별도 런타임을 만들고, opendal의 전역 HTTP
/// 클라이언트는 연결 태스크를 만든 런타임에 묶인다. 런타임이 먼저
/// 종료되면 "dispatch task is gone" 오류가 나므로 하나를 공유한다.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("테스트 런타임 생성 실패")
    })
}

/// 테스트마다 고유한 임시 캐시 디렉터리. Drop에서 지운다.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "pier-it-{}-{}-{tag}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 시드는 프로세스당 한 번만 실행한다.
static SEED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

async fn ensure_seeded(operator: &Operator) {
    SEED.get_or_init(|| async { seed(operator).await.expect("시드 실패") })
        .await;
}

/// 테스트 객체를 멱등하게 생성한다. 이미 시드돼 있으면 건너뛴다.
/// 페이지네이션 데이터는 docs/ 아래에 두어, 버킷 루트 목록이 시드 여부와
/// 무관하게 항상 6개 항목으로 유지되게 한다.
///
/// 참고: S3는 `create_dir`이 미지원이고 `write`/`writer`가 '/'로 끝나는
/// 경로를 거부해, 공개 API로는 폴더 마커 객체를 만들 수 없다. 따라서
/// 폴더는 자식 객체로 만든다(목록 결과는 마커 폴더와 동일하다).
/// `empty-folder/` 마커는 seed-minio.sh가 이미 만들어 두었으므로
/// 마커 관련 검증은 그 객체를 사용한다.
async fn seed(operator: &Operator) -> Result<(), opendal::Error> {
    // 마지막에 쓰는 객체가 있으면 이전 실행에서 완료된 것으로 본다.
    if operator.stat("docs/p1-bugcheck/n/x.txt").await.is_ok() {
        return Ok(());
    }
    // 파일 2,500개 + 하위 폴더 3개(d1~d3 각각 자식 객체 1개씩).
    for i in 0..2500u32 {
        let key = format!("docs/p1-paging/f{i:04}.txt");
        operator
            .write(&key, format!("paging {i}").into_bytes())
            .await?;
    }
    operator
        .write("docs/p1-paging/d1/x.txt", b"x".to_vec())
        .await?;
    operator
        .write("docs/p1-paging/d2/y.txt", b"y".to_vec())
        .await?;
    operator
        .write("docs/p1-paging/d3/z.txt", b"z".to_vec())
        .await?;

    // S3 응답 경계에서 항목이 누락되던 버그의 재현 구성.
    // (파일 a/b/c.txt 사이에 CommonPrefix m/, n/이 끼어드는 형태)
    for name in ["a.txt", "b.txt", "c.txt"] {
        let key = format!("docs/p1-bugcheck/{name}");
        operator.write(&key, b"x".to_vec()).await?;
    }
    operator
        .write("docs/p1-bugcheck/m/x.txt", b"x".to_vec())
        .await?;
    operator
        .write("docs/p1-bugcheck/n/x.txt", b"x".to_vec())
        .await?;
    Ok(())
}

fn open_conn(cache_dir: &Path) -> Connection {
    Connection::open(&minio_config(), "minioadmin", cache_dir).expect("연결 생성 실패")
}

/// prefix 아래 모든 페이지를 순회해 (페이지 크기 목록, 전체 키, 캐시 여부)를 모은다.
async fn collect_all(conn: &Connection, prefix: &str) -> (Vec<usize>, Vec<String>, Vec<bool>) {
    let mut page_sizes = Vec::new();
    let mut keys = Vec::new();
    let mut cache_flags = Vec::new();
    let mut cursor = None;
    loop {
        let page = conn
            .list_folder(prefix, cursor.as_deref())
            .await
            .expect("페이지 조회 실패");
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

#[test]
fn root_first_page_minio() {
    if !minio_enabled() {
        return;
    }
    let _cache = TempDir::new("root");
    runtime().block_on(async {
        let conn = open_conn(&_cache.0);
        let page = conn
            .list_folder("", None)
            .await
            .expect("루트 목록 조회 실패");

        let keys: Vec<&str> = page.items.iter().map(|item| item.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "docs/",
                "empty-folder/",
                "hello.txt",
                "photos/",
                "with space & special(1).txt",
                "한글 파일.txt",
            ]
        );
        assert!(page.next_cursor.is_none(), "루트는 한 페이지에 다 온다");
        assert!(!page.from_cache, "첫 호출은 원격 목록이다");

        // 두 번째 호출은 캐시에서 온다.
        let page = conn
            .list_folder("", None)
            .await
            .expect("캐시 목록 조회 실패");
        assert!(page.from_cache);
        assert_eq!(page.items.len(), 6);
    });
}

#[test]
fn paging_minio() {
    if !minio_enabled() {
        return;
    }
    let _cache = TempDir::new("paging");
    runtime().block_on(async {
        let operator = minio_operator();
        ensure_seeded(&operator).await;
        let conn = open_conn(&_cache.0);

        let (sizes, keys, flags) = collect_all(&conn, "docs/p1-paging/").await;

        // 파일 2,500 + 폴더 3 = 2,503개. 페이지는 1000/1000/503.
        assert_eq!(sizes, [1000, 1000, 503]);
        assert_eq!(keys.len(), 2503);
        // 첫 페이지만 원격, 이후는 캐시.
        assert_eq!(flags, [false, true, true]);

        // 중복 없이 키 오름차순으로 이어져야 한다.
        let unique: HashSet<&String> = keys.iter().collect();
        assert_eq!(unique.len(), keys.len(), "중복 키가 있다");
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(keys, sorted, "키 정렬 순서가 아니다");

        // 하위 폴더 3개가 모두 폴더로 나오고, 그 안의 객체는 상위 목록에 없다.
        let page_items: Vec<Item> = {
            // 검증용으로 전체 항목을 다시 모은다(캐시라 비용 없음).
            let mut all = Vec::new();
            let mut cursor = None;
            loop {
                let page = conn
                    .list_folder("docs/p1-paging/", cursor.as_deref())
                    .await
                    .expect("페이지 조회 실패");
                all.extend(page.items);
                match page.next_cursor {
                    Some(next) => cursor = Some(next),
                    None => break,
                }
            }
            all
        };
        for dir in ["d1/", "d2/", "d3/"] {
            let key = format!("docs/p1-paging/{dir}");
            let item = page_items
                .iter()
                .find(|item| item.key == key)
                .unwrap_or_else(|| panic!("폴더가 없다: {key}"));
            assert!(item.is_dir, "{key}가 폴더가 아니다");
            assert_eq!(item.size, 0);
        }
        assert!(
            !page_items.iter().any(|item| item.key.ends_with("/x.txt")
                || item.key.ends_with("/y.txt")
                || item.key.ends_with("/z.txt")),
            "하위 객체가 상위 목록에 나타났다"
        );

        // 두 번째 순회는 전부 캐시에서 온다.
        let (sizes2, keys2, flags2) = collect_all(&conn, "docs/p1-paging/").await;
        assert_eq!(sizes2, [1000, 1000, 503]);
        assert_eq!(keys2, keys);
        assert_eq!(flags2, [true, true, true]);
    });
}

#[test]
fn bugcheck_regression_minio() {
    if !minio_enabled() {
        return;
    }
    let _cache = TempDir::new("bugcheck");
    runtime().block_on(async {
        let operator = minio_operator();
        ensure_seeded(&operator).await;
        let conn = open_conn(&_cache.0);

        // S3 응답 경계에서 c.txt가 누락되던 재현 케이스.
        // 파일 3개 사이에 CommonPrefix 폴더 2개가 끼어드는 형태다.
        let page = conn
            .list_folder("docs/p1-bugcheck/", None)
            .await
            .expect("bugcheck 목록 조회 실패");
        let keys: Vec<&str> = page.items.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "docs/p1-bugcheck/a.txt",
                "docs/p1-bugcheck/b.txt",
                "docs/p1-bugcheck/c.txt",
                "docs/p1-bugcheck/m/",
                "docs/p1-bugcheck/n/",
            ]
        );
        assert!(page.next_cursor.is_none());
        assert!(!page.from_cache);
        assert!(page.items[3].is_dir && page.items[4].is_dir);
        assert!(!page.items[0].is_dir);

        // 두 번째 호출은 캐시에서 같은 결과.
        let page = conn
            .list_folder("docs/p1-bugcheck/", None)
            .await
            .expect("캐시 목록 조회 실패");
        assert!(page.from_cache);
        assert_eq!(page.items.len(), 5);
    });
}

#[test]
fn marker_only_folder_lists_empty_minio() {
    if !minio_enabled() {
        return;
    }
    let _cache = TempDir::new("marker");
    runtime().block_on(async {
        let conn = open_conn(&_cache.0);
        // empty-folder/는 0바이트 마커 객체만 있다(seed-minio.sh).
        // 폴더를 열면 자기 자신 마커는 제외돼 빈 페이지가 온다.
        let page = conn
            .list_folder("empty-folder/", None)
            .await
            .expect("empty-folder 목록 조회 실패");
        assert!(page.items.is_empty());
        assert!(page.next_cursor.is_none());
    });
}

#[test]
fn stat_and_read_minio() {
    if !minio_enabled() {
        return;
    }
    let _cache = TempDir::new("statread");
    runtime().block_on(async {
        let conn = open_conn(&_cache.0);

        let item = conn.stat("hello.txt").await.expect("stat 실패");
        assert_eq!(item.key, "hello.txt");
        assert_eq!(item.parent, "");
        assert_eq!(item.name, "hello.txt");
        assert!(!item.is_dir);
        assert_eq!(item.size, 17);

        // 0바이트 폴더 마커 객체는 폴더로 stat된다.
        let marker = conn
            .stat("empty-folder/")
            .await
            .expect("폴더 마커 stat 실패");
        assert!(marker.is_dir);
        assert_eq!(marker.size, 0);

        let err = conn.stat("없는키").await.expect_err("NotFound여야 한다");
        assert!(
            matches!(err, Error::NotFound(_)),
            "NotFound가 아니다: {err:?}"
        );

        // 한글 키 파일을 내려받아 크기와 내용을 검증한다.
        let dest =
            std::env::temp_dir().join(format!("pier-read-{}-korean.txt", std::process::id()));
        let written = conn
            .download(
                "한글 파일.txt",
                &dest,
                &TransferOptions::default(),
                &NoopProgress,
                &CancelToken::new(),
            )
            .await
            .expect("download 실패");
        assert_eq!(written, 49);
        let data = std::fs::read(&dest).expect("다운로드 파일 읽기 실패");
        assert_eq!(data.len(), 49);
        let _ = std::fs::remove_file(&dest);

        // 없는 키는 NotFound이고 부분 파일을 남기지 않는다.
        let missing_dest =
            std::env::temp_dir().join(format!("pier-read-{}-missing.txt", std::process::id()));
        let err = conn
            .download(
                "없는키",
                &missing_dest,
                &TransferOptions::default(),
                &NoopProgress,
                &CancelToken::new(),
            )
            .await
            .expect_err("NotFound여야 한다");
        assert!(
            matches!(err, Error::NotFound(_)),
            "NotFound가 아니다: {err:?}"
        );
        assert!(
            !missing_dest.exists(),
            "실패한 다운로드의 부분 파일이 남았다"
        );
    });
}

#[test]
fn refresh_detects_add_and_delete_minio() {
    if !minio_enabled() {
        return;
    }
    let _cache = TempDir::new("refresh");
    runtime().block_on(async {
        let operator = minio_operator();
        ensure_seeded(&operator).await;
        // 이전 실패 실행의 잔여물을 지워 멱등하게 만든다.
        let _ = operator.delete("docs/p2-refresh/a.txt").await;
        let _ = operator.delete("docs/p2-refresh/sub/b.txt").await;

        let conn = open_conn(&_cache.0);
        let prefix = "docs/p2-refresh/";

        // 대상 폴더가 아직 없으면 빈 목록으로 complete 표시된다.
        let changes = conn.refresh(prefix).await.expect("초기 refresh 실패");
        assert!(changes.upserted.is_empty());
        assert!(changes.removed.is_empty());

        // 파일 1개와 하위 폴더 안 파일 1개를 쓴다.
        operator
            .write("docs/p2-refresh/a.txt", b"a".to_vec())
            .await
            .expect("쓰기 실패");
        operator
            .write("docs/p2-refresh/sub/b.txt", b"b".to_vec())
            .await
            .expect("쓰기 실패");

        // refresh가 추가를 잡는다: a.txt와 CommonPrefix 폴더 sub/.
        let changes = conn.refresh(prefix).await.expect("refresh 실패");
        let upserted: HashSet<&str> = changes.upserted.iter().map(|i| i.key.as_str()).collect();
        assert!(upserted.contains("docs/p2-refresh/a.txt"), "{upserted:?}");
        assert!(upserted.contains("docs/p2-refresh/sub/"), "{upserted:?}");
        assert!(changes.removed.is_empty());

        // a.txt 삭제 → removed에 나타나고 캐시에서도 사라진다.
        operator
            .delete("docs/p2-refresh/a.txt")
            .await
            .expect("삭제 실패");
        let changes = conn.refresh(prefix).await.expect("refresh 실패");
        assert_eq!(changes.removed, ["docs/p2-refresh/a.txt"]);
        assert!(
            !changes
                .upserted
                .iter()
                .any(|i| i.key == "docs/p2-refresh/sub/"),
            "변하지 않은 폴더가 upserted에 들어갔다"
        );
        let err = conn
            .stat("docs/p2-refresh/a.txt")
            .await
            .expect_err("캐시에 없어야 한다");
        assert!(matches!(err, Error::NotFound(_)), "{err:?}");

        // sub/b.txt 삭제 → sub/ 폴더 자체가 사라지고 하위 캐시도 정리된다.
        operator
            .delete("docs/p2-refresh/sub/b.txt")
            .await
            .expect("삭제 실패");
        let changes = conn.refresh(prefix).await.expect("refresh 실패");
        assert_eq!(changes.removed, ["docs/p2-refresh/sub/"]);
        let page = conn
            .list_folder(prefix, None)
            .await
            .expect("빈 폴더 목록 조회 실패");
        assert!(page.items.is_empty());
        assert!(page.from_cache, "갱신 후 캐시가 complete여야 한다");
    });
}

#[test]
fn test_connection_minio() {
    if !minio_enabled() {
        return;
    }
    let _cache = TempDir::new("testconn");
    let _bad_cache = TempDir::new("testconn-bad");
    runtime().block_on(async {
        // 올바른 시크릿 → 성공.
        let conn = open_conn(&_cache.0);
        conn.test_connection()
            .await
            .expect("test_connection이 실패했다");

        // 잘못된 시크릿 → Authentication 에러가 전파된다.
        let bad = Connection::open(&minio_config(), "wrong-secret", &_bad_cache.0)
            .expect("연결 생성 실패");
        let err = bad
            .test_connection()
            .await
            .expect_err("Authentication이어야 한다");
        assert!(
            matches!(err, Error::Authentication(_)),
            "Authentication이 아니다: {err:?}"
        );
    });
}
