//! 통합 테스트 공용 헬퍼. 대상 엔드포인트를 환경변수로 지정한다.
//!
//! - 기본: `PIER_TEST_MINIO=1`이면 로컬 MinIO(127.0.0.1:9000, pier-test,
//!   minioadmin/minioadmin)를 대상으로 한다(기존 동작과 같다).
//!   `seed-minio.sh`의 루트 시드 데이터(hello.txt, empty-folder/ 등)에
//!   의존하는 테스트는 `local_minio()`로만 통과시켜 원격에서 건너뛴다.
//! - 원격: `PIER_TEST_BUCKET`을 설정하면 S3 호환 대상으로 바뀐다.
//!   `PIER_TEST_ENDPOINT`(없으면 AWS), `PIER_TEST_REGION`,
//!   `PIER_TEST_ACCESS_KEY`, `PIER_TEST_SECRET_KEY`,
//!   `PIER_TEST_PATH_STYLE`(1/true), `PIER_TEST_PREFIX`(기본
//!   `pier-compat-test/`)로 대상을 지정한다. 모든 테스트 객체는
//!   연결 루트를 PIER_TEST_PREFIX로 잡아 그 아래에만 만들고,
//!   끝에는 그 접두사만 정리한다.

// 이 모듈은 테스트 바이너리마다 따로 컴파일되므로, 어떤 헬퍼든
// 그 바이너리에서 안 쓰이는 것이 있을 수 있다.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

use opendal::services::S3;
use opendal::Operator;
use pier_core::config::{ConnectionConfig, Provider};
use pier_core::enumerate::Connection;
use pier_core::s3_raw::S3Raw;
use pier_core::transfer::ProgressSink;
use pier_core::OpendalS3;

/// 환경변수로 결정된 테스트 대상. 프로세스당 한 번 읽는다.
pub struct Target {
    /// PIER_TEST_BUCKET이 있으면 원격 모드다.
    pub remote: bool,
    /// 원격 엔드포인트. None이면 AWS 기본값을 쓴다.
    pub endpoint: Option<String>,
    pub region: String,
    pub bucket: String,
    pub access_key: String,
    pub secret: String,
    /// path-style 요청 여부. 원격 기본값은 가상 호스트다.
    pub path_style: bool,
    /// 테스트 객체 루트. MinIO는 "", 원격은 "a/b/" 형태다.
    pub prefix: String,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

pub fn target() -> &'static Target {
    static TARGET: OnceLock<Target> = OnceLock::new();
    TARGET.get_or_init(|| {
        let remote = env("PIER_TEST_BUCKET").is_some();
        let prefix = if remote {
            pier_core::key::normalize_prefix(
                &env("PIER_TEST_PREFIX").unwrap_or_else(|| "pier-compat-test/".to_string()),
            )
        } else {
            String::new()
        };
        Target {
            remote,
            endpoint: env("PIER_TEST_ENDPOINT")
                .or_else(|| (!remote).then(|| "http://127.0.0.1:9000".to_string())),
            region: env("PIER_TEST_REGION").unwrap_or_else(|| "us-east-1".to_string()),
            bucket: env("PIER_TEST_BUCKET").unwrap_or_else(|| "pier-test".to_string()),
            access_key: env("PIER_TEST_ACCESS_KEY").unwrap_or_else(|| "minioadmin".to_string()),
            secret: env("PIER_TEST_SECRET_KEY").unwrap_or_else(|| "minioadmin".to_string()),
            path_style: env("PIER_TEST_PATH_STYLE")
                .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
                .unwrap_or(!remote),
            prefix,
        }
    })
}

/// 테스트를 실행할 대상이 설정돼 있는가.
/// 로컬 MinIO는 PIER_TEST_MINIO=1, 원격은 PIER_TEST_BUCKET으로 켠다.
pub fn enabled() -> bool {
    std::env::var("PIER_TEST_MINIO").as_deref() == Ok("1") || target().remote
}

/// 로컬 MinIO 대상인가. 루트 시드 데이터(seed-minio.sh)에 의존하는
/// 테스트는 이것이 참일 때만 실행한다 — 원격에서는 항상 거짓이다.
pub fn local_minio() -> bool {
    enabled() && !target().remote
}

/// PIER_TEST_BIG=1이면 대용량(5GB급) 테스트를 켠다.
pub fn big_enabled() -> bool {
    std::env::var("PIER_TEST_BIG").as_deref() == Ok("1")
}

/// 대상에 맞는 연결 설정. 원격이면 루트를 PIER_TEST_PREFIX로 잡는다.
pub fn config() -> ConnectionConfig {
    let t = target();
    ConnectionConfig {
        id: String::new(),
        display_name: "pier-test".to_string(),
        provider: if t.remote {
            Provider::Custom
        } else {
            Provider::MinIO
        },
        endpoint: t.endpoint.clone(),
        region: t.region.clone(),
        bucket: t.bucket.clone(),
        prefix: (!t.prefix.is_empty()).then(|| t.prefix.clone()),
        path_style: t.path_style,
        access_key_id: t.access_key.clone(),
        chunk_size_mb: None,
        concurrency: None,
    }
}

pub fn secret() -> &'static str {
    &target().secret
}

/// 시드·검증용 Operator. 루트 규칙은 config()와 같아 상대 키가
/// MinIO에서는 버킷 루트, 원격에서는 PIER_TEST_PREFIX 아래에 놓인다.
pub fn operator() -> Operator {
    let t = target();
    let mut builder = S3::default()
        .bucket(&t.bucket)
        .region(&t.region)
        .access_key_id(&t.access_key)
        .secret_access_key(&t.secret)
        .disable_config_load()
        .disable_ec2_metadata()
        .root(&format!("/{}", t.prefix));
    if let Some(endpoint) = &t.endpoint {
        builder = builder.endpoint(endpoint);
    }
    // OpenDAL 기본값이 path-style이다. OpendalS3와 같은 규칙으로
    // 가상 호스트 스타일을 켠다(점 포함 버킷은 path-style 유지).
    if !t.path_style && !t.bucket.contains('.') {
        builder = builder.enable_virtual_host_style();
    }
    opendal::install_default();
    Operator::new(builder).expect("테스트 Operator 생성 실패")
}

/// 연결 루트 기준 상대 키를 버킷 절대 키로 바꾼다.
/// 원격에선 PIER_TEST_PREFIX를 붙인다(ListMultipartUploads처럼 버킷
/// 루트 기준 키가 오는 응답과 비교할 때 쓴다).
pub fn bucket_key(key: &str) -> String {
    format!("{}{}", target().prefix, key)
}

/// `GET bucket?uploads`의 요청 URL을 대상 규칙으로 만든다.
/// s3_raw::S3Raw::build_url과 같은 규칙(빈 쿼리 값은 '=' 없이).
pub fn list_uploads_url() -> String {
    let t = target();
    let base = match &t.endpoint {
        Some(e) => e.trim_end_matches('/').to_string(),
        // S3Raw::new의 AWS 기본 엔드포인트 템플릿과 같다.
        None => format!("https://s3.{}.amazonaws.com", t.region),
    };
    if t.path_style || t.bucket.contains('.') {
        format!("{base}/{}/?uploads", t.bucket)
    } else {
        format!(
            "{}/?uploads",
            base.replacen("://", &format!("://{}.", t.bucket), 1)
        )
    }
}

/// 모든 통합 테스트가 공유하는 단일 런타임.
/// #[tokio::test]는 테스트마다 별도 런타임을 만들고, opendal의 전역 HTTP
/// 클라이언트는 연결 태스크를 만든 런타임에 묶인다. 런타임이 먼저
/// 종료되면 "dispatch task is gone" 오류가 나므로 하나를 공유한다.
pub fn runtime() -> &'static tokio::runtime::Runtime {
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
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "pier-it-{}-{}-{tag}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("임시 디렉터리 생성 실패");
        Self(path)
    }

    pub fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    /// 패턴이 있는(size 만큼의) 테스트 파일을 만든다.
    pub fn write_file(&self, name: &str, size: usize) -> PathBuf {
        let path = self.0.join(name);
        let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).expect("테스트 파일 쓰기 실패");
        path
    }

    /// `.name.pierdownload` 임시 파일이 최상위에 남아 있으면 true.
    pub fn has_temp_file(&self) -> bool {
        std::fs::read_dir(&self.0)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .any(|e| e.file_name().to_string_lossy().contains("pierdownload"))
            })
            .unwrap_or(false)
    }

    /// `.name.pierdownload` 임시 파일 전체(하위 포함).
    pub fn temp_files(&self) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut stack = vec![self.0.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in rd.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if entry.file_name().to_string_lossy().contains("pierdownload") {
                    found.push(path);
                }
            }
        }
        found
    }

    /// 하위를 포함해 임시 파일이 남아 있으면 true.
    pub fn has_temp_file_deep(&self) -> bool {
        !self.temp_files().is_empty()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 진행률을 무시하는 테스트용 싱크.
pub struct Nop;

impl ProgressSink for Nop {
    fn on_progress(&self, _transferred: u64, _total: u64) {}
}

pub fn open_backend() -> OpendalS3 {
    OpendalS3::new(&config(), secret()).expect("백엔드 생성 실패")
}

pub fn open_conn(cache_dir: &Path) -> Connection {
    Connection::open(&config(), secret(), cache_dir).expect("연결 생성 실패")
}

pub fn open_raw() -> S3Raw {
    S3Raw::new(&config(), secret()).expect("S3Raw 생성 실패")
}

/// prefix 아래 객체를 전부 지운다(테스트 정리용).
pub async fn cleanup_prefix(operator: &Operator, prefix: &str) {
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
pub async fn seed_files(operator: &Operator, prefix: &str, count: usize) {
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
