//! PIER_TEST_MINIO=1 또는 원격 대상(PIER_TEST_BUCKET 등)에서 실행되는
//! 업로드 통합 테스트. 공용 설정·헬퍼는 tests/common/mod.rs를 본다.
//! 테스트마다 docs/p3-upload/ 아래 고유한 하위 접두사를 써서 병렬 실행 시
//! 서로의 객체를 지우지 않게 하고, 각 테스트 끝에서 자기 접두사만 정리한다
//! (원격에서는 PIER_TEST_PREFIX 아래에 놓인다).

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use pier_core::error::Error;
use pier_core::transfer::{self, CancelToken, ProgressSink, TransferOptions, UploadMode};
use pier_core::{Backend, OpendalS3};

use common::{big_enabled, cleanup_prefix, enabled, open_backend, runtime, TempDir};

/// 모든 테스트 객체는 docs/p3-upload/<테스트명>/ 아래에 만든다.
const P3: &str = "docs/p3-upload/";

/// 진행률 호출을 기록하지 않는 싱크.
struct RecProgress;

impl ProgressSink for RecProgress {
    fn on_progress(&self, _transferred: u64, _total: u64) {}
}

/// n번째 진행률 콜백에서 취소하는 싱크(업로드 중간 취소 재현용).
struct CancelAfter {
    token: CancelToken,
    calls: AtomicUsize,
    after: usize,
}

impl ProgressSink for CancelAfter {
    fn on_progress(&self, _transferred: u64, _total: u64) {
        if self.calls.fetch_add(1, Ordering::SeqCst) + 1 >= self.after {
            self.token.cancel();
        }
    }
}

/// 결정적 패턴의 로컬 파일을 만든다.
fn make_file(dir: &TempDir, name: &str, size: usize) -> PathBuf {
    let path = dir.0.join(name);
    let data: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
    std::fs::write(&path, data).expect("로컬 파일 생성 실패");
    path
}

/// 업로드한 객체의 내용을 읽어 expected와 비교한다.
async fn assert_remote_bytes(backend: &OpendalS3, key: &str, expected: &[u8]) {
    let item = backend.stat(key).await.expect("stat 실패");
    assert!(!item.is_dir, "{key}가 폴더다");
    assert_eq!(item.size, expected.len() as u64, "{key} 크기 불일치");
    let mut got = Vec::with_capacity(expected.len());
    let mut offset = 0u64;
    while offset < expected.len() as u64 {
        let len = (expected.len() as u64 - offset).min(4 * 1024 * 1024);
        let chunk = backend
            .read_range(key, offset, len)
            .await
            .expect("read_range 실패");
        got.extend_from_slice(&chunk);
        offset += len;
    }
    assert_eq!(got, expected, "{key} 내용이 원본과 다르다");
}

#[test]
fn upload_small_file_minio() {
    if !enabled() {
        return;
    }
    let prefix = format!("{P3}small/");
    let dir = TempDir::new("small");
    runtime().block_on(async {
        let operator = common::operator();
        cleanup_prefix(&operator, &prefix).await;
        let backend = open_backend();
        let key = format!("{prefix}small.bin");
        let src = make_file(&dir, "small.bin", 100 * 1024);

        let item = transfer::upload(
            &backend,
            &src,
            &prefix,
            "small.bin",
            &UploadMode::Fail,
            &TransferOptions::default(),
            &RecProgress,
            &CancelToken::new(),
        )
        .await
        .expect("업로드 실패");
        assert_eq!(item.key, key);
        assert_eq!(item.size, 100 * 1024);

        // 다운로드 경로로 바이트를 검증한다.
        let dest = dir.0.join("down.bin");
        transfer::download(
            &backend,
            &key,
            &dest,
            &TransferOptions::default(),
            &RecProgress,
            &CancelToken::new(),
        )
        .await
        .expect("다운로드 실패");
        assert_eq!(std::fs::read(&dest).unwrap(), std::fs::read(&src).unwrap());
        cleanup_prefix(&operator, &prefix).await;
    });
}

#[test]
fn upload_multipart_40mb_minio() {
    if !enabled() {
        return;
    }
    let prefix = format!("{P3}mpu/");
    let dir = TempDir::new("mpu");
    runtime().block_on(async {
        let operator = common::operator();
        cleanup_prefix(&operator, &prefix).await;
        let backend = open_backend();
        let key = format!("{prefix}mpu-40mb.bin");
        // 40MB > 16MB 파트 → 멀티파트 경로(파트 3개).
        let src = make_file(&dir, "mpu.bin", 40 * 1024 * 1024);

        transfer::upload(
            &backend,
            &src,
            &prefix,
            "mpu-40mb.bin",
            &UploadMode::Fail,
            &TransferOptions::default(),
            &RecProgress,
            &CancelToken::new(),
        )
        .await
        .expect("멀티파트 업로드 실패");

        let expected: Vec<u8> = (0..40 * 1024 * 1024).map(|i| (i % 256) as u8).collect();
        assert_remote_bytes(&backend, &key, &expected).await;
        cleanup_prefix(&operator, &prefix).await;
    });
}

#[test]
fn upload_cancel_minio() {
    if !enabled() {
        return;
    }
    let prefix = format!("{P3}cancel/");
    let dir = TempDir::new("cancel");
    runtime().block_on(async {
        let operator = common::operator();
        cleanup_prefix(&operator, &prefix).await;
        let backend = open_backend();
        let key = format!("{prefix}cancel.bin");
        // 64MB → 16MB 파트 4개. 두 번째 진행률 콜백(첫 파트 완료)에서 취소.
        let src = make_file(&dir, "cancel.bin", 64 * 1024 * 1024);

        let cancel = CancelToken::new();
        let progress = CancelAfter {
            token: cancel.clone(),
            calls: AtomicUsize::new(0),
            after: 2,
        };
        let err = transfer::upload(
            &backend,
            &src,
            &prefix,
            "cancel.bin",
            &UploadMode::Fail,
            &TransferOptions::default(),
            &progress,
            &cancel,
        )
        .await
        .expect_err("Cancelled여야 한다");
        assert!(matches!(err, Error::Cancelled(_)), "{err:?}");

        // 대상 키가 없어야 하고, 멀티파트 abort로 객체가 생기지 않아야 한다.
        // abort는 백그라운드로 보내지므로 잠시 뒤 재확인한다.
        for _ in 0..10 {
            let stat = backend.stat(&key).await;
            assert!(
                matches!(stat, Err(Error::NotFound(_))),
                "취소 후 객체가 남았다: {stat:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        cleanup_prefix(&operator, &prefix).await;
    });
}

#[test]
fn upload_conflict_modes_minio() {
    if !enabled() {
        return;
    }
    let prefix = format!("{P3}conflict/");
    let dir = TempDir::new("conflict");
    runtime().block_on(async {
        let operator = common::operator();
        cleanup_prefix(&operator, &prefix).await;
        let backend = open_backend();
        let key = format!("{prefix}conflict.txt");
        let src = make_file(&dir, "conflict.txt", 10);
        let opts = TransferOptions::default();
        let cancel = CancelToken::new();

        // Fail로 첫 업로드.
        transfer::upload(
            &backend,
            &src,
            &prefix,
            "conflict.txt",
            &UploadMode::Fail,
            &opts,
            &RecProgress,
            &cancel,
        )
        .await
        .expect("첫 업로드 실패");
        let etag1 = backend.stat(&key).await.unwrap().etag;

        // Fail 모드로 다시 → Conflict.
        let err = transfer::upload(
            &backend,
            &src,
            &prefix,
            "conflict.txt",
            &UploadMode::Fail,
            &opts,
            &RecProgress,
            &cancel,
        )
        .await
        .expect_err("Conflict여야 한다");
        assert!(matches!(err, Error::Conflict(_)), "{err:?}");

        // 다른 쓰기로 ETag가 바뀐다.
        operator
            .write(&key, b"changed-by-other".to_vec())
            .await
            .expect("덮어쓰기 실패");
        let etag2 = backend.stat(&key).await.unwrap().etag;
        assert_ne!(etag1, etag2, "ETag가 바뀌지 않았다");

        // 낡은 ETag로 조건부 Replace → Conflict.
        let err = transfer::upload(
            &backend,
            &src,
            &prefix,
            "conflict.txt",
            &UploadMode::Replace {
                if_match: etag1.clone(),
            },
            &opts,
            &RecProgress,
            &cancel,
        )
        .await
        .expect_err("Conflict여야 한다");
        assert!(matches!(err, Error::Conflict(_)), "{err:?}");

        // 현재 ETag로는 성공.
        transfer::upload(
            &backend,
            &src,
            &prefix,
            "conflict.txt",
            &UploadMode::Replace { if_match: etag2 },
            &opts,
            &RecProgress,
            &cancel,
        )
        .await
        .expect("조건부 업로드 실패");

        // KeepBoth → "conflict (2).txt".
        let item = transfer::upload(
            &backend,
            &src,
            &prefix,
            "conflict.txt",
            &UploadMode::KeepBoth,
            &opts,
            &RecProgress,
            &cancel,
        )
        .await
        .expect("KeepBoth 업로드 실패");
        assert_eq!(item.key, format!("{prefix}conflict (2).txt"));

        cleanup_prefix(&operator, &prefix).await;
    });
}

#[test]
fn upload_folder_minio() {
    if !enabled() {
        return;
    }
    let prefix = format!("{P3}folder/");
    let dir = TempDir::new("folder");
    runtime().block_on(async {
        let operator = common::operator();
        cleanup_prefix(&operator, &prefix).await;
        let backend = open_backend();

        // 로컬 폴더: 파일 2개, .DS_Store, 빈 디렉터리.
        let src_dir = dir.0.join("myfolder");
        std::fs::create_dir_all(src_dir.join("sub/empty")).unwrap();
        std::fs::write(src_dir.join("a.txt"), b"aaa").unwrap();
        std::fs::write(src_dir.join(".DS_Store"), b"junk").unwrap();
        std::fs::write(src_dir.join("sub/b.txt"), b"bb").unwrap();

        let result = transfer::upload_folder(
            &backend,
            &src_dir,
            &prefix,
            &UploadMode::Fail,
            &TransferOptions::default(),
            &RecProgress,
            &CancelToken::new(),
        )
        .await
        .expect("폴더 업로드 실패");

        assert_eq!(result.uploaded, 2);
        assert_eq!(result.ignored, 1, ".DS_Store만 제외돼야 한다");
        assert_eq!(result.total_bytes, 5);

        // .DS_Store는 버킷에 없어야 한다.
        let folder_prefix = format!("{prefix}myfolder/");
        let listed = backend
            .list_recursive(&folder_prefix)
            .await
            .expect("목록 실패");
        let keys: Vec<&str> = listed.iter().map(|i| i.key.as_str()).collect();
        assert!(
            keys.contains(&"docs/p3-upload/folder/myfolder/a.txt"),
            "{keys:?}"
        );
        assert!(
            keys.contains(&"docs/p3-upload/folder/myfolder/sub/b.txt"),
            "{keys:?}"
        );
        assert!(
            !keys.iter().any(|k| k.contains(".DS_Store")),
            ".DS_Store가 업로드됐다: {keys:?}"
        );
        // 빈 디렉터리 마커가 폴더로 보여야 한다.
        assert!(
            keys.contains(&"docs/p3-upload/folder/myfolder/sub/empty/"),
            "빈 디렉터리 마커가 없다: {keys:?}"
        );
        let empty_item = listed
            .iter()
            .find(|i| i.key == "docs/p3-upload/folder/myfolder/sub/empty/")
            .expect("마커 항목 없음");
        assert!(empty_item.is_dir, "마커가 폴더로 표시되지 않는다");

        // 내용 검증.
        assert_remote_bytes(&backend, "docs/p3-upload/folder/myfolder/a.txt", b"aaa").await;
        assert_remote_bytes(&backend, "docs/p3-upload/folder/myfolder/sub/b.txt", b"bb").await;
        cleanup_prefix(&operator, &prefix).await;
    });
}

#[test]
fn create_dir_and_list_minio() {
    if !enabled() {
        return;
    }
    let prefix = format!("{P3}mkdir/");
    runtime().block_on(async {
        let operator = common::operator();
        cleanup_prefix(&operator, &prefix).await;
        let backend = open_backend();

        backend
            .create_dir("docs/p3-upload/mkdir/newdir/")
            .await
            .expect("create_dir 실패");
        // 이미 있어도 성공해야 한다.
        backend
            .create_dir("docs/p3-upload/mkdir/newdir/")
            .await
            .expect("create_dir 재호출 실패");

        let items = backend
            .list_folder(&prefix)
            .await
            .expect("list_folder 실패");
        let folder = items
            .iter()
            .find(|i| i.key == "docs/p3-upload/mkdir/newdir/")
            .expect("생성한 폴더가 목록에 없다");
        assert!(folder.is_dir, "폴더로 표시되지 않는다: {folder:?}");

        // write_bytes로 권한 확인용 객체를 쓸 수 있다.
        backend
            .write_bytes("docs/p3-upload/mkdir/write-test.bin", b"ok".to_vec())
            .await
            .expect("write_bytes 실패");
        assert_remote_bytes(&backend, "docs/p3-upload/mkdir/write-test.bin", b"ok").await;

        // copy는 서버 측 복사로 내용을 그대로 옮긴다.
        backend
            .copy(
                "docs/p3-upload/mkdir/write-test.bin",
                "docs/p3-upload/mkdir/copy-test.bin",
            )
            .await
            .expect("copy 실패");
        assert_remote_bytes(&backend, "docs/p3-upload/mkdir/copy-test.bin", b"ok").await;

        cleanup_prefix(&operator, &prefix).await;
    });
}

#[test]
fn delete_keys_batches_minio() {
    if !enabled() {
        return;
    }
    let prefix = format!("{P3}bulk/");
    runtime().block_on(async {
        let operator = common::operator();
        cleanup_prefix(&operator, &prefix).await;
        let backend = open_backend();

        // 1,500개 객체 — S3 DeleteObjects 배치 상한(1,000)을 넘는다.
        let keys: Vec<String> = (0..1500).map(|i| format!("{prefix}{i:04}.bin")).collect();
        // 시드는 OpenDAL write로 빠르게 쓴다(내용은 작게).
        for chunk in keys.chunks(100) {
            let mut set = tokio::task::JoinSet::new();
            for key in chunk {
                let op = operator.clone();
                let key = key.clone();
                set.spawn(async move {
                    op.write(&key, b"x".to_vec()).await.expect("시드 쓰기 실패");
                });
            }
            while set.join_next().await.is_some() {}
        }

        backend.delete_keys(&keys).await.expect("delete_keys 실패");

        // 전부 사라졌는지 확인한다.
        let listed = backend.list_recursive(&prefix).await.expect("목록 실패");
        assert!(listed.is_empty(), "삭제 후 {}개가 남았다", listed.len());
        cleanup_prefix(&operator, &prefix).await;
    });
}

/// PIER_TEST_BIG=1일 때만 실행. 5.1GB 희소 파일 멀티파트 업로드 후
/// 서버 측 복사해 크기가 같은지 확인한다.
#[test]
fn upload_big_51gb_minio() {
    if !enabled() || !big_enabled() {
        return;
    }
    let prefix = format!("{P3}big51/");
    let dir = TempDir::new("big51");
    runtime().block_on(async {
        let operator = common::operator();
        cleanup_prefix(&operator, &prefix).await;
        let backend = open_backend();

        // 희소 파일: 약 5.1GB 논리 크기, 실제 블록은 거의 없다.
        let size: u64 = 5_368_709_120; // 5.0 GiB
        let src = dir.0.join("big51.bin");
        {
            let f = std::fs::File::create(&src).expect("파일 생성 실패");
            f.set_len(size).expect("set_len 실패");
        }
        let key = format!("{prefix}big51.bin");
        let copy_key = format!("{prefix}big51-copy.bin");

        let start = std::time::Instant::now();
        transfer::upload(
            &backend,
            &src,
            &prefix,
            "big51.bin",
            &UploadMode::Fail,
            &TransferOptions::default(),
            &RecProgress,
            &CancelToken::new(),
        )
        .await
        .expect("5.1GB 업로드 실패");
        let upload_secs = start.elapsed().as_secs_f64();
        eprintln!(
            "5.1GB 업로드: {upload_secs:.1}s ({:.1} MB/s)",
            size as f64 / upload_secs / (1024.0 * 1024.0)
        );

        // 서버 측 복사(5GB 초과 → 멀티파트 복사 경로).
        let start = std::time::Instant::now();
        backend
            .copy(&key, &copy_key)
            .await
            .expect("5.1GB 복사 실패");
        let copy_secs = start.elapsed().as_secs_f64();
        eprintln!("5.1GB 복사: {copy_secs:.1}s");

        let item = backend.stat(&copy_key).await.expect("복사본 stat 실패");
        assert_eq!(item.size, size, "복사본 크기 불일치");
        assert!(!item.is_dir);

        // 내용은 0으로 채워진 희소 파일이므로 양 끝 일부만 확인한다.
        let head = backend
            .read_range(&copy_key, 0, 1024 * 1024)
            .await
            .expect("read_range 실패");
        assert!(head.iter().all(|b| *b == 0), "복사본 앞이 0이 아니다");
        let tail_off = size - 1024 * 1024;
        let tail = backend
            .read_range(&copy_key, tail_off, 1024 * 1024)
            .await
            .expect("read_range 실패");
        assert!(tail.iter().all(|b| *b == 0), "복사본 끝이 0이 아니다");

        cleanup_prefix(&operator, &prefix).await;
    });
}
