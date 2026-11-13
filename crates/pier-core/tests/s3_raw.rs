//! PIER_TEST_MINIO=1 또는 원격 대상(PIER_TEST_BUCKET 등)에서 실행되는
//! 서명 S3 호출·이어올리기 통합 테스트.
//! 공용 설정·헬퍼는 tests/common/mod.rs를 본다.
//! 테스트 객체는 docs/p4-raw/ 아래에 두고 끝에 지운다
//! (원격에서는 PIER_TEST_PREFIX 아래에 놓인다).

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};

use pier_core::backend::WriteCondition;
use pier_core::error::Error;
use pier_core::metadata::MetadataStore;
use pier_core::s3_raw::{abort_upload, upload_resumable, ResumableUpload, S3Raw};
use pier_core::transfer::{CancelToken, ProgressSink, TransferOptions};
use reqsign_aws_v4::{Credential, RequestSigner};
use reqsign_core::hash::hex_sha256;
use reqsign_core::{Context, SignRequest};

use common::{enabled, open_raw, runtime, target, TempDir};

/// 테스트 객체 루트.
const PREFIX: &str = "docs/p4-raw/";
/// 40MB. chunk 10MB로 파트 4개다.
const SIZE: usize = 40 * 1024 * 1024;

/// 진행률을 기록하고, 임계치에 도달하면 토큰을 취소하는 싱크.
struct RecSink {
    calls: Mutex<Vec<u64>>,
    cancel_at: Option<u64>,
    token: CancelToken,
}

impl RecSink {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            cancel_at: None,
            token: CancelToken::new(),
        }
    }

    /// transferred가 at 이상이 되면 token을 취소한다.
    fn cancelling(at: u64, token: CancelToken) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            cancel_at: Some(at),
            token,
        }
    }

    fn calls(&self) -> Vec<u64> {
        self.calls.lock().expect("잠금").clone()
    }
}

impl ProgressSink for RecSink {
    fn on_progress(&self, transferred: u64, _total: u64) {
        self.calls.lock().expect("잠금").push(transferred);
        if let Some(at) = self.cancel_at {
            if transferred >= at {
                self.token.cancel();
            }
        }
    }
}

/// 버킷의 미완료 멀티파트 업로드 (키, 업로드 ID) 목록.
/// 검증용 ListMultipartUploads 직접 호출 — 코어와 같은 reqwest+reqsign 경로.
async fn list_mpu() -> Vec<(String, String)> {
    let url: reqwest::Url = common::list_uploads_url().parse().expect("URL");
    let mut req = reqwest::Request::new(http::Method::GET, url);
    req.headers_mut().insert(
        http::header::HeaderName::from_static("x-amz-content-sha256"),
        hex_sha256(&[]).parse().expect("해시 헤더"),
    );
    *req.body_mut() = Some(reqwest::Body::from(Vec::new()));
    let http_req: http::Request<reqwest::Body> = req.try_into().expect("요청 변환");
    let (mut parts, body) = http_req.into_parts();
    let t = target();
    let signer = RequestSigner::new("s3", &t.region);
    let cred = Credential {
        access_key_id: t.access_key.clone(),
        secret_access_key: t.secret.clone(),
        session_token: None,
        expires_in: None,
    };
    signer
        .sign_request(&Context::new(), &mut parts, Some(&cred), None)
        .await
        .expect("서명");
    let url: reqwest::Url = parts.uri.to_string().parse().expect("URL");
    let mut out = reqwest::Request::new(parts.method, url);
    *out.headers_mut() = parts.headers;
    *out.body_mut() = Some(body);
    let resp = reqwest::Client::new()
        .execute(out)
        .await
        .expect("list_mpu 요청");
    assert!(
        resp.status().is_success(),
        "list_mpu 실패: {}",
        resp.status()
    );
    let body = resp.bytes().await.expect("응답 본문");
    parse_mpu_list(&String::from_utf8_lossy(&body))
}

/// ListMultipartUploadsResult에서 (Key, UploadId) 쌍을 뽑는다.
fn parse_mpu_list(xml: &str) -> Vec<(String, String)> {
    use quick_xml::events::Event;
    use quick_xml::Reader;
    let mut reader = Reader::from_str(xml);
    let mut out = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if e.name().as_ref() == "Upload" => {
                let mut key = String::new();
                let mut id = String::new();
                loop {
                    match reader.read_event() {
                        Ok(Event::Start(e)) => match e.name().as_ref() {
                            "Key" => {
                                key = reader
                                    .read_text(e.name())
                                    .expect("Key")
                                    .xml10_content()
                                    .into_owned();
                            }
                            "UploadId" => {
                                id = reader
                                    .read_text(e.name())
                                    .expect("UploadId")
                                    .xml10_content()
                                    .into_owned();
                            }
                            _ => {}
                        },
                        Ok(Event::End(e)) if e.name().as_ref() == "Upload" => break,
                        Ok(Event::Eof) => break,
                        _ => {}
                    }
                }
                out.push((key, id));
            }
            Ok(Event::Eof) => break,
            Err(e) => panic!("MPU 목록 XML 파싱 실패: {e}"),
            _ => {}
        }
    }
    out
}

/// 진행 중인 멀티파트 업로드를 50% 지점에서 멈추고 상태를 돌려준다.
async fn pause_mid_upload(
    raw: &S3Raw,
    src: &Path,
    key: &str,
    condition: WriteCondition,
    cancel_at: u64,
) -> (Option<ResumableUpload>, Vec<u64>) {
    let token = CancelToken::new();
    let sink = RecSink::cancelling(cancel_at, token.clone());
    let mut state = None;
    let opts = TransferOptions {
        chunk_size: 10 * 1024 * 1024,
        ..Default::default()
    };
    let err = upload_resumable(raw, src, key, condition, &opts, &mut state, &sink, &token)
        .await
        .expect_err("취소된 업로드는 Err를 반환해야 한다");
    assert!(
        matches!(err, Error::Cancelled(_)),
        "기대: Cancelled, 실제: {err}"
    );
    (state, sink.calls())
}

#[test]
fn head_storage_class_standard() {
    if !enabled() {
        return;
    }
    let dir = TempDir::new("head");
    runtime().block_on(async {
        let raw = open_raw();
        let key = format!("{PREFIX}head.bin");
        let src = dir.write_file("head.bin", 6 * 1024 * 1024);
        let mut state = None;
        upload_resumable(
            &raw,
            &src,
            &key,
            WriteCondition::Any,
            &TransferOptions::default(),
            &mut state,
            &RecSink::new(),
            &CancelToken::new(),
        )
        .await
        .expect("시드 업로드 실패");

        let class = raw.head_storage_class(&key).await.expect("head 실패");
        assert_eq!(class, "STANDARD");

        common::operator()
            .delete(&key)
            .await
            .expect("테스트 객체 정리 실패");
    });
}

#[test]
fn resumable_upload_pause_and_resume() {
    if !enabled() {
        return;
    }
    let dir = TempDir::new("resume");
    runtime().block_on(async {
        let raw = open_raw();
        let key = format!("{PREFIX}resume.bin");
        let src = dir.write_file("resume.bin", SIZE);
        let expected = std::fs::read(&src).expect("원본 읽기");
        let opts = TransferOptions {
            chunk_size: 10 * 1024 * 1024,
            ..Default::default()
        };

        // 40MB, 파트 4개. 진행률이 20MB(50%)를 넘는 순간 취소한다.
        let (state, calls) =
            pause_mid_upload(&raw, &src, &key, WriteCondition::Any, SIZE as u64 / 2).await;
        let paused = state.expect("일시정지 후 상태가 남아야 한다");
        assert!(!paused.parts.is_empty(), "완료된 파트가 상태에 있어야 한다");
        assert!(paused.parts.len() < 4, "전부 올라가면 안 된다");
        let paused_bytes: u64 = paused
            .parts
            .keys()
            .map(|&n| (u64::from(n - 1) * paused.part_size).min(SIZE as u64))
            .map(|off| paused.part_size.min(SIZE as u64 - off))
            .sum();
        assert_eq!(calls.last().copied(), Some(paused_bytes));

        // 재개: 첫 진행률이 이미 올라간 분량에서 시작해야 한다.
        let sink2 = RecSink::new();
        let mut resume_state = Some(paused.clone());
        upload_resumable(
            &raw,
            &src,
            &key,
            WriteCondition::Any,
            &opts,
            &mut resume_state,
            &sink2,
            &CancelToken::new(),
        )
        .await
        .expect("재개 업로드 실패");
        assert!(resume_state.is_none(), "완료 후 상태는 비워야 한다");

        let calls2 = sink2.calls();
        let first = calls2.first().copied().expect("진행률 호출이 있어야 한다");
        // 취소로 끊긴 요청이 서버에서 늦게 완료됐을 수 있어 재개 시작점은
        // 로컬 기록 이상이다. 중요한 것은 완료된 파트를 다시 안 보낸다는 것.
        assert!(
            first >= paused_bytes && first % paused.part_size == 0,
            "재개 진행률은 서버가 가진 파트 분량({paused_bytes} 이상)부터 시작한다: {first}"
        );
        // 재개 때 보낸 분량 ≤ 일시정지 시점의 남은 분량.
        let resumed: u64 = calls2.windows(2).map(|w| w[1] - w[0]).sum();
        assert!(resumed <= SIZE as u64 - paused_bytes);
        assert_eq!(calls2.last().copied(), Some(SIZE as u64));

        // 바이트 일치 확인.
        let got = common::operator()
            .read(&key)
            .await
            .expect("다운로드 실패")
            .to_vec();
        assert_eq!(got, expected, "재개 완료 객체가 원본과 달라야 하지 않는다");

        common::operator()
            .delete(&key)
            .await
            .expect("테스트 객체 정리 실패");
    });
}

#[test]
fn abort_after_pause_leaves_no_mpu() {
    if !enabled() {
        return;
    }
    let dir = TempDir::new("abort");
    runtime().block_on(async {
        let raw = open_raw();
        let key = format!("{PREFIX}abort.bin");
        let src = dir.write_file("abort.bin", SIZE);

        let (state, _) =
            pause_mid_upload(&raw, &src, &key, WriteCondition::Any, SIZE as u64 / 2).await;
        let paused = state.expect("상태가 남아야 한다");
        assert!(
            list_mpu()
                .await
                .iter()
                .any(|(k, _)| *k == common::bucket_key(&key)),
            "abort 전에는 MPU가 있어야 한다"
        );

        abort_upload(&raw, &paused).await.expect("abort 실패");
        assert!(
            !list_mpu()
                .await
                .iter()
                .any(|(k, _)| *k == common::bucket_key(&key)),
            "abort 후에는 미완료 MPU가 없어야 한다"
        );
    });
}

#[test]
fn complete_conditional_conflict() {
    if !enabled() {
        return;
    }
    let dir = TempDir::new("conflict");
    runtime().block_on(async {
        let raw = open_raw();
        let key = format!("{PREFIX}conflict.bin");
        let src = dir.write_file("conflict.bin", 6 * 1024 * 1024);
        let opts = TransferOptions::default();

        // 기존 객체를 만들어 둔다.
        let mut state = None;
        upload_resumable(
            &raw,
            &src,
            &key,
            WriteCondition::Any,
            &opts,
            &mut state,
            &RecSink::new(),
            &CancelToken::new(),
        )
        .await
        .expect("시드 업로드 실패");

        // 어긋나는 ETag로 조건부 Complete → Conflict.
        let mut state = None;
        let err = upload_resumable(
            &raw,
            &src,
            &key,
            WriteCondition::IfMatch("\"deadbeef\"".to_string()),
            &opts,
            &mut state,
            &RecSink::new(),
            &CancelToken::new(),
        )
        .await
        .expect_err("조건 불일치는 실패해야 한다");
        assert!(
            matches!(err, Error::Conflict(_)),
            "기대: Conflict, 실제: {err}"
        );

        // 실패한 Complete도 MPU는 남는다 — 테스트 MPU를 정리한다.
        if let Some(s) = state {
            abort_upload(&raw, &s).await.expect("테스트 MPU 정리 실패");
        }

        common::operator()
            .delete(&key)
            .await
            .expect("테스트 객체 정리 실패");
    });
}

#[test]
fn cleanup_pending_uploads_removes_leftover() {
    if !enabled() {
        return;
    }
    let dir = TempDir::new("cleanup");
    runtime().block_on(async {
        let mut raw = open_raw();
        let store = Arc::new(Mutex::new(
            MetadataStore::open_in_memory().expect("저장소 생성 실패"),
        ));
        raw.set_metadata_store(Arc::clone(&store));

        let key = format!("{PREFIX}cleanup.bin");
        let src = dir.write_file("cleanup.bin", SIZE);
        let (state, _) =
            pause_mid_upload(&raw, &src, &key, WriteCondition::Any, SIZE as u64 / 2).await;
        assert!(state.is_some());
        assert_eq!(
            store
                .lock()
                .expect("잠금")
                .pending_uploads()
                .expect("목록")
                .len(),
            1,
            "일시정지 후 pending 기록이 있어야 한다"
        );

        // 다음 실행 때 정리하듯 새 S3Raw로 정리한다.
        let cleaned = open_raw()
            .cleanup_pending_uploads(&store)
            .await
            .expect("정리 실패");
        assert_eq!(cleaned, 1);
        assert!(
            store
                .lock()
                .expect("잠금")
                .pending_uploads()
                .expect("목록")
                .is_empty(),
            "정리 후 기록이 비어야 한다"
        );
        assert!(
            !list_mpu()
                .await
                .iter()
                .any(|(k, _)| *k == common::bucket_key(&key)),
            "정리 후에는 미완료 MPU가 없어야 한다"
        );
    });
}

#[test]
fn wrong_secret_is_authentication() {
    if !enabled() {
        return;
    }
    runtime().block_on(async {
        let raw = S3Raw::new(&common::config(), "wrong-secret").expect("S3Raw 생성 실패");
        let err = raw
            .head_storage_class(&format!("{PREFIX}head.bin"))
            .await
            .expect_err("잘못된 시크릿은 실패해야 한다");
        assert!(
            matches!(err, Error::Authentication(_)),
            "기대: Authentication, 실제: {err}"
        );
    });
}

#[test]
fn korean_space_plus_key_upload() {
    if !enabled() {
        return;
    }
    let dir = TempDir::new("utf8");
    runtime().block_on(async {
        let raw = open_raw();
        let key = format!("{PREFIX}한글 파일 +.bin");
        let src = dir.write_file("utf8.bin", 6 * 1024 * 1024);
        let expected = std::fs::read(&src).expect("원본 읽기");

        let mut state = None;
        upload_resumable(
            &raw,
            &src,
            &key,
            WriteCondition::Any,
            &TransferOptions::default(),
            &mut state,
            &RecSink::new(),
            &CancelToken::new(),
        )
        .await
        .expect("한글·공백 키 업로드 실패");

        let got = common::operator()
            .read(&key)
            .await
            .expect("다운로드 실패")
            .to_vec();
        assert_eq!(got, expected);

        common::operator()
            .delete(&key)
            .await
            .expect("테스트 객체 정리 실패");
    });
}

/// IfMatch로 올리는 도중 다른 클라이언트가 덮어쓰면 재개 시
/// Complete 직전 HEAD 비교가 Conflict를 낸다 — 조건부 헤더를 무시하는
/// 백엔드에서도 데이터 유실이 없음을 검증한다(SPEC 3.4).
#[test]
fn resume_conflict_after_remote_change_minio() {
    if !enabled() {
        return;
    }
    let dir = TempDir::new("race");
    runtime().block_on(async {
        let operator = common::operator();
        let raw = open_raw();
        let key = format!("{PREFIX}race.bin");
        let src = dir.write_file("race.bin", SIZE);
        let opts = TransferOptions {
            chunk_size: 10 * 1024 * 1024,
            ..Default::default()
        };

        // 기존 객체를 만들고 현재 ETag를 가져온다.
        // HEAD 응답의 ETag는 인용부호를 포함해 오므로 형태를 맞춘다.
        operator
            .write(&key, b"original".to_vec())
            .await
            .expect("시드 쓰기 실패");
        let remote_etag = operator
            .stat(&key)
            .await
            .expect("stat 실패")
            .etag()
            .expect("ETag 없음")
            .to_string();
        let etag = if remote_etag.starts_with('"') {
            remote_etag
        } else {
            format!("\"{remote_etag}\"")
        };

        // IfMatch(etag)로 업로드를 시작해 중간에 멈춘다.
        let (state, _) = pause_mid_upload(
            &raw,
            &src,
            &key,
            WriteCondition::IfMatch(etag.clone()),
            SIZE as u64 / 2,
        )
        .await;
        let paused = state.expect("일시정지 후 상태가 남아야 한다");

        // 다른 클라이언트가 같은 키를 덮어쓴다.
        operator
            .write(&key, b"changed-by-other".to_vec())
            .await
            .expect("덮어쓰기 실패");

        // 재개: Complete 직전 HEAD 비교에서 Conflict가 나야 한다.
        let mut resume_state = Some(paused);
        let err = upload_resumable(
            &raw,
            &src,
            &key,
            WriteCondition::IfMatch(etag),
            &opts,
            &mut resume_state,
            &RecSink::new(),
            &CancelToken::new(),
        )
        .await
        .expect_err("원격 변경 후 재개는 Conflict여야 한다");
        assert!(
            matches!(err, Error::Conflict(_)),
            "기대: Conflict, 실제: {err}"
        );

        // 남은 MPU와 시드 객체를 정리한다.
        if let Some(s) = resume_state {
            abort_upload(&raw, &s).await.expect("MPU 정리 실패");
        }
        operator.delete(&key).await.expect("테스트 객체 정리 실패");
    });
}
