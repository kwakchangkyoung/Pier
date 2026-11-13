//! PIER_TEST_MINIO=1 또는 원격 대상(PIER_TEST_BUCKET 등)에서 실행되는
//! 병렬 다운로드 통합 테스트. 공용 설정·헬퍼는 tests/common/mod.rs를 본다.

mod common;

use std::sync::Mutex;

use opendal::Operator;
use pier_core::error::Error;
use pier_core::transfer::{self, CancelToken, ProgressSink, TransferOptions};

use common::{enabled, local_minio, open_backend, runtime, TempDir};

const KEY: &str = "docs/p2-transfer/big.bin";
/// 약 40MB. 기본 청크(8MB)로 5개 청크가 된다.
const SIZE: usize = 40 * 1024 * 1024;

/// 결정적 패턴의 big.bin 내용을 만든다.
fn big_data() -> Vec<u8> {
    (0..SIZE).map(|i| (i % 256) as u8).collect()
}

/// big.bin이 같은 크기로 이미 있으면 건너뛰고, 아니면 업로드한다.
/// 비교용 원본 데이터를 돌려준다.
async fn ensure_big_file(operator: &Operator) -> Vec<u8> {
    let data = big_data();
    let exists = operator
        .stat(KEY)
        .await
        .map(|meta| meta.content_length() == data.len() as u64)
        .unwrap_or(false);
    if !exists {
        operator
            .write(KEY, data.clone())
            .await
            .expect("big.bin 업로드 실패");
    }
    data
}

/// 진행률 호출을 기록하는 싱크.
#[derive(Default)]
struct RecProgress {
    calls: Mutex<Vec<(u64, u64)>>,
}

impl RecProgress {
    fn last(&self) -> Option<(u64, u64)> {
        self.calls.lock().unwrap().last().copied()
    }
}

impl ProgressSink for RecProgress {
    fn on_progress(&self, transferred: u64, total: u64) {
        self.calls.lock().unwrap().push((transferred, total));
    }
}

/// 첫 진행률 콜백에서 취소하는 싱크.
struct CancelOnProgress {
    token: CancelToken,
}

impl ProgressSink for CancelOnProgress {
    fn on_progress(&self, _transferred: u64, _total: u64) {
        self.token.cancel();
    }
}

#[test]
fn download_big_minio() {
    if !enabled() {
        return;
    }
    let dir = TempDir::new("big");
    runtime().block_on(async {
        let operator = common::operator();
        let data = ensure_big_file(&operator).await;
        let backend = open_backend();
        let dest = dir.join("big.bin");
        let progress = RecProgress::default();

        let written = transfer::download(
            &backend,
            KEY,
            &dest,
            &TransferOptions::default(),
            &progress,
            &CancelToken::new(),
        )
        .await
        .expect("다운로드 실패");

        assert_eq!(written, data.len() as u64);
        let got = std::fs::read(&dest).expect("다운로드 파일 읽기 실패");
        assert_eq!(got, data, "다운로드 내용이 원본과 다르다");
        assert_eq!(
            progress.last(),
            Some((data.len() as u64, data.len() as u64)),
            "진행률 마지막 값이 전체 크기가 아니다"
        );
        assert!(!dir.has_temp_file());
    });
}

#[test]
fn download_cancel_minio() {
    if !enabled() {
        return;
    }
    let dir = TempDir::new("cancel");
    runtime().block_on(async {
        let operator = common::operator();
        ensure_big_file(&operator).await;
        let backend = open_backend();
        let dest = dir.join("cancel.bin");

        // 1MB 청크·동시성 2 → 40청크. 첫 진행률에서 취소하면 나머지가 남는다.
        let opts = TransferOptions {
            chunk_size: 1024 * 1024,
            concurrency: 2,
        };
        let cancel = CancelToken::new();
        let progress = CancelOnProgress {
            token: cancel.clone(),
        };
        let err = transfer::download(&backend, KEY, &dest, &opts, &progress, &cancel)
            .await
            .expect_err("Cancelled여야 한다");
        assert!(matches!(err, Error::Cancelled(_)), "{err:?}");
        assert!(!dest.exists(), "취소된 다운로드가 dest를 만들었다");
        assert!(!dir.has_temp_file(), "취소 후 임시 파일이 남았다");

        // 시작 전 취소도 같은 결과여야 한다.
        let cancel = CancelToken::new();
        cancel.cancel();
        let err = transfer::download(
            &backend,
            KEY,
            &dest,
            &opts,
            &RecProgress::default(),
            &cancel,
        )
        .await
        .expect_err("Cancelled여야 한다");
        assert!(matches!(err, Error::Cancelled(_)), "{err:?}");
        assert!(!dest.exists());
        assert!(!dir.has_temp_file());
    });
}

/// 폴더 다운로드용 시드: docs/p2-folderdl/a.jpg, docs/p2-folderdl/2024/b.jpg.
/// 루트 목록 항목 수를 유지하기 위해 docs/ 아래에 둔다(minio_list.rs와 같은 규칙).
/// 비교용 원본 데이터를 돌려준다.
async fn ensure_folder_fixture(operator: &Operator) -> (Vec<u8>, Vec<u8>) {
    let a = b"folder-a-data".to_vec();
    let b = b"folder-b-data-2024".to_vec();
    for (key, data) in [
        ("docs/p2-folderdl/a.jpg", &a),
        ("docs/p2-folderdl/2024/b.jpg", &b),
    ] {
        // 내용이 바뀌었으면 다시 쓴다(멱등).
        let ok = operator
            .stat(key)
            .await
            .map(|m| m.content_length() == data.len() as u64)
            .unwrap_or(false);
        if !ok {
            operator
                .write(key, data.clone())
                .await
                .expect("폴더 시드 업로드 실패");
        }
    }
    (a, b)
}

#[test]
fn download_folder_minio() {
    if !enabled() {
        return;
    }
    let dir = TempDir::new("folder");
    runtime().block_on(async {
        let operator = common::operator();
        let (a, b) = ensure_folder_fixture(&operator).await;
        let backend = open_backend();

        let result = transfer::download_folder(
            &backend,
            "docs/p2-folderdl/",
            &dir.0,
            &TransferOptions::default(),
            &RecProgress::default(),
            &CancelToken::new(),
        )
        .await
        .expect("폴더 다운로드 실패");

        let root = dir.0.join("p2-folderdl");
        assert_eq!(result.local_path, root);
        assert_eq!(result.file_count, 2);
        assert_eq!(result.total_bytes, (a.len() + b.len()) as u64);
        // 하위 구조가 그대로 유지되고 내용이 원본과 일치한다.
        assert_eq!(std::fs::read(root.join("a.jpg")).unwrap(), a);
        assert_eq!(std::fs::read(root.join("2024/b.jpg")).unwrap(), b);
        assert!(!dir.has_temp_file_deep());

        // 폴더 마커만 있는 폴더(루트의 empty-folder/, seed-minio.sh가 생성)는
        // 빈 디렉터리로 만들어진다. 루트 시드 데이터라 로컬 MinIO에서만 본다.
        if local_minio() {
            let empty = transfer::download_folder(
                &backend,
                "empty-folder/",
                &dir.0,
                &TransferOptions::default(),
                &RecProgress::default(),
                &CancelToken::new(),
            )
            .await
            .expect("빈 폴더 다운로드 실패");
            assert_eq!(empty.local_path, dir.0.join("empty-folder"));
            assert_eq!(empty.file_count, 0);
            assert_eq!(empty.total_bytes, 0);
            assert!(empty.local_path.is_dir());
            assert_eq!(std::fs::read_dir(&empty.local_path).unwrap().count(), 0);
            assert!(!dir.has_temp_file_deep());
        }
    });
}

#[test]
fn download_missing_key_minio() {
    if !enabled() {
        return;
    }
    let dir = TempDir::new("missing");
    runtime().block_on(async {
        let backend = open_backend();
        let dest = dir.join("missing.bin");
        let err = transfer::download(
            &backend,
            "docs/p2-transfer/no-such-key",
            &dest,
            &TransferOptions::default(),
            &RecProgress::default(),
            &CancelToken::new(),
        )
        .await
        .expect_err("NotFound여야 한다");
        assert!(matches!(err, Error::NotFound(_)), "{err:?}");
        assert!(!dest.exists());
        assert!(!dir.has_temp_file());
    });
}
