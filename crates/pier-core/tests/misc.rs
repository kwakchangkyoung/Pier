//! PIER_TEST_MINIO=1일 때 MinIO로 실행되는 Phase 4 편의 기능 통합 테스트
//! (presigned URL, 프리페치, 미리보기 캐시).
//! 프리셋·presign 만료 검증·로그 싱크 설치는 MinIO 없이도 실행한다.
//! 다른 통합 테스트 파일들과 같은 패턴(공유 런타임, 임시 캐시 디렉터리)을 따른다.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use opendal::services::S3;
use opendal::Operator;
use pier_core::config::{preset_endpoint, presets, ConnectionConfig, Provider};
use pier_core::enumerate::Connection;
use pier_core::error::Error;
use pier_core::logging::{self, LogSink};
use pier_core::preview::preview_file;
use pier_core::transfer::{CancelToken, ProgressSink};
use pier_core::{Backend, OpendalS3};

/// 테스트 객체는 모두 이 접두사 아래에 두고 끝에 지운다.
const ROOT: &str = "docs/p4-misc";
const SECRET: &str = "minioadmin";

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

/// 시드·정리용 Operator. 백엔드와 달리 루트가 버킷 루트다.
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
/// (minio_list.rs와 같은 이유).
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
            "pier-misc-it-{}-{}-{tag}",
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

/// 아무것도 하지 않는 진행률 싱크.
struct Nop;

impl ProgressSink for Nop {
    fn on_progress(&self, _transferred: u64, _total: u64) {}
}

/// http:// URL을 std TcpStream으로 GET해 응답 원문을 반환한다.
/// reqwest 없이 presigned URL을 검증하기 위한 최소 HTTP 클라이언트다.
fn http_get(url: &str) -> String {
    let rest = url
        .strip_prefix("http://")
        .expect("이 테스트는 http URL만 다룬다");
    let (authority, path_query) = match rest.split_once('/') {
        Some((a, p)) => (a.to_string(), format!("/{p}")),
        None => (rest.to_string(), "/".to_string()),
    };
    let mut stream = TcpStream::connect(&authority).expect("MinIO 연결 실패");
    let request =
        format!("GET {path_query} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .expect("요청 쓰기 실패");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("응답 읽기 실패");
    String::from_utf8_lossy(&response).into_owned()
}

// ===== MinIO 없이도 실행되는 테스트 =====

#[test]
fn presets_and_endpoints() {
    // 5개 프리셋이 5개 Provider와 1:1로 대응한다.
    assert_eq!(presets().len(), 5);
    for provider in [
        Provider::Aws,
        Provider::R2,
        Provider::Wasabi,
        Provider::MinIO,
        Provider::Custom,
    ] {
        assert!(presets().iter().any(|p| p.provider == provider));
    }

    // R2: 계정 ID로 엔드포인트 생성, 형식이 틀리면 None.
    let account = "a1".repeat(16);
    assert_eq!(
        preset_endpoint("r2", Some(&account), "auto").unwrap(),
        format!("https://{account}.r2.cloudflarestorage.com")
    );
    assert!(preset_endpoint("r2", Some("짧음"), "auto").is_none());

    // Wasabi: 리전으로 생성.
    assert_eq!(
        preset_endpoint("wasabi", None, "ap-northeast-2").unwrap(),
        "https://s3.ap-northeast-2.wasabisys.com"
    );

    // AWS·MinIO·커스텀·알 수 없는 id는 None.
    for id in ["aws", "minio", "custom", "없는id"] {
        assert!(preset_endpoint(id, None, "us-east-1").is_none(), "{id}");
    }
}

#[test]
fn presign_expiry_bounds() {
    // 서명 생성은 네트워크를 타지 않으므로 오프라인에서도 검증할 수 있다.
    let backend = OpendalS3::new(&minio_config(), SECRET).expect("백엔드 생성 실패");
    let rt = runtime();
    // 1초 미만·7일 초과는 원격 호출 전에 거절한다.
    assert!(rt
        .block_on(backend.presign_read("a.txt", Duration::ZERO))
        .is_err());
    assert!(rt
        .block_on(backend.presign_read("a.txt", Duration::from_secs(604_801)))
        .is_err());
    // URL에 서명이 들어가므로 값 자체는 로그·출력에 남기지 않는다.
    let url = rt
        .block_on(backend.presign_read("a.txt", Duration::from_secs(3600)))
        .expect("presign 실패");
    assert!(url.starts_with("http://127.0.0.1:9000/pier-test/"));
    assert!(url.contains("X-Amz-Signature="));
}

#[test]
fn log_sink_install_and_capture() {
    struct Capture(Arc<Mutex<Vec<(log::Level, String)>>>);

    impl LogSink for Capture {
        fn log(&self, level: log::Level, _target: &str, message: &str) {
            self.0.lock().unwrap().push((level, message.to_string()));
        }
    }

    let captured = Arc::new(Mutex::new(Vec::new()));
    logging::install(Box::new(Capture(Arc::clone(&captured)))).expect("첫 설치는 성공해야 한다");
    // 전역 로거는 한 번만 설치할 수 있다.
    assert!(logging::install(Box::new(Capture(Arc::clone(&captured)))).is_err());

    if minio_enabled() {
        runtime().block_on(async {
            let cache = TempDir::new("log");
            let conn = Connection::open(&minio_config(), SECRET, &cache.0).expect("연결 생성 실패");
            conn.list_folder(&format!("{ROOT}/"), None)
                .await
                .expect("목록 실패");
        });
    } else {
        log::info!("테스트 로그");
    }

    let captured = captured.lock().unwrap();
    assert!(!captured.is_empty(), "로그가 전달되지 않았다");
    for (level, message) in captured.iter() {
        // Info 이상만 전달된다.
        assert!(*level <= log::Level::Info, "{level}: {message}");
        // 시크릿·서명·객체 키는 로그에 남기지 않는다(SPEC 4.6).
        assert!(!message.contains(SECRET), "{message}");
        assert!(!message.contains("X-Amz-Signature"), "{message}");
        assert!(!message.contains(ROOT), "{message}");
    }
}

// ===== MinIO 통합 테스트 =====

#[test]
fn presigned_url_get_minio() {
    if !minio_enabled() {
        return;
    }
    runtime().block_on(async {
        let operator = minio_operator();
        let key = format!("{ROOT}/presign/hello.txt");
        operator
            .write(&key, "presigned hello")
            .await
            .expect("시드 쓰기 실패");

        let backend = OpendalS3::new(&minio_config(), SECRET).expect("백엔드 생성 실패");
        let url = backend
            .presign_read(&key, Duration::from_secs(300))
            .await
            .expect("presign 실패");
        let response = http_get(&url);
        let status = response.lines().next().unwrap_or_default().to_string();
        assert!(status.contains("200"), "상태: {status}");
        assert!(
            response.contains("presigned hello"),
            "본문이 없다: {status}"
        );

        operator
            .delete_iter(vec![key])
            .await
            .expect("정리 삭제 실패");
    });
}

#[test]
fn prefetch_minio() {
    if !minio_enabled() {
        return;
    }
    runtime().block_on(async {
        let operator = minio_operator();
        let cache = TempDir::new("prefetch");
        let conn = Connection::open(&minio_config(), SECRET, &cache.0).expect("연결 생성 실패");
        let paths = vec![
            format!("{ROOT}/pf/sub1/a.txt"),
            format!("{ROOT}/pf/sub2/b.txt"),
            format!("{ROOT}/pf/file.txt"),
        ];
        for path in &paths {
            operator.write(path, "x").await.expect("시드 쓰기 실패");
        }

        // 부모 목록을 먼저 받아 하위 폴더가 캐시에 생기게 한다.
        conn.list_folder(&format!("{ROOT}/pf/"), None)
            .await
            .expect("목록 실패");
        let fetched = conn
            .prefetch(&format!("{ROOT}/pf/"), 20, 2)
            .await
            .expect("프리페치 실패");
        assert_eq!(fetched, 2);

        // 하위 폴더가 원격 호출 없이 캐시에서 응답한다.
        assert!(
            conn.list_folder(&format!("{ROOT}/pf/sub1/"), None)
                .await
                .unwrap()
                .from_cache
        );
        assert!(
            conn.list_folder(&format!("{ROOT}/pf/sub2/"), None)
                .await
                .unwrap()
                .from_cache
        );
        // 다시 해도 새로 받을 것이 없다.
        assert_eq!(
            conn.prefetch(&format!("{ROOT}/pf/"), 20, 2).await.unwrap(),
            0
        );

        // max_folders 제한 확인(다른 폴더로).
        let extra = [format!("{ROOT}/pf2/s1/x"), format!("{ROOT}/pf2/s2/y")];
        for path in &extra {
            operator.write(path, "x").await.expect("시드 쓰기 실패");
        }
        conn.list_folder(&format!("{ROOT}/pf2/"), None)
            .await
            .unwrap();
        assert_eq!(
            conn.prefetch(&format!("{ROOT}/pf2/"), 1, 2).await.unwrap(),
            1
        );
        assert_eq!(
            conn.prefetch(&format!("{ROOT}/pf2/"), 20, 2).await.unwrap(),
            1
        );

        let mut all = paths;
        all.extend(extra);
        operator.delete_iter(all).await.expect("정리 삭제 실패");
    });
}

#[test]
fn preview_minio() {
    if !minio_enabled() {
        return;
    }
    runtime().block_on(async {
        let operator = minio_operator();
        let cache = TempDir::new("preview");
        let conn = Connection::open(&minio_config(), SECRET, &cache.0).expect("연결 생성 실패");
        let key = format!("{ROOT}/preview/미리보기 파일.txt");
        operator
            .write(&key, "미리보기 내용")
            .await
            .expect("시드 쓰기 실패");

        let path = preview_file(&conn, &key, &cache.0, &Nop, &CancelToken::new())
            .await
            .expect("미리보기 실패");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "미리보기 내용");
        assert!(path.parent().unwrap().join(".access").exists());

        // 캐시된 파일에 표시를 남기고 다시 호출하면 그대로 재사용한다
        // (다시 받았다면 표시가 덮어써진다).
        std::fs::write(&path, "재사용 표시").unwrap();
        let path2 = preview_file(&conn, &key, &cache.0, &Nop, &CancelToken::new())
            .await
            .expect("두 번째 미리보기 실패");
        assert_eq!(path2, path);
        assert_eq!(std::fs::read_to_string(&path2).unwrap(), "재사용 표시");

        // 폴더 키는 거부한다.
        let err = preview_file(
            &conn,
            &format!("{ROOT}/"),
            &cache.0,
            &Nop,
            &CancelToken::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, Error::Other(_)), "{err:?}");

        operator
            .delete_iter(vec![key])
            .await
            .expect("정리 삭제 실패");
    });
}
