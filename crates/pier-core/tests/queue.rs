//! 전송 대기열 테스트. 가짜 Backend로 큐 동작(동시성·FIFO·상태 전이·
//! 스로틀)을 보고, PIER_TEST_MINIO=1일 때 MinIO로 실제 이어받기를 본다.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use pier_core::backend::{Backend, PutOptions};
use pier_core::enumerate::Connection;
use pier_core::error::Error;
use pier_core::item::Item;
use pier_core::metadata::MetadataStore;
use pier_core::queue::{JobSnapshot, JobState, QueueListener, TransferQueue};
use pier_core::transfer::{CancelToken, ProgressSink, TransferOptions, UploadMode};
use pier_core::OpendalS3;

// ===== 공통 도구 =====

/// 상태를 기다리는 폴링 도우미. 시간 안에 안 오면 패닉.
fn wait_state<B: Backend + Clone + Send + Sync + 'static>(
    queue: &TransferQueue<B>,
    id: u64,
    want: JobState,
    timeout: Duration,
) -> JobSnapshot {
    let start = Instant::now();
    loop {
        let jobs = queue.jobs();
        if let Some(job) = jobs.iter().find(|j| j.id == id) {
            if job.state == want {
                return job.clone();
            }
        } else if want == JobState::Cancelled {
            panic!("작업 {id}가 목록에서 사라졌다");
        }
        assert!(
            start.elapsed() < timeout,
            "{timeout:?} 안에 작업 {id}가 {want:?}가 되지 않았다 (현재: {:?})",
            jobs.iter().map(|j| (j.id, j.state)).collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn snapshot<B: Backend + Clone + Send + Sync + 'static>(
    queue: &TransferQueue<B>,
    id: u64,
) -> JobSnapshot {
    queue
        .jobs()
        .into_iter()
        .find(|j| j.id == id)
        .unwrap_or_else(|| panic!("작업 {id}가 없다"))
}

/// 리스너 호출을 기록한다.
#[derive(Default)]
struct RecListener {
    events: Mutex<Vec<JobSnapshot>>,
}

impl RecListener {
    fn events_for(&self, id: u64) -> Vec<JobSnapshot> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.id == id)
            .cloned()
            .collect()
    }
}

impl QueueListener for RecListener {
    fn on_job_update(&self, job: JobSnapshot) {
        self.events.lock().unwrap().push(job);
    }
}

/// 테스트마다 고유한 임시 디렉터리. Drop에서 지운다.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "pier-queue-it-{}-{}-{tag}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("임시 디렉터리 생성 실패");
        Self(path)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    /// `.name.pierdownload` 임시 파일 수(하위 포함).
    fn temp_files(&self) -> Vec<PathBuf> {
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
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ===== 가짜 백엔드 =====

/// 키 → (etag, 내용). 폴더 마커는 끝 '/' 키로 둔다.
type ObjectMap = HashMap<String, (String, Vec<u8>)>;

/// 인메모리 객체 저장소 백엔드. 읽기는 게이트로 막을 수 있고 호출을 기록한다.
#[derive(Clone, Default)]
struct FakeBackend {
    objects: Arc<Mutex<ObjectMap>>,
    /// read_range 호출 오프셋 기록.
    read_offsets: Arc<Mutex<Vec<u64>>>,
    /// 지금 막힌(read_range 안) 호출 수와 최고 동시 수.
    active_reads: Arc<AtomicUsize>,
    max_active: Arc<AtomicUsize>,
    /// gating이 켜져 있으면 read_range가 gate_open까지 대기한다.
    gating: Arc<AtomicBool>,
    gate_open: Arc<AtomicBool>,
    gate_notify: Arc<tokio::sync::Notify>,
    /// 남은 실패 횟수 — 있으면 read_range가 Network 오류를 낸다.
    fail_next_reads: Arc<AtomicUsize>,
    /// read_range를 이 시간만큼 늦춘다(청크 완료 시점 제어용).
    read_delay: Arc<std::sync::RwLock<Duration>>,
    /// put_file 호출 수.
    put_calls: Arc<AtomicUsize>,
}

impl FakeBackend {
    fn put_object(&self, key: &str, data: Vec<u8>) {
        self.objects
            .lock()
            .unwrap()
            .insert(key.to_string(), (format!("etag-{key}"), data));
    }

    fn read_calls(&self) -> usize {
        self.read_offsets.lock().unwrap().len()
    }

    fn offsets(&self) -> Vec<u64> {
        self.read_offsets.lock().unwrap().clone()
    }

    /// read_range 대기를 풀어준다.
    fn open_gate(&self) {
        self.gate_open.store(true, Ordering::SeqCst);
        self.gate_notify.notify_waiters();
    }

    /// read_range를 다시 막는다.
    fn close_gate(&self) {
        self.gate_open.store(false, Ordering::SeqCst);
    }
}

impl Backend for FakeBackend {
    async fn list_folder(&self, prefix: &str) -> Result<Vec<Item>, Error> {
        let objects = self.objects.lock().unwrap();
        let mut items = Vec::new();
        let mut dirs = std::collections::HashSet::new();
        for (key, (etag, data)) in objects.iter() {
            let Some(rest) = key.strip_prefix(prefix) else {
                continue;
            };
            if rest.is_empty() {
                continue;
            }
            match rest.find('/') {
                // 직속 파일 또는 직속 폴더 마커.
                None => items.push(if key.ends_with('/') {
                    Item::folder(key.clone())
                } else {
                    Item::file(key.clone(), data.len() as u64, Some(etag.clone()), None)
                }),
                Some(i) if i == rest.len() - 1 => {
                    items.push(Item::folder(key.clone()));
                }
                Some(i) => {
                    dirs.insert(format!("{prefix}{}/", &rest[..i]));
                }
            }
        }
        for dir in dirs {
            items.push(Item::folder(dir));
        }
        items.sort_by(|a, b| a.key.cmp(&b.key));
        items.dedup_by(|a, b| a.key == b.key);
        Ok(items)
    }

    async fn list_recursive(&self, prefix: &str) -> Result<Vec<Item>, Error> {
        let objects = self.objects.lock().unwrap();
        let mut items = Vec::new();
        for (key, (etag, data)) in objects.iter() {
            if key == prefix || !key.starts_with(prefix) {
                continue;
            }
            items.push(if key.ends_with('/') {
                Item::folder(key.clone())
            } else {
                Item::file(key.clone(), data.len() as u64, Some(etag.clone()), None)
            });
        }
        items.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(items)
    }

    async fn stat(&self, key: &str) -> Result<Item, Error> {
        let objects = self.objects.lock().unwrap();
        match objects.get(key) {
            Some((etag, data)) if !key.ends_with('/') => {
                Ok(Item::file(key, data.len() as u64, Some(etag.clone()), None))
            }
            Some(_) => Ok(Item::folder(key)),
            None => Err(Error::NotFound(key.to_string())),
        }
    }

    async fn read_range(&self, key: &str, offset: u64, len: u64) -> Result<Vec<u8>, Error> {
        let active = self.active_reads.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(active, Ordering::SeqCst);
        let _guard = ReadGuard(self.active_reads.clone());
        if self.gating.load(Ordering::SeqCst) {
            loop {
                let notified = self.gate_notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.gate_open.load(Ordering::SeqCst) {
                    break;
                }
                notified.await;
            }
        }
        let delay = *self.read_delay.read().unwrap();
        if delay > Duration::ZERO {
            tokio::time::sleep(delay).await;
        }
        self.read_offsets.lock().unwrap().push(offset);
        if self.fail_next_reads.load(Ordering::SeqCst) > 0 {
            self.fail_next_reads.fetch_sub(1, Ordering::SeqCst);
            return Err(Error::Network("fake: 읽기 실패".to_string()));
        }
        let objects = self.objects.lock().unwrap();
        let Some((_, data)) = objects.get(key) else {
            return Err(Error::NotFound(key.to_string()));
        };
        let start = offset.min(data.len() as u64) as usize;
        let end = (offset + len).min(data.len() as u64) as usize;
        Ok(data[start..end].to_vec())
    }

    async fn put_file(
        &self,
        key: &str,
        src: &Path,
        _opts: PutOptions,
        progress: &dyn ProgressSink,
        _cancel: &CancelToken,
    ) -> Result<(), Error> {
        let data = std::fs::read(src)?;
        // condition은 테스트에서 검증하지 않는다.
        self.objects
            .lock()
            .unwrap()
            .insert(key.to_string(), (format!("etag-{key}"), data.clone()));
        self.put_calls.fetch_add(1, Ordering::SeqCst);
        progress.on_progress(data.len() as u64, data.len() as u64);
        Ok(())
    }

    async fn write_bytes(&self, key: &str, data: Vec<u8>) -> Result<(), Error> {
        self.objects
            .lock()
            .unwrap()
            .insert(key.to_string(), (format!("etag-{key}"), data));
        Ok(())
    }

    async fn create_dir(&self, key: &str) -> Result<(), Error> {
        self.objects
            .lock()
            .unwrap()
            .insert(key.to_string(), (String::new(), Vec::new()));
        Ok(())
    }

    async fn delete_keys(&self, keys: &[String]) -> Result<(), Error> {
        let mut objects = self.objects.lock().unwrap();
        for key in keys {
            objects.remove(key);
        }
        Ok(())
    }
}

/// active_reads를 스코프 끝에서 내리는 가드.
struct ReadGuard(Arc<AtomicUsize>);

impl Drop for ReadGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn queue_with_opts(
    backend: FakeBackend,
    opts: TransferOptions,
) -> (TransferQueue<FakeBackend>, Arc<RecListener>, TempDir) {
    let dir = TempDir::new("fake");
    let store = MetadataStore::open(&dir.join("metadata.sqlite")).expect("메타데이터 열기 실패");
    let conn = Arc::new(Connection::with_backend(backend, store));
    // 대기열은 호출자의 tokio 런타임 위에서 작업을 실행한다.
    let _enter = runtime().enter();
    let queue = TransferQueue::new(conn, opts, None).expect("대기열 생성 실패");
    let listener = Arc::new(RecListener::default());
    queue.set_listener(Some(listener.clone()));
    (queue, listener, dir)
}

fn queue_with(backend: FakeBackend) -> (TransferQueue<FakeBackend>, Arc<RecListener>, TempDir) {
    queue_with_opts(
        backend,
        TransferOptions {
            chunk_size: 1024 * 1024,
            concurrency: 2,
        },
    )
}

const WAIT: Duration = Duration::from_secs(15);

// ===== 가짜 백엔드 단위 테스트 =====

/// 동시 실행 작업은 최대 3개고 FIFO(id) 순서로 시작한다.
#[test]
fn at_most_three_running_and_fifo() {
    let backend = FakeBackend::default();
    backend.gating.store(true, Ordering::SeqCst);
    for i in 0..5 {
        backend.put_object(&format!("f{i}.bin"), vec![7u8; 64]);
    }
    let (queue, _listener, dir) = queue_with(backend.clone());

    let ids: Vec<u64> = (0..5)
        .map(|i| queue.enqueue_download(&format!("f{i}.bin"), &dir.join(&format!("f{i}.bin"))))
        .collect();

    // 세 개가 읽기 안에 들어가서 막힐 때까지 기다린다.
    let start = Instant::now();
    while backend.active_reads.load(Ordering::SeqCst) < 3 {
        assert!(start.elapsed() < WAIT, "3개 작업이 시작되지 않았다");
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(100));
    let jobs = queue.jobs();
    assert_eq!(jobs[0].state, JobState::Running);
    assert_eq!(jobs[1].state, JobState::Running);
    assert_eq!(jobs[2].state, JobState::Running);
    assert_eq!(
        jobs[3].state,
        JobState::Queued,
        "4번째가 3개 한도를 넘어 시작했다"
    );
    assert_eq!(jobs[4].state, JobState::Queued);
    assert_eq!(backend.max_active.load(Ordering::SeqCst), 3);

    // FIFO: 처음 시작한 읽기는 처음 넣은 작업들이다(오프셋 0, 키는 읽기
    // 순서가 아니라 작업 상태로 확인 — id 순으로 Running이 됐다).
    assert_eq!(jobs.iter().map(|j| j.id).collect::<Vec<_>>(), ids);

    backend.open_gate();
    for id in ids {
        wait_state(&queue, id, JobState::Completed, WAIT);
    }
    // FIFO라 완료 순서도 등록 순서다 — read 호출은 f0..f4 오프셋 0이었다.
    assert_eq!(backend.read_calls(), 5);
}

/// 실행 중 pause → Paused, resume → 다시 실행해 완료.
/// 청크 태스크는 세마포어를 임의 순서로 잡으므로 어느 청크가 먼저
/// 끝날지 정해져 있지 않다 — 게이트로 정확히 하나만 완료시키고,
/// 재개 후에는 그 청크를 빼고 나머지만 읽는지 확인한다.
#[test]
fn pause_then_resume() {
    let backend = FakeBackend::default();
    backend.gating.store(true, Ordering::SeqCst);
    *backend.read_delay.write().unwrap() = Duration::from_millis(20);
    backend.put_object("big.bin", vec![5u8; 3 * 1024 * 1024]);
    let (queue, listener, dir) = queue_with_opts(
        backend.clone(),
        TransferOptions {
            chunk_size: 1024 * 1024,
            concurrency: 1,
        },
    );

    let id = queue.enqueue_download("big.bin", &dir.join("big.bin"));
    // 청크 하나가 읽기 안에 들어가 게이트에서 막힐 때까지 기다린다.
    let start = Instant::now();
    while backend.active_reads.load(Ordering::SeqCst) < 1 {
        assert!(start.elapsed() < WAIT, "다운로드가 시작되지 않았다");
        std::thread::sleep(Duration::from_millis(10));
    }
    // 그 청크만 완료시킨 뒤 다시 막는다.
    backend.open_gate();
    let start = Instant::now();
    while snapshot(&queue, id).transferred == 0 {
        assert!(start.elapsed() < WAIT, "첫 청크가 끝나지 않았다");
        std::thread::sleep(Duration::from_millis(10));
    }
    backend.close_gate();

    queue.pause_job(id);
    let paused = wait_state(&queue, id, JobState::Paused, WAIT);
    assert!(paused.transferred > 0 && paused.transferred < 3 * 1024 * 1024);
    // Paused 통지가 즉시 갔는지 확인.
    assert!(
        listener
            .events_for(id)
            .iter()
            .any(|e| e.state == JobState::Paused),
        "Paused 상태 통지가 없었다"
    );

    // 지금까지 읽기 요청은 완료된 청크 하나뿐이어야 한다(끝난 읽기만
    // 오프셋을 남기므로). 재개 후엔 그 오프셋을 다시 읽으면 안 된다.
    let finished_offset = backend.offsets()[0];
    let calls_at_pause = backend.read_calls();
    queue.resume_job(id);
    backend.open_gate();
    let done = wait_state(&queue, id, JobState::Completed, WAIT);
    assert_eq!(done.transferred, 3 * 1024 * 1024);
    assert_eq!(
        std::fs::read(dir.join("big.bin")).unwrap().len(),
        3 * 1024 * 1024
    );
    // 재개 후에는 남은 두 청크만 읽었다 — 완료된 청크는 다시 안 읽는다.
    let new_offsets = &backend.offsets()[calls_at_pause..];
    assert_eq!(
        new_offsets.len(),
        2,
        "남은 청크 수와 다르다: {new_offsets:?}"
    );
    assert!(
        !new_offsets.contains(&finished_offset),
        "완료된 청크(오프셋 {finished_offset})를 다시 읽었다: {new_offsets:?}"
    );
}

/// 일시정지된 작업의 cancel → Cancelled + 임시 파일 정리.
#[test]
fn cancel_paused_job_removes_tmp() {
    let backend = FakeBackend::default();
    *backend.read_delay.write().unwrap() = Duration::from_millis(60);
    backend.put_object("big.bin", vec![9u8; 3 * 1024 * 1024]);
    let (queue, _listener, dir) = queue_with(backend.clone());

    let id = queue.enqueue_download("big.bin", &dir.join("big.bin"));
    let start = Instant::now();
    while snapshot(&queue, id).transferred == 0 {
        assert!(start.elapsed() < WAIT);
        std::thread::sleep(Duration::from_millis(10));
    }
    queue.pause_job(id);
    wait_state(&queue, id, JobState::Paused, WAIT);
    assert!(!dir.temp_files().is_empty(), "일시정지에 임시 파일이 없다");

    queue.cancel_job(id);
    wait_state(&queue, id, JobState::Cancelled, WAIT);
    // 정리 태스크가 비동기라 잠시 기다린다.
    let start = Instant::now();
    while !dir.temp_files().is_empty() {
        assert!(start.elapsed() < WAIT, "취소 후 임시 파일이 남았다");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 실패한 작업은 Failed가 되고 retry로 다시 완료한다.
#[test]
fn failed_job_retries() {
    let backend = FakeBackend::default();
    backend.fail_next_reads.store(1, Ordering::SeqCst);
    backend.put_object("f.bin", vec![1u8; 100]);
    let (queue, _listener, dir) = queue_with(backend.clone());

    let id = queue.enqueue_download("f.bin", &dir.join("f.bin"));
    let failed = wait_state(&queue, id, JobState::Failed, WAIT);
    let err = failed.error.expect("Failed에 오류가 없다");
    assert_eq!(err.kind, pier_core::queue::ErrorKind::Network);

    queue.retry_job(id);
    wait_state(&queue, id, JobState::Completed, WAIT);
    assert_eq!(std::fs::read(dir.join("f.bin")).unwrap(), vec![1u8; 100]);
}

/// remove는 대기·종료 상태 작업을 목록에서 지운다.
#[test]
fn remove_queued_job() {
    let backend = FakeBackend::default();
    backend.gating.store(true, Ordering::SeqCst);
    backend.put_object("f.bin", vec![1u8; 10]);
    let (queue, _listener, dir) = queue_with(backend.clone());

    // 실행 슬롯을 채운 뒤 Queued 작업을 지운다.
    let busy: Vec<u64> = (0..3)
        .map(|i| {
            backend.put_object(&format!("busy{i}.bin"), vec![0u8; 10]);
            queue.enqueue_download(&format!("busy{i}.bin"), &dir.join(&format!("busy{i}.bin")))
        })
        .collect();
    let id = queue.enqueue_download("f.bin", &dir.join("f.bin"));
    assert_eq!(snapshot(&queue, id).state, JobState::Queued);

    queue.remove_job(id);
    assert!(
        queue.jobs().iter().all(|j| j.id != id),
        "remove 후에도 남았다"
    );
    backend.open_gate();
    for busy_id in busy {
        wait_state(&queue, busy_id, JobState::Completed, WAIT);
    }
}

/// 진행 통지는 스로틀된다 — 청크가 아주 많아도 Running 진행 통지는
/// 청크 수보다 훨씬 적다. 상태 변화 통지는 항상 간다.
#[test]
fn progress_emits_are_throttled() {
    let backend = FakeBackend::default();
    // 64개 청크가 거의 한 번에 끝나는 크기.
    backend.put_object("many.bin", vec![3u8; 64 * 1024]);
    let (queue, listener, dir) = queue_with_opts(
        backend.clone(),
        TransferOptions {
            chunk_size: 1024,
            concurrency: 8,
        },
    );

    let id = queue.enqueue_download("many.bin", &dir.join("many.bin"));
    wait_state(&queue, id, JobState::Completed, WAIT);

    let events = listener.events_for(id);
    let progress_emits = events
        .iter()
        .filter(|e| e.state == JobState::Running && e.transferred > 0)
        .count();
    // 진행 콜백은 청크 수(64)만큼 왔지만 통지는 스로틀로 훨씬 적다.
    assert!(
        progress_emits < 32,
        "통지가 스로틀되지 않았다: {progress_emits}번 (청크 64개)"
    );
    assert!(
        events.iter().any(|e| e.state == JobState::Completed),
        "Completed 통지가 없다"
    );
}

/// 작은 파일 업로드도 큐로 실행되고 완료된다.
#[test]
fn small_upload_completes() {
    let backend = FakeBackend::default();
    let (queue, _listener, dir) = queue_with(backend.clone());
    let src = dir.join("up.txt");
    std::fs::write(&src, b"hello-queue").unwrap();

    let id = queue.enqueue_upload(&src, "dst/", "up.txt", UploadMode::Fail);
    let done = wait_state(&queue, id, JobState::Completed, WAIT);
    assert_eq!(done.direction, pier_core::queue::Direction::Upload);
    assert!(
        done.result_entry.is_some(),
        "완료된 업로드에 result_entry가 없다"
    );
    assert_eq!(backend.put_calls.load(Ordering::SeqCst), 1);
    assert!(
        backend.objects.lock().unwrap().contains_key("dst/up.txt"),
        "원격에 객체가 없다"
    );
}

// ===== MinIO 통합 테스트 =====

fn minio_enabled() -> bool {
    std::env::var("PIER_TEST_MINIO").as_deref() == Ok("1")
}

fn minio_config() -> pier_core::config::ConnectionConfig {
    pier_core::config::ConnectionConfig {
        id: String::new(),
        display_name: "pier-test".to_string(),
        provider: pier_core::config::Provider::MinIO,
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

/// 시드용 Operator(루트가 버킷 루트).
fn minio_operator() -> opendal::Operator {
    let builder = opendal::services::S3::default()
        .bucket("pier-test")
        .region("us-east-1")
        .access_key_id("minioadmin")
        .secret_access_key("minioadmin")
        .disable_config_load()
        .disable_ec2_metadata()
        .root("/")
        .endpoint("http://127.0.0.1:9000");
    opendal::install_default();
    opendal::Operator::new(builder).expect("테스트 Operator 생성 실패")
}

/// opendal 전역 HTTP 클라이언트 런타임과 묶이므로 공유 런타임을 쓴다
/// (tests/transfer.rs와 같은 이유).
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

const BIG_KEY: &str = "docs/p4-queue/big.bin";
const BIG_SIZE: usize = 40 * 1024 * 1024;

fn big_data() -> Vec<u8> {
    (0..BIG_SIZE).map(|i| (i % 256) as u8).collect()
}

/// MinIO 백엔드의 Connection + 대기열을 만든다.
fn minio_queue(dir: &TempDir, chunk_size: u64) -> TransferQueue<OpendalS3> {
    let backend = OpendalS3::new(&minio_config(), "minioadmin").expect("백엔드 생성 실패");
    let store = MetadataStore::open(&dir.join("metadata.sqlite")).expect("메타데이터 열기 실패");
    let conn = Arc::new(Connection::with_backend(backend, store));
    let opts = TransferOptions {
        chunk_size,
        concurrency: 2,
    };
    // 대기열은 호출자의 tokio 런타임 위에서 작업을 실행한다.
    let _enter = runtime().enter();
    TransferQueue::new(conn, opts, None).expect("대기열 생성 실패")
}

/// 40MB 다운로드 중 pause → 임시 파일 유지 → resume → 바이트 일치.
#[test]
fn download_pause_resume_minio() {
    if !minio_enabled() {
        return;
    }
    let dir = TempDir::new("dl-pause");
    runtime().block_on(async {
        let operator = minio_operator();
        let data = big_data();
        let ok = operator
            .stat(BIG_KEY)
            .await
            .map(|m| m.content_length() == data.len() as u64)
            .unwrap_or(false);
        if !ok {
            operator
                .write(BIG_KEY, data.clone())
                .await
                .expect("시드 업로드 실패");
        }
        drop(operator);
    });

    let queue = minio_queue(&dir, 1024 * 1024);
    let dest = dir.join("big.bin");
    let id = queue.enqueue_download(BIG_KEY, &dest);

    // 첫 청크가 진행될 때까지 기다리고 멈춘다.
    let start = Instant::now();
    loop {
        let job = snapshot(&queue, id);
        if job.state == JobState::Completed {
            break;
        }
        if job.transferred > 0 {
            queue.pause_job(id);
            break;
        }
        assert!(start.elapsed() < WAIT, "다운로드가 시작되지 않았다");
        std::thread::sleep(Duration::from_millis(20));
    }
    if snapshot(&queue, id).state != JobState::Completed {
        wait_state(&queue, id, JobState::Paused, WAIT);
        // 일시정지해도 임시 파일이 남아 있어야 한다.
        assert!(!dir.temp_files().is_empty(), "pause 후 임시 파일이 없다");
        let paused = snapshot(&queue, id);
        assert!(paused.transferred > 0);

        queue.resume_job(id);
    }
    let done = wait_state(&queue, id, JobState::Completed, Duration::from_secs(60));
    assert_eq!(done.transferred, BIG_SIZE as u64);
    assert_eq!(std::fs::read(&dest).unwrap(), big_data(), "내용이 다르다");
    assert!(
        dir.temp_files().is_empty(),
        "완료 후 임시 파일이 남았다: {:?}",
        dir.temp_files()
    );
    assert!(done.result_entry.is_some());
}

/// 다운로드 중 cancel → Cancelled + 임시 파일 제거.
#[test]
fn download_cancel_removes_tmp_minio() {
    if !minio_enabled() {
        return;
    }
    let dir = TempDir::new("dl-cancel");
    runtime().block_on(async {
        let operator = minio_operator();
        let data = big_data();
        let ok = operator
            .stat(BIG_KEY)
            .await
            .map(|m| m.content_length() == data.len() as u64)
            .unwrap_or(false);
        if !ok {
            operator
                .write(BIG_KEY, data.clone())
                .await
                .expect("시드 업로드 실패");
        }
        drop(operator);
    });

    let queue = minio_queue(&dir, 1024 * 1024);
    let dest = dir.join("big.bin");
    let id = queue.enqueue_download(BIG_KEY, &dest);

    let start = Instant::now();
    while snapshot(&queue, id).transferred == 0 {
        assert!(start.elapsed() < WAIT, "다운로드가 시작되지 않았다");
        std::thread::sleep(Duration::from_millis(20));
    }
    queue.cancel_job(id);
    wait_state(&queue, id, JobState::Cancelled, WAIT);
    // 정리 태스크가 비동기라 잠시 기다린다.
    let start = Instant::now();
    while !dir.temp_files().is_empty() || dest.exists() {
        assert!(start.elapsed() < WAIT, "취소 후 임시 파일/결과가 남았다");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 폴더 다운로드 중 pause → 완료된 파일은 재다운로드하지 않는다.
/// (pause 시점에 존재하던 완료 파일의 수정시각이 resume 후에도 같다.)
#[test]
fn folder_download_pause_resume_minio() {
    if !minio_enabled() {
        return;
    }
    let dir = TempDir::new("folder-pause");
    let prefix = "docs/p4-queue-folder/";
    runtime().block_on(async {
        let operator = minio_operator();
        // 파일 여덟 개(각 512KB — 빨리 끝나는 것과 늦게 끝나는 것이 섞이게).
        for i in 0..8 {
            let key = format!("{prefix}f{i}.bin");
            let data = vec![i as u8; 512 * 1024];
            let ok = operator
                .stat(&key)
                .await
                .map(|m| m.content_length() == data.len() as u64)
                .unwrap_or(false);
            if !ok {
                operator.write(&key, data).await.expect("시드 업로드 실패");
            }
        }
        drop(operator);
    });

    let queue = minio_queue(&dir, 256 * 1024);
    let id = queue.enqueue_download_folder(prefix, &dir.0);

    // 파일 하나 이상이 끝날 때까지 기다리고 멈춘다.
    let start = Instant::now();
    loop {
        let job = snapshot(&queue, id);
        if job.state == JobState::Completed {
            break;
        }
        if job.files_done >= 1 {
            queue.pause_job(id);
            break;
        }
        assert!(start.elapsed() < WAIT, "폴더 다운로드가 시작되지 않았다");
        std::thread::sleep(Duration::from_millis(20));
    }
    if snapshot(&queue, id).state != JobState::Completed {
        wait_state(&queue, id, JobState::Paused, WAIT);
        // pause 시점에 rename까지 끝난 완료 파일들의 수정시각을 기록한다.
        let target = dir.0.join("p4-queue-folder");
        let done_files: HashMap<PathBuf, std::time::SystemTime> = std::fs::read_dir(&target)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| {
                        e.path().is_file()
                            && !e.file_name().to_string_lossy().contains("pierdownload")
                    })
                    .map(|e| (e.path(), e.metadata().unwrap().modified().unwrap()))
                    .collect()
            })
            .unwrap_or_default();

        queue.resume_job(id);
        let done = wait_state(&queue, id, JobState::Completed, Duration::from_secs(60));
        // 완료 파일은 다시 받지 않았다 — 수정시각이 pause 때 그대로다.
        for (path, mtime) in done_files {
            assert_eq!(
                path.metadata().unwrap().modified().unwrap(),
                mtime,
                "완료 파일 {path:?}을 재다운로드했다"
            );
        }
        assert!(done.files_done >= 1);
    }
    // 여덟 파일 모두 내려왔고 내용이 맞다.
    let target = dir.0.join("p4-queue-folder");
    for i in 0..8 {
        assert_eq!(
            std::fs::read(target.join(format!("f{i}.bin"))).unwrap(),
            vec![i as u8; 512 * 1024]
        );
    }
    assert!(dir.temp_files().is_empty());
}
