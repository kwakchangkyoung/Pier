//! SPEC Phase 4 전송 대기열: 연결당 최대 3개 작업의 FIFO 큐.
//!
//! - 작업 종류: 파일 다운로드, 폴더 다운로드, 파일 업로드, 폴더 업로드.
//! - 상태: Queued → Running → (Paused | Completed | Failed | Cancelled).
//! - 대기열은 메모리에만 있다 — 앱 종료 시 사라지고, 남은 멀티파트
//!   업로드는 다음 실행 때 `S3Raw::cleanup_pending_uploads`가 정리한다.
//! - 일시정지(pause)는 전송 계층의 취소 토큰으로 전달해 완료 청크·파트
//!   상태를 보존하고, 명시적 취소(cancel)는 상태를 버리고 임시
//!   파일·멀티파트를 정리한다.
//! - 진행 통지는 작업당 100ms로 스로틀하되 상태 변화는 즉시 보낸다.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use crate::backend::Backend;
use crate::enumerate::Connection;
use crate::error::Error;
use crate::item::Item;
use crate::key;
use crate::s3_raw::{self, ResumableUpload, S3Raw};
use crate::transfer::{
    self, CancelToken, ProgressSink, ResumableDownload, ResumableFolderDownload,
    ResumableFolderUpload, TransferOptions, UploadMode, RESUMABLE_UPLOAD_MIN,
};

/// 연결당 동시에 실행하는 작업 수.
const MAX_RUNNING: usize = 3;
/// 진행 통지 스로틀 간격.
const EMIT_INTERVAL: Duration = Duration::from_millis(100);
/// 속도 측정 창(최근 약 3초 이동 평균).
const SPEED_WINDOW: Duration = Duration::from_secs(3);

/// 전송 방향.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Download,
    Upload,
}

/// 대기열 작업 상태.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Queued,
    Running,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

/// 실패 분류. FFI의 PierErrorKind에 대응한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Authentication,
    Network,
    NotFound,
    Conflict,
    Cancelled,
    Other,
}

/// 작업 실패 정보.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobError {
    pub kind: ErrorKind,
    pub message: String,
}

/// 작업의 외부 공개 스냅샷. FFI의 PierJob에 대응한다.
#[derive(Debug, Clone)]
pub struct JobSnapshot {
    pub id: u64,
    pub direction: Direction,
    pub is_folder: bool,
    /// 표시용 이름(파일·폴더 이름).
    pub name: String,
    /// 요청한 원격 키 또는 접두사. KeepBoth로 확정되면 실제 키로 바뀐다.
    pub remote_key: String,
    /// 요청한 로컬 경로(다운로드는 대상, 업로드는 원본).
    pub local_path: String,
    pub state: JobState,
    pub transferred: u64,
    pub total: u64,
    /// 최근 약 3초 이동 평균 전송 속도(바이트/초).
    pub bytes_per_sec: u64,
    pub files_done: u64,
    pub files_total: u64,
    pub error: Option<JobError>,
    /// 완료 시 결과 항목(업로드는 원격 stat, 다운로드는 로컬 stat).
    pub result_entry: Option<Item>,
}

/// 작업 변화 통지 콜백. 백그라운드 스레드에서 호출된다.
pub trait QueueListener: Send + Sync {
    fn on_job_update(&self, job: JobSnapshot);
}

/// 작업의 실제 내용. 스냅샷에는 없는 내부 정보.
#[derive(Debug, Clone)]
enum JobKind {
    DownloadFile {
        key: String,
        dest: PathBuf,
    },
    DownloadFolder {
        prefix: String,
        dest_dir: PathBuf,
    },
    UploadFile {
        src: PathBuf,
        parent: String,
        name: String,
        mode: UploadMode,
    },
    UploadFolder {
        src_dir: PathBuf,
        parent: String,
        mode: UploadMode,
    },
}

/// 최근 SPEED_WINDOW 안의 누적 바이트로 이동 평균 속도를 낸다.
#[derive(Default)]
struct SpeedMeter {
    /// (기록 시각, 그 시각까지의 누적 바이트).
    samples: VecDeque<(Instant, u64)>,
}

impl SpeedMeter {
    fn record(&mut self, transferred: u64) {
        let now = Instant::now();
        if self.samples.back().is_some_and(|(_, v)| *v == transferred) {
            return;
        }
        self.samples.push_back((now, transferred));
        while self
            .samples
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) > SPEED_WINDOW)
        {
            self.samples.pop_front();
        }
    }

    fn bytes_per_sec(&self) -> u64 {
        let (Some((t0, v0)), Some((t1, v1))) = (self.samples.front(), self.samples.back()) else {
            return 0;
        };
        let secs = t1.duration_since(*t0).as_secs_f64();
        if secs <= 0.0 {
            return 0;
        }
        (v1.saturating_sub(*v0) as f64 / secs) as u64
    }
}

/// 큐 안의 작업 하나. jobs 맵 잠금 아래에서만 읽고 쓴다.
struct JobEntry {
    id: u64,
    kind: JobKind,
    direction: Direction,
    name: String,
    remote_key: String,
    local_path: String,
    state: JobState,
    transferred: u64,
    total: u64,
    files_done: u64,
    files_total: u64,
    speed: SpeedMeter,
    error: Option<JobError>,
    result_entry: Option<Item>,
    /// 일시정지 신호 — 실행 중 전송의 cancel 토큰으로 전달된다.
    /// 재개 때 새 토큰으로 교체한다.
    pause_token: CancelToken,
    /// 명시적 취소 표시. 실행 끝에서 상태를 버리고 정리하게 한다.
    cancel_flag: bool,
    /// 이어받기 상태들 — 실행 중에는 runner가 꺼내 들고 있다.
    dl_state: Option<ResumableDownload>,
    folder_dl_state: Option<ResumableFolderDownload>,
    ul_state: Option<ResumableUpload>,
    folder_ul_state: Option<ResumableFolderUpload>,
    /// 파일 업로드가 확정한 원격 키(KeepBoth 이름). 재개·재시도 때 재사용.
    resolved_key: Option<String>,
    /// 마지막으로 리스너에 보낸 시각(스로틀 기준).
    last_emit: Option<Instant>,
}

/// runner가 entry에서 꺼내 실행 중 들고 있는 이어받기 상태 묶음.
#[derive(Default)]
struct ResumeBag {
    dl: Option<ResumableDownload>,
    folder_dl: Option<ResumableFolderDownload>,
    ul: Option<ResumableUpload>,
    folder_ul: Option<ResumableFolderUpload>,
}

impl ResumeBag {
    fn has_any(&self) -> bool {
        self.dl.is_some()
            || self.folder_dl.is_some()
            || self.ul.is_some()
            || self.folder_ul.is_some()
    }
}

/// 큐가 공유하는 상태 전부.
struct Shared<B: Backend> {
    conn: Arc<Connection<B>>,
    raw: Option<Arc<S3Raw>>,
    opts: TransferOptions,
    jobs: Mutex<BTreeMap<u64, JobEntry>>,
    listener: Mutex<Option<Arc<dyn QueueListener>>>,
    next_id: AtomicU64,
    /// 작업을 spawn할 호출자의 tokio 런타임.
    handle: tokio::runtime::Handle,
}

/// 연결 하나의 전송 대기열. `Arc<Connection>`을 받아 만들고
/// 모든 제어 메서드는 동기다(없는 id는 무시).
///
/// `raw`는 이어올리기용 원시 S3 백엔드다. None이면 큰 파일 업로드도
/// 일반 멀티파트로 처리해 중단 시 그 파일은 처음부터 올린다.
///
/// 작업은 `new`를 호출한 스레드의 tokio 런타임(멀티스레드) 위에서
/// 돌기 때문에 `new`는 런타임 안에서 호출해야 한다.
pub struct TransferQueue<B: Backend> {
    shared: Arc<Shared<B>>,
}

impl<B> TransferQueue<B>
where
    B: Backend + Clone + Send + Sync + 'static,
{
    pub fn new(
        conn: Arc<Connection<B>>,
        opts: TransferOptions,
        raw: Option<Arc<S3Raw>>,
    ) -> Result<Self, Error> {
        let handle = tokio::runtime::Handle::try_current().map_err(|_| {
            Error::Other("전송 대기열은 tokio 런타임 안에서 만들어야 한다".to_string())
        })?;
        let shared = Arc::new(Shared {
            conn,
            raw,
            opts,
            jobs: Mutex::new(BTreeMap::new()),
            listener: Mutex::new(None),
            next_id: AtomicU64::new(1),
            handle,
        });
        Ok(Self { shared })
    }

    /// 작업 변화 리스너를 등록한다. None이면 해제.
    pub fn set_listener(&self, listener: Option<Arc<dyn QueueListener>>) {
        *self
            .shared
            .listener
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = listener;
    }

    /// key 객체를 dest 파일로 받는 작업을 넣는다.
    pub fn enqueue_download(&self, key: &str, dest: &Path) -> u64 {
        self.enqueue(
            JobKind::DownloadFile {
                key: key.to_string(),
                dest: dest.to_path_buf(),
            },
            Direction::Download,
            key::name(key).to_string(),
            key.to_string(),
            dest.to_string_lossy().into_owned(),
        )
    }

    /// prefix 폴더 전체를 dest_dir 아래에 받는 작업을 넣는다.
    pub fn enqueue_download_folder(&self, prefix: &str, dest_dir: &Path) -> u64 {
        self.enqueue(
            JobKind::DownloadFolder {
                prefix: prefix.to_string(),
                dest_dir: dest_dir.to_path_buf(),
            },
            Direction::Download,
            key::name(prefix.trim_end_matches('/')).to_string(),
            prefix.to_string(),
            dest_dir.to_string_lossy().into_owned(),
        )
    }

    /// src 파일을 parent 아래 name으로 올리는 작업을 넣는다.
    pub fn enqueue_upload(&self, src: &Path, parent: &str, name: &str, mode: UploadMode) -> u64 {
        let parent = key::normalize_prefix(parent);
        self.enqueue(
            JobKind::UploadFile {
                src: src.to_path_buf(),
                parent: parent.clone(),
                name: name.to_string(),
                mode,
            },
            Direction::Upload,
            name.to_string(),
            key::child_key(&parent, name, false),
            src.to_string_lossy().into_owned(),
        )
    }

    /// src_dir를 parent 아래 폴더로 올리는 작업을 넣는다.
    pub fn enqueue_upload_folder(&self, src_dir: &Path, parent: &str, mode: UploadMode) -> u64 {
        let parent = key::normalize_prefix(parent);
        let name = src_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.enqueue(
            JobKind::UploadFolder {
                src_dir: src_dir.to_path_buf(),
                parent: parent.clone(),
                mode,
            },
            Direction::Upload,
            name.clone(),
            key::child_key(&parent, &name, true),
            src_dir.to_string_lossy().into_owned(),
        )
    }

    fn enqueue(
        &self,
        kind: JobKind,
        direction: Direction,
        name: String,
        remote_key: String,
        local_path: String,
    ) -> u64 {
        let shared = &self.shared;
        let id = shared.next_id.fetch_add(1, Ordering::SeqCst);
        lock_jobs(shared).insert(
            id,
            JobEntry {
                id,
                kind,
                direction,
                name,
                remote_key,
                local_path,
                state: JobState::Queued,
                transferred: 0,
                total: 0,
                files_done: 0,
                files_total: 0,
                speed: SpeedMeter::default(),
                error: None,
                result_entry: None,
                pause_token: CancelToken::new(),
                cancel_flag: false,
                dl_state: None,
                folder_dl_state: None,
                ul_state: None,
                folder_ul_state: None,
                resolved_key: None,
                last_emit: None,
            },
        );
        emit(shared, id, true);
        pump(shared);
        id
    }

    /// 실행 중 작업을 멈추고 Paused로 둔다. 완료 청크·파트 상태는 보존.
    /// Queued 작업은 시작하지 않고 바로 Paused가 된다.
    pub fn pause_job(&self, id: u64) {
        let shared = &self.shared;
        let mut jobs = lock_jobs(shared);
        let Some(job) = jobs.get_mut(&id) else {
            return;
        };
        match job.state {
            JobState::Running => job.pause_token.cancel(),
            JobState::Queued => {
                job.state = JobState::Paused;
                drop(jobs);
                emit(shared, id, true);
            }
            _ => {}
        }
    }

    /// Paused 작업을 대기열에 다시 넣는다. id가 작으므로 먼저 넣은
    /// 작업이 여전히 먼저 실행되는 FIFO를 유지한다.
    pub fn resume_job(&self, id: u64) {
        let shared = &self.shared;
        {
            let mut jobs = lock_jobs(shared);
            let Some(job) = jobs.get_mut(&id) else {
                return;
            };
            if job.state != JobState::Paused {
                return;
            }
            job.state = JobState::Queued;
            job.pause_token = CancelToken::new();
            job.error = None;
        }
        emit(shared, id, true);
        pump(shared);
    }

    /// 작업을 취소한다. 임시 파일·진행 중 멀티파트를 정리한다.
    pub fn cancel_job(&self, id: u64) {
        let shared = &self.shared;
        let mut resume = ResumeBag::default();
        {
            let mut jobs = lock_jobs(shared);
            let Some(job) = jobs.get_mut(&id) else {
                return;
            };
            match job.state {
                // 실행 중이면 runner가 cancel_flag를 보고 정리·전이한다.
                JobState::Running => {
                    job.cancel_flag = true;
                    job.pause_token.cancel();
                    return;
                }
                JobState::Queued => job.state = JobState::Cancelled,
                // 일시정지 상태는 runner가 없어 여기서 정리를 띄운다.
                JobState::Paused => {
                    job.state = JobState::Cancelled;
                    job.cancel_flag = true;
                    take_states(job, &mut resume);
                }
                _ => return,
            }
        }
        emit(shared, id, true);
        if resume.has_any() {
            spawn_cleanup(shared, resume);
        }
        pump(shared);
    }

    /// Failed 작업을 같은 자리(FIFO)에서 다시 시도한다.
    /// 남은 이어받기 상태가 있으면 이어서 진행한다.
    pub fn retry_job(&self, id: u64) {
        let shared = &self.shared;
        {
            let mut jobs = lock_jobs(shared);
            let Some(job) = jobs.get_mut(&id) else {
                return;
            };
            if job.state != JobState::Failed {
                return;
            }
            job.state = JobState::Queued;
            job.error = None;
            job.pause_token = CancelToken::new();
            job.speed = SpeedMeter::default();
        }
        emit(shared, id, true);
        pump(shared);
    }

    /// 작업을 대기열에서 지운다. 실행 중이면 취소를 보낸 뒤 제거해
    /// runner가 남은 정리를 하게 하고, 일시정지 상태의 임시
    /// 파일·멀티파트는 여기서 정리한다.
    pub fn remove_job(&self, id: u64) {
        let shared = &self.shared;
        let mut resume = ResumeBag::default();
        {
            let mut jobs = lock_jobs(shared);
            let Some(mut job) = jobs.remove(&id) else {
                return;
            };
            if job.state == JobState::Running {
                // 상태는 runner가 들고 있다 — 취소만 보내면 runner가
                // entry가 없어진 것을 보고 정리한다.
                job.pause_token.cancel();
            } else {
                take_states(&mut job, &mut resume);
            }
        }
        if resume.has_any() {
            spawn_cleanup(shared, resume);
        }
    }

    /// 모든 작업의 스냅샷을 id 순(FIFO)으로 반환한다.
    pub fn jobs(&self) -> Vec<JobSnapshot> {
        lock_jobs(&self.shared).values().map(snapshot_of).collect()
    }
}

/// entry의 이어받기 상태를 꺼낸다. 실행 중에는 runner가 들고 있으므로
/// 여기서는 runner가 채워 넣은 값만 나온다.
fn take_states(job: &mut JobEntry, bag: &mut ResumeBag) {
    bag.dl = job.dl_state.take();
    bag.folder_dl = job.folder_dl_state.take();
    bag.ul = job.ul_state.take();
    bag.folder_ul = job.folder_ul_state.take();
}

/// runner가 실행 끝에 상태를 entry에 되돌려 놓는다.
fn put_states(job: &mut JobEntry, bag: ResumeBag) {
    job.dl_state = bag.dl;
    job.folder_dl_state = bag.folder_dl;
    job.ul_state = bag.ul;
    job.folder_ul_state = bag.folder_ul;
}

fn lock_jobs<B: Backend>(shared: &Shared<B>) -> MutexGuard<'_, BTreeMap<u64, JobEntry>> {
    shared.jobs.lock().unwrap_or_else(|e| e.into_inner())
}

fn lock_listener<B: Backend>(shared: &Shared<B>) -> Option<Arc<dyn QueueListener>> {
    shared
        .listener
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

fn snapshot_of(job: &JobEntry) -> JobSnapshot {
    JobSnapshot {
        id: job.id,
        direction: job.direction,
        is_folder: matches!(
            job.kind,
            JobKind::DownloadFolder { .. } | JobKind::UploadFolder { .. }
        ),
        name: job.name.clone(),
        remote_key: job.remote_key.clone(),
        local_path: job.local_path.clone(),
        state: job.state,
        transferred: job.transferred,
        total: job.total,
        bytes_per_sec: job.speed.bytes_per_sec(),
        files_done: job.files_done,
        files_total: job.files_total,
        error: job.error.clone(),
        result_entry: job.result_entry.clone(),
    }
}

/// 스냅샷을 만들어 리스너에 보낸다. force=false면 작업당
/// EMIT_INTERVAL보다 잦은 통지는 건너뛴다. 리스너 호출은 잠금 밖에서 한다.
fn emit<B: Backend>(shared: &Arc<Shared<B>>, id: u64, force: bool) {
    let snapshot = {
        let mut jobs = lock_jobs(shared);
        let Some(job) = jobs.get_mut(&id) else {
            return;
        };
        if !force
            && job
                .last_emit
                .is_some_and(|last| last.elapsed() < EMIT_INTERVAL)
        {
            return;
        }
        job.last_emit = Some(Instant::now());
        snapshot_of(job)
    };
    if let Some(listener) = lock_listener(shared) {
        listener.on_job_update(snapshot);
    }
}

/// 빈 자리가 있으면 Queued 작업을 id 순으로 Running으로 바꾸고 띄운다.
fn pump<B>(shared: &Arc<Shared<B>>)
where
    B: Backend + Clone + Send + Sync + 'static,
{
    let mut start = Vec::new();
    {
        let mut jobs = lock_jobs(shared);
        let running = jobs
            .values()
            .filter(|j| j.state == JobState::Running)
            .count();
        let mut free = MAX_RUNNING.saturating_sub(running);
        for (id, job) in jobs.iter_mut() {
            if free == 0 {
                break;
            }
            if job.state == JobState::Queued {
                job.state = JobState::Running;
                job.pause_token = CancelToken::new();
                job.speed = SpeedMeter::default();
                start.push(*id);
                free -= 1;
            }
        }
    }
    for id in start {
        emit(shared, id, true);
        // 호출자의 런타임에서 작업을 실행한다.
        drop(shared.handle.spawn(run_job(Arc::clone(shared), id)));
    }
}

/// 작업 하나를 실행하고 결과에 따라 상태를 옮긴다.
async fn run_job<B>(shared: Arc<Shared<B>>, id: u64)
where
    B: Backend + Clone + Send + Sync + 'static,
{
    // kind·토큰·재개 상태를 entry에서 꺼낸다 — 실행 동안 jobs 잠금을
    // 잡지 않게 상태는 runner가 들고 다닌다.
    let (kind, pause_token, resolved_key, mut bag) = {
        let mut jobs = lock_jobs(&shared);
        let Some(job) = jobs.get_mut(&id) else {
            return;
        };
        let mut bag = ResumeBag::default();
        take_states(job, &mut bag);
        (
            job.kind.clone(),
            job.pause_token.clone(),
            job.resolved_key.clone(),
            bag,
        )
    };

    let sink = JobSink {
        shared: Arc::clone(&shared),
        id,
    };
    let backend = shared.conn.backend();
    let opts = &shared.opts;
    let token = &pause_token;

    let outcome: Result<Item, Error> = match &kind {
        JobKind::DownloadFile { key, dest } => {
            match transfer::download_resumable(backend, key, dest, opts, &mut bag.dl, &sink, token)
                .await
            {
                Ok(_) => local_item(dest).await,
                Err(e) => Err(e),
            }
        }
        JobKind::DownloadFolder { prefix, dest_dir } => {
            match transfer::download_folder_resumable(
                backend,
                prefix,
                dest_dir,
                opts,
                &mut bag.folder_dl,
                &sink,
                token,
            )
            .await
            {
                Ok(fd) => local_item(&fd.local_path).await,
                Err(e) => Err(e),
            }
        }
        JobKind::UploadFile {
            src,
            parent,
            name,
            mode,
        } => {
            run_upload_file(
                &shared,
                id,
                src,
                parent,
                name,
                mode,
                resolved_key,
                &mut bag,
                &sink,
                token,
            )
            .await
        }
        JobKind::UploadFolder {
            src_dir,
            parent,
            mode,
        } => {
            match transfer::upload_folder_resumable(
                backend,
                shared.raw.clone(),
                src_dir,
                parent,
                mode,
                opts,
                &mut bag.folder_ul,
                &sink,
                token,
            )
            .await
            {
                Ok(_) => finish_folder_upload(&shared, src_dir, parent).await,
                Err(e) => Err(e),
            }
        }
    };

    finish_run(&shared, id, outcome, bag).await;
}

/// 단일 파일 업로드를 실행한다. 작은 파일은 conn.upload(캐시 반영
/// 포함), 큰 파일은 upload_resumable로 올린다.
#[allow(clippy::too_many_arguments)]
async fn run_upload_file<B>(
    shared: &Arc<Shared<B>>,
    id: u64,
    src: &Path,
    parent: &str,
    name: &str,
    mode: &UploadMode,
    resolved_key: Option<String>,
    bag: &mut ResumeBag,
    sink: &JobSink<B>,
    token: &CancelToken,
) -> Result<Item, Error>
where
    B: Backend + Clone + Send + Sync + 'static,
{
    let size = tokio::fs::metadata(src).await?.len();
    let raw = match (shared.raw.as_deref(), size > RESUMABLE_UPLOAD_MIN) {
        (Some(raw), true) => raw,
        // 작은 파일이나 raw가 없으면 캐시 반영까지 하는 conn.upload.
        _ => {
            return shared
                .conn
                .upload(src, parent, name, mode, &shared.opts, sink, token)
                .await;
        }
    };
    // KeepBoth로 고른 이름은 재개·재시도 때도 같아야 한다 —
    // 업로드 ID가 키에 묶이기 때문이다.
    let remote_key = match resolved_key {
        Some(k) => k,
        None => {
            let resolved = resolve_upload_key(shared.conn.backend(), parent, name, mode).await?;
            let mut jobs = lock_jobs(shared);
            if let Some(job) = jobs.get_mut(&id) {
                job.resolved_key = Some(resolved.clone());
                job.remote_key = resolved.clone();
            }
            resolved
        }
    };
    s3_raw::upload_resumable(
        raw,
        src,
        &remote_key,
        transfer::mode_condition(mode),
        &shared.opts,
        &mut bag.ul,
        sink,
        token,
    )
    .await?;
    let item = shared.conn.backend().stat(&remote_key).await?;
    // 부모 목록을 갱신해 새 항목을 캐시에 넣는다(실패해도 업로드는 성공).
    let _ = shared.conn.refresh(parent).await;
    Ok(item)
}

/// 파일 업로드의 최종 원격 키를 정한다. KeepBoth만 원격 목록으로
/// 비어 있는 이름을 고른다.
async fn resolve_upload_key<B>(
    backend: &B,
    parent: &str,
    name: &str,
    mode: &UploadMode,
) -> Result<String, Error>
where
    B: Backend + Send + Sync,
{
    let final_name = if matches!(mode, UploadMode::KeepBoth) {
        let taken: HashSet<String> = backend
            .list_folder(parent)
            .await?
            .iter()
            .map(|i| i.name.clone())
            .collect();
        key::available_name(&taken, name)
    } else {
        name.to_string()
    };
    Ok(key::child_key(parent, &final_name, false))
}

/// 폴더 업로드 완료 후: 부모 목록을 갱신해 새 폴더 항목을 캐시에 넣고
/// 결과 Item을 만든다.
async fn finish_folder_upload<B>(
    shared: &Arc<Shared<B>>,
    src_dir: &Path,
    parent: &str,
) -> Result<Item, Error>
where
    B: Backend + Send + Sync,
{
    // 갱신이 실패해도 업로드 자체는 성공 — 목록은 다음 열람 때 맞는다.
    let _ = shared.conn.refresh(parent).await;
    let name = src_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(Item::folder(key::child_key(parent, &name, true)))
}

/// 실행 결과로 작업 상태를 정리한다. 상태 변화 통지는 잠금 밖에서 보낸다.
async fn finish_run<B>(
    shared: &Arc<Shared<B>>,
    id: u64,
    outcome: Result<Item, Error>,
    bag: ResumeBag,
) where
    B: Backend + Clone + Send + Sync + 'static,
{
    enum Step {
        /// entry가 없다(remove됨) — 조용히 정리만 한다.
        Gone,
        /// 명시적 취소 — 상태를 버리고 정리한다.
        Cancel,
        /// 상태를 보존한 채 Paused/Failed/Completed로 이미 전이했다.
        Done,
    }
    let (step, cleanup_bag) = {
        let mut jobs = lock_jobs(shared);
        match jobs.get_mut(&id) {
            None => (Step::Gone, Some(bag)),
            Some(job) => {
                if job.cancel_flag {
                    (Step::Cancel, Some(bag))
                } else {
                    match outcome {
                        Ok(item) => {
                            put_states(job, bag);
                            job.state = JobState::Completed;
                            job.result_entry = Some(item);
                            job.error = None;
                            job.transferred = job.total;
                            (Step::Done, None)
                        }
                        Err(err) => {
                            let cancelled = matches!(err, Error::Cancelled(_));
                            put_states(job, bag);
                            if cancelled {
                                job.state = JobState::Paused;
                            } else {
                                job.state = JobState::Failed;
                                job.error = Some(job_error(&err));
                            }
                            (Step::Done, None)
                        }
                    }
                }
            }
        }
    };
    match step {
        Step::Gone => {
            if let Some(bag) = cleanup_bag {
                cleanup(Arc::clone(shared), bag).await;
            }
        }
        Step::Cancel => {
            if let Some(bag) = cleanup_bag {
                cleanup(Arc::clone(shared), bag).await;
            }
            {
                let mut jobs = lock_jobs(shared);
                if let Some(job) = jobs.get_mut(&id) {
                    job.state = JobState::Cancelled;
                    job.error = None;
                }
            }
            emit(shared, id, true);
        }
        Step::Done => emit(shared, id, true),
    }
    pump(shared);
}

/// 취소·삭제된 작업의 잔여물을 정리한다. 다운로드는 임시 파일 삭제,
/// 업로드는 진행 중 멀티파트 abort.
async fn cleanup<B>(shared: Arc<Shared<B>>, mut bag: ResumeBag)
where
    B: Backend + Send + Sync,
{
    transfer::discard_download(&mut bag.dl).await;
    transfer::discard_folder_download(&mut bag.folder_dl).await;
    if let Some(raw) = shared.raw.as_deref() {
        if let Some(state) = bag.ul.take() {
            let _ = s3_raw::abort_upload(raw, &state).await;
        }
        if let Some(folder) = bag.folder_ul.take() {
            for state in folder.in_progress.values() {
                let _ = s3_raw::abort_upload(raw, state).await;
            }
        }
    }
}

/// 정리를 런타임에 넘긴다 — 제어 API는 동기라 여기서 await하지 않는다.
fn spawn_cleanup<B>(shared: &Arc<Shared<B>>, bag: ResumeBag)
where
    B: Backend + Clone + Send + Sync + 'static,
{
    drop(shared.handle.spawn(cleanup(Arc::clone(shared), bag)));
}

/// 로컬 경로의 stat으로 Item을 만든다(다운로드 결과 표시용).
async fn local_item(path: &Path) -> Result<Item, Error> {
    let meta = tokio::fs::metadata(path).await?;
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64);
    let display = path.to_string_lossy().into_owned();
    if meta.is_dir() {
        Ok(Item::folder(format!("{display}/")))
    } else {
        Ok(Item::file(display, meta.len(), None, modified))
    }
}

/// 코어 에러를 작업 에러 분류로 변환한다.
fn job_error(err: &Error) -> JobError {
    let kind = match err {
        Error::Authentication(_) => ErrorKind::Authentication,
        Error::Network(_) => ErrorKind::Network,
        Error::NotFound(_) => ErrorKind::NotFound,
        Error::Conflict(_) => ErrorKind::Conflict,
        Error::Cancelled(_) => ErrorKind::Cancelled,
        Error::Other(_) => ErrorKind::Other,
    };
    JobError {
        kind,
        message: err.to_string(),
    }
}

/// 진행률을 entry에 반영하고 스로틀된 통지를 보내는 ProgressSink.
struct JobSink<B: Backend> {
    shared: Arc<Shared<B>>,
    id: u64,
}

impl<B: Backend + Send + Sync> ProgressSink for JobSink<B> {
    fn on_progress(&self, transferred: u64, total: u64) {
        {
            let mut jobs = lock_jobs(&self.shared);
            if let Some(job) = jobs.get_mut(&self.id) {
                job.transferred = transferred;
                if total > 0 {
                    job.total = total;
                }
                job.speed.record(transferred);
            }
        }
        emit(&self.shared, self.id, false);
    }

    fn on_files(&self, done: u64, total: u64) {
        {
            let mut jobs = lock_jobs(&self.shared);
            if let Some(job) = jobs.get_mut(&self.id) {
                job.files_done = done;
                if total > 0 {
                    job.files_total = total;
                }
            }
        }
        emit(&self.shared, self.id, false);
    }
}
