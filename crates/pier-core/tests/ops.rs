//! PIER_TEST_MINIO=1일 때만 실행되는 객체 작업 통합 테스트.
//! tests/upload.rs와 같은 패턴(공유 런타임·Operator 시드·TempDir).
//! 테스트마다 docs/p3-ops/ 아래 고유한 하위 접두사를 써서 병렬 실행 시
//! 서로의 객체를 지우지 않게 하고, 각 테스트 끝에서 자기 접두사만 정리한다.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

use opendal::services::S3;
use opendal::Operator;
use pier_core::config::{ConnectionConfig, Provider};
use pier_core::enumerate::Connection;
use pier_core::error::Error;
use pier_core::transfer::{CancelToken, ProgressSink, TransferOptions, UploadMode};
use pier_core::{Backend, OpendalS3};

/// 모든 테스트 객체는 docs/p3-ops/<테스트명>/ 아래에 만든다.
const P3: &str = "docs/p3-ops/";

fn minio_enabled() -> bool {
    std::env::var("PIER_TEST_MINIO").as_deref() == Ok("1")
}

fn big_enabled() -> bool {
    std::env::var("PIER_TEST_BIG").as_deref() == Ok("1")
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
    }
}

/// 시드·검증용 Operator. 백엔드와 달리 루트가 버킷 루트다.
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

/// opendal의 전역 HTTP 클라이언트가 만든 런타임에 묶이므로 하나를 공유한다
/// (tests/upload.rs와 같은 이유).
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

/// 테스트마다 고유한 임시 디렉터리. Drop에서 지운다.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "pier-ops-it-{}-{}-{tag}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("임시 디렉터리 생성 실패");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn open_connection(cache: &TempDir) -> Connection {
    Connection::open(&minio_config(), "minioadmin", &cache.0).expect("연결 생성 실패")
}

fn open_backend() -> OpendalS3 {
    OpendalS3::new(&minio_config(), "minioadmin").expect("백엔드 생성 실패")
}

/// 진행률 호출을 기록하지 않는 싱크.
struct RecProgress;

impl ProgressSink for RecProgress {
    fn on_progress(&self, _transferred: u64, _total: u64) {}
}

/// prefix 아래 객체를 전부 지운다(테스트 정리용).
async fn cleanup_prefix(operator: &Operator, prefix: &str) {
    let entries = operator
        .list_with(prefix)
        .recursive(true)
        .await
        .expect("정리 목록 실패");
    let paths: Vec<String> = entries.iter().map(|e| e.path().to_string()).collect();
    if !paths.is_empty() {
        operator.delete_iter(paths).await.expect("정리 삭제 실패");
    }
}

/// prefix/ 아래 count개 파일을 빠르게 시드한다.
async fn seed_files(operator: &Operator, prefix: &str, count: usize) {
    for chunk in (0..count).collect::<Vec<_>>().chunks(100) {
        let mut set = tokio::task::JoinSet::new();
        for i in chunk {
            let op = operator.clone();
            let key = format!("{prefix}f{i:04}.txt");
            set.spawn(async move {
                op.write(&key, b"x".to_vec()).await.expect("시드 쓰기 실패");
            });
        }
        while set.join_next().await.is_some() {}
    }
}

#[test]
fn create_folder_list_existing_conflict_minio() {
    if !minio_enabled() {
        return;
    }
    let prefix = format!("{P3}mkdir/");
    runtime().block_on(async {
        let operator = minio_operator();
        cleanup_prefix(&operator, &prefix).await;
        let cache = TempDir::new("mkdir-cache");
        let conn = open_connection(&cache);

        let item = conn
            .create_folder(&prefix, "newdir")
            .await
            .expect("폴더 생성 실패");
        assert_eq!(item.key, format!("{prefix}newdir/"));
        assert!(item.is_dir);

        // 목록에 폴더로 보인다.
        let page = conn.list_folder(&prefix, None).await.expect("목록 실패");
        let found = page
            .items
            .iter()
            .find(|i| i.key == format!("{prefix}newdir/"))
            .expect("생성한 폴더가 목록에 없다");
        assert!(found.is_dir);

        // existing은 폴더도 이름 끝 '/' 없이 반환한다.
        let names = vec!["newdir".to_string(), "none".to_string()];
        let found = conn.existing(&prefix, &names).await.expect("existing 실패");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].key, format!("{prefix}newdir/"));
        assert_eq!(found[0].name, "newdir");
        assert!(found[0].is_dir);

        // 같은 이름 재생성은 Conflict.
        let err = conn
            .create_folder(&prefix, "newdir")
            .await
            .expect_err("Conflict여야 한다");
        assert!(matches!(err, Error::Conflict(_)), "{err:?}");

        cleanup_prefix(&operator, &prefix).await;
    });
}

#[test]
fn rename_file_move_minio() {
    if !minio_enabled() {
        return;
    }
    let prefix = format!("{P3}mvfile/");
    runtime().block_on(async {
        let operator = minio_operator();
        cleanup_prefix(&operator, &prefix).await;
        let cache = TempDir::new("mvfile-cache");
        let conn = open_connection(&cache);
        let backend = open_backend();

        let src_key = format!("{prefix}from/old.txt");
        let dst_parent = format!("{prefix}to/");
        let dst_key = format!("{dst_parent}new.txt");
        operator
            .write(&src_key, b"hello pier".to_vec())
            .await
            .expect("시드 쓰기 실패");

        // 이동 + 이름 변경을 한 번에.
        let item = conn
            .rename(&src_key, &dst_parent, "new.txt")
            .await
            .expect("rename 실패");
        assert_eq!(item.key, dst_key);
        assert_eq!(item.size, 10);

        // 원본은 없고 대상 내용은 그대로다.
        assert!(
            matches!(conn.stat(&src_key).await, Err(Error::NotFound(_))),
            "원본이 남았다"
        );
        let chunk = backend
            .read_range(&dst_key, 0, 10)
            .await
            .expect("read_range 실패");
        assert_eq!(chunk, b"hello pier");

        cleanup_prefix(&operator, &prefix).await;
    });
}

#[test]
fn rename_file_conflict_minio() {
    if !minio_enabled() {
        return;
    }
    let prefix = format!("{P3}mvconf/");
    runtime().block_on(async {
        let operator = minio_operator();
        cleanup_prefix(&operator, &prefix).await;
        let cache = TempDir::new("mvconf-cache");
        let conn = open_connection(&cache);

        operator
            .write(&format!("{prefix}a.txt"), b"1".to_vec())
            .await
            .expect("시드 쓰기 실패");
        operator
            .write(&format!("{prefix}b.txt"), b"2".to_vec())
            .await
            .expect("시드 쓰기 실패");

        let err = conn
            .rename(&format!("{prefix}a.txt"), &prefix, "b.txt")
            .await
            .expect_err("Conflict여야 한다");
        assert!(matches!(err, Error::Conflict(_)), "{err:?}");

        // 원본은 그대로 남아 있다.
        let backend = open_backend();
        assert!(backend.stat(&format!("{prefix}a.txt")).await.is_ok());
        assert!(backend.stat(&format!("{prefix}b.txt")).await.is_ok());

        cleanup_prefix(&operator, &prefix).await;
    });
}

#[test]
fn rename_folder_30_minio() {
    if !minio_enabled() {
        return;
    }
    let prefix = format!("{P3}r30/");
    runtime().block_on(async {
        let operator = minio_operator();
        cleanup_prefix(&operator, &prefix).await;
        let cache = TempDir::new("r30-cache");
        let conn = open_connection(&cache);
        let backend = open_backend();

        // 마커가 있는 폴더 + 하위 30개.
        conn.create_folder(&prefix, "src")
            .await
            .expect("폴더 생성 실패");
        seed_files(&operator, &format!("{prefix}src/"), 30).await;

        let item = conn
            .rename(&format!("{prefix}src/"), &prefix, "dst")
            .await
            .expect("폴더 rename 실패");
        assert_eq!(item.key, format!("{prefix}dst/"));
        assert!(item.is_dir);

        // 원본은 마커·하위 모두 없고, 대상은 마커 + 30개가 완전하다.
        assert!(
            backend
                .list_recursive(&format!("{prefix}src/"))
                .await
                .expect("목록 실패")
                .is_empty(),
            "원본 폴더가 남았다"
        );
        let dst_items = backend
            .list_recursive(&format!("{prefix}dst/"))
            .await
            .expect("목록 실패");
        assert_eq!(dst_items.len(), 30, "{dst_items:?}");
        assert!(dst_items.iter().all(|i| !i.is_dir));

        // 부모 목록에 dst/만 보인다.
        let page = conn.list_folder(&prefix, None).await.expect("목록 실패");
        let keys: Vec<&str> = page.items.iter().map(|i| i.key.as_str()).collect();
        assert!(keys.contains(&format!("{prefix}dst/").as_str()), "{keys:?}");
        assert!(!keys.iter().any(|k| k.contains("src/")), "{keys:?}");

        cleanup_prefix(&operator, &prefix).await;
    });
}

#[test]
fn rename_folder_over_limit_minio() {
    if !minio_enabled() {
        return;
    }
    let prefix = format!("{P3}r1001/");
    runtime().block_on(async {
        let operator = minio_operator();
        cleanup_prefix(&operator, &prefix).await;
        let cache = TempDir::new("r1001-cache");
        let conn = open_connection(&cache);
        let backend = open_backend();

        conn.create_folder(&prefix, "big")
            .await
            .expect("폴더 생성 실패");
        // 마커 제외 1,001개 → 한도 초과.
        seed_files(&operator, &format!("{prefix}big/"), 1001).await;

        let err = conn
            .rename(&format!("{prefix}big/"), &prefix, "big2")
            .await
            .expect_err("한도 초과는 거부돼야 한다");
        assert!(matches!(err, Error::Other(_)), "{err:?}");
        assert!(err.to_string().contains("1,000"), "{err}");

        // 아무것도 옮겨지지 않았다.
        let items = backend
            .list_recursive(&format!("{prefix}big/"))
            .await
            .expect("목록 실패");
        assert_eq!(items.len(), 1001);
        assert!(backend
            .list_recursive(&format!("{prefix}big2/"))
            .await
            .expect("목록 실패")
            .is_empty());

        cleanup_prefix(&operator, &prefix).await;
    });
}

#[test]
fn delete_folder_minio() {
    if !minio_enabled() {
        return;
    }
    let prefix = format!("{P3}del/");
    runtime().block_on(async {
        let operator = minio_operator();
        cleanup_prefix(&operator, &prefix).await;
        let cache = TempDir::new("del-cache");
        let conn = open_connection(&cache);
        let backend = open_backend();

        conn.create_folder(&prefix, "victim")
            .await
            .expect("폴더 생성 실패");
        seed_files(&operator, &format!("{prefix}victim/"), 25).await;
        // 다른 폴더는 건드리지 않아야 한다.
        operator
            .write(&format!("{prefix}keep.txt"), b"k".to_vec())
            .await
            .expect("시드 쓰기 실패");

        conn.delete(&format!("{prefix}victim/"))
            .await
            .expect("폴더 삭제 실패");

        assert!(
            backend
                .list_recursive(&format!("{prefix}victim/"))
                .await
                .expect("목록 실패")
                .is_empty(),
            "폴더 하위가 남았다"
        );
        assert!(backend.stat(&format!("{prefix}keep.txt")).await.is_ok());

        cleanup_prefix(&operator, &prefix).await;
    });
}

#[test]
fn test_connection_minio() {
    if !minio_enabled() {
        return;
    }
    runtime().block_on(async {
        let cache = TempDir::new("tc-cache");
        let conn = open_connection(&cache);
        // 목록 + 쓰기 권한 확인. minioadmin은 쓰기가 되므로 true.
        let can_write = conn.test_connection().await.expect("연결 실패");
        assert!(can_write, "can_write가 false다");
    });
}

/// PIER_TEST_BIG=1일 때만 실행. 5.1GB 파일을 업로드한 뒤 서버 측 복사로
/// 이름 변경·이동하고 원본이 지워지는지 확인한다.
#[test]
fn rename_big_file_minio() {
    if !minio_enabled() || !big_enabled() {
        return;
    }
    let prefix = format!("{P3}big/");
    let dir = TempDir::new("big-src");
    runtime().block_on(async {
        let operator = minio_operator();
        cleanup_prefix(&operator, &prefix).await;
        let cache = TempDir::new("big-cache");
        let conn = open_connection(&cache);
        let backend = open_backend();

        // 희소 파일: 약 5.1GB 논리 크기, 실제 블록은 거의 없다.
        let size: u64 = 5_368_709_120; // 5.0 GiB
        let src = dir.0.join("big.bin");
        {
            let f = std::fs::File::create(&src).expect("파일 생성 실패");
            f.set_len(size).expect("set_len 실패");
        }

        let start = std::time::Instant::now();
        let item = conn
            .upload(
                &src,
                &prefix,
                "big.bin",
                &UploadMode::Fail,
                &TransferOptions::default(),
                &RecProgress,
                &CancelToken::new(),
            )
            .await
            .expect("5.1GB 업로드 실패");
        eprintln!("5.1GB 업로드: {:.1}s", start.elapsed().as_secs_f64());
        let src_key = item.key;

        // 다른 부모로 옮기며 이름 변경. 5GB 초과라 백엔드가 멀티파트 복사한다.
        let dst_parent = format!("{prefix}moved/");
        let dst_key = format!("{dst_parent}big-renamed.bin");
        let start = std::time::Instant::now();
        let moved = conn
            .rename(&src_key, &dst_parent, "big-renamed.bin")
            .await
            .expect("5.1GB rename 실패");
        assert_eq!(moved.key, dst_key);
        eprintln!("5.1GB rename: {:.1}s", start.elapsed().as_secs_f64());

        // 원본은 지워졌고 대상 크기는 같다.
        assert!(
            matches!(conn.stat(&src_key).await, Err(Error::NotFound(_))),
            "원본이 남았다"
        );
        let st = backend.stat(&dst_key).await.expect("대상 stat 실패");
        assert_eq!(st.size, size, "대상 크기 불일치");

        cleanup_prefix(&operator, &prefix).await;
    });
}
