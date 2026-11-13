//! SPEC 4.4 전송: 범위 읽기 병렬 다운로드, 업로드, 취소, 진행률.
//!
//! 다운로드는 객체 크기가 chunk_size 이하면 범위 요청 1개, 초과하면
//! chunk_size 범위를 최대 concurrency개 동시에 요청해 임시 파일의 정해진
//! 오프셋에 직접 쓴다. 모든 작업은 dest와 같은 디렉터리의 임시 파일
//! (`.<이름>.pierdownload`)에 쓰고, 크기 검증 후 rename한다.
//! 실패·취소 시 임시 파일을 지우고 dest는 건드리지 않는다.
//!
//! 업로드는 크기가 part_size 이하면 단일 PUT, 초과하면 part_size 파트의
//! 멀티파트 업로드를 backend.put_file에 위임한다. 취소·실패 시 미완료
//! 멀티파트는 백엔드가 abort해 원격에 객체를 남기지 않는다(SPEC 3.4).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Notify, Semaphore};
use tokio::task::JoinSet;

use crate::backend::{Backend, PutOptions, WriteCondition};
use crate::error::Error;
use crate::ignore;
use crate::item::Item;
use crate::key;
use crate::s3_raw::{self, ResumableUpload, S3Raw};

/// 이 크기보다 큰 업로드는 멀티파트이므로 `upload_resumable`로
/// 이어올릴 수 있다. 16MB = part_size_for의 최소 파트 크기.
pub const RESUMABLE_UPLOAD_MIN: u64 = 16 * 1024 * 1024;

/// 병렬 다운로드 옵션. SPEC 4.4 기본값: 8MB 청크, 동시성 8.
#[derive(Debug, Clone, Copy)]
pub struct TransferOptions {
    pub chunk_size: u64,
    pub concurrency: usize,
}

impl Default for TransferOptions {
    fn default() -> Self {
        Self {
            chunk_size: 8 * 1024 * 1024,
            concurrency: 8,
        }
    }
}

/// 진행률 콜백. UniFFI의 PierProgress에 대응하는 코어 쪽 트레이트.
/// 청크가 완료될 때마다 누적 바이트로 호출된다(최소 청크 단위).
pub trait ProgressSink: Send + Sync {
    /// 지금까지 받은 바이트 transferred / 전체 크기 total.
    fn on_progress(&self, transferred: u64, total: u64);
    /// 폴더 작업의 파일 단위 진행: 완료 파일 수 done / 전체 파일 수 total.
    /// 단일 파일 전송이나 바이트만 다루는 구현은 무시해도 된다.
    fn on_files(&self, _done: u64, _total: u64) {}
}

/// 취소 토큰. 여러 태스크·스레드에서 `cancel()`을 호출할 수 있다.
#[derive(Clone, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// 취소를 요청한다. 이미 대기 중인 `cancelled()`Future를 깨운다.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// 취소될 때까지 대기한다. 이미 취소됐으면 즉시 반환한다.
    pub async fn cancelled(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            // waiter로 등록된 뒤에 플래그를 확인해야 notify_waiters를
            // 놓치지 않는다(notify_waiters는 permit을 저장하지 않는다).
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

/// key 객체를 dest 파일로 다운로드하고 쓴 바이트 수를 반환한다.
///
/// 성공 시에만 dest가 생긴다(임시 파일에 쓰고 rename). 실패·취소 시
/// dest는 건드리지 않고 임시 파일도 남기지 않는다.
pub async fn download<B>(
    backend: &B,
    key: &str,
    dest: &Path,
    opts: &TransferOptions,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<u64, Error>
where
    B: Backend + Clone + Send + Sync + 'static,
{
    let mut state = None;
    match download_resumable(backend, key, dest, opts, &mut state, progress, cancel).await {
        Ok(item) => Ok(item.size),
        // 이어받기 정보는 쓰지 않으므로 남은 임시 파일을 지운다.
        Err(err) => {
            discard_download(&mut state).await;
            Err(err)
        }
    }
}

/// 이어받기 가능한 단일 파일 다운로드의 메모리 상태.
///
/// `download_resumable`이 일시정지(취소 토큰)·실패로 중단될 때 채운다.
/// 임시 파일(`.<이름>.pierdownload`)은 지우지 않고 남는다 — 다시 호출해
/// 이어받거나 `discard_download`로 정리한다.
#[derive(Debug, Clone)]
pub struct ResumableDownload {
    /// 시작 시점에 stat한 원격 ETag. 불투명 문자열로만 취급한다.
    pub etag: Option<String>,
    /// 시작 시점의 객체 크기(바이트).
    pub size: u64,
    /// 완료된 청크의 시작 오프셋 집합.
    pub done_chunks: HashSet<u64>,
    /// 부분 데이터가 담긴 임시 파일 경로.
    pub tmp_path: PathBuf,
}

/// key 객체를 dest 파일로 다운로드하고 원격 stat Item을 반환한다.
///
/// `state`가 Some이고 원격 ETag·크기가 그대로면 완료된 청크를 건너뛰고
/// 남은 청크만 받는다. 원격이 바뀌었거나 임시 파일이 없으면 처음부터 받는다.
/// 일시정지·실패로 중단되면 진행 상태를 `state`에 채우고 Err를 반환한다
/// (임시 파일은 남는다). 완전히 버리려면 `discard_download`를 호출한다.
/// 성공하면 임시 파일을 dest로 옮기고 `state`를 비운다.
pub async fn download_resumable<B>(
    backend: &B,
    key: &str,
    dest: &Path,
    opts: &TransferOptions,
    state: &mut Option<ResumableDownload>,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<Item, Error>
where
    B: Backend + Clone + Send + Sync + 'static,
{
    if cancel.is_cancelled() {
        return Err(cancelled_error());
    }
    // 크기·ETag 확인을 겸해 없는 키는 여기서 NotFound가 난다.
    let item = backend.stat(key).await?;
    if item.is_dir {
        return Err(Error::Other(format!("{key}는 폴더라 다운로드할 수 없다")));
    }
    let size = item.size;
    let tmp = temp_path(dest);

    // 이전 상태를 쓸 수 있는지 확인한다. 원격이 바뀌었거나(ETag·크기)
    // 임시 파일이 유효하지 않으면 버리고 처음부터 받는다.
    let mut done_chunks = HashSet::new();
    if let Some(prev) = state.take() {
        let usable = prev.etag == item.etag
            && prev.size == size
            && prev.tmp_path == tmp
            && matches!(tokio::fs::metadata(&prev.tmp_path).await, Ok(m) if m.len() == size);
        if usable {
            done_chunks = prev.done_chunks;
        } else {
            let _ = tokio::fs::remove_file(&prev.tmp_path).await;
        }
    }

    let mut run = ResumableDownload {
        etag: item.etag.clone(),
        size,
        done_chunks,
        tmp_path: tmp.clone(),
    };
    match run_download(backend, key, &tmp, size, &mut run, opts, progress, cancel).await {
        Ok(()) => match tokio::fs::rename(&tmp, dest).await {
            Ok(()) => {
                *state = None;
                Ok(item)
            }
            // rename 실패도 이어받기 가능하다 — 모든 청크는 tmp에 있다.
            Err(err) => {
                *state = Some(run);
                Err(err.into())
            }
        },
        Err(err) => {
            *state = Some(run);
            Err(err)
        }
    }
}

/// 이어받기를 포기하고 남은 임시 파일을 지운다.
pub async fn discard_download(state: &mut Option<ResumableDownload>) {
    if let Some(prev) = state.take() {
        let _ = tokio::fs::remove_file(&prev.tmp_path).await;
    }
}

/// 임시 파일에 전체 바이트를 쓴다. run.done_chunks에 있는 오프셋의
/// 청크는 건너뛰고, 완료한 청크는 run에 누적해 호출자가 중단 지점을
/// 알 수 있게 한다.
#[allow(clippy::too_many_arguments)]
async fn run_download<B>(
    backend: &B,
    key: &str,
    tmp: &Path,
    size: u64,
    run: &mut ResumableDownload,
    opts: &TransferOptions,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<(), Error>
where
    B: Backend + Clone + Send + Sync + 'static,
{
    // 목적 크기로 미리 길이를 잡는다(희소 파일). 청크 태스크는 각자
    // 파일 핸들을 열어 오프셋에 직접 쓰므로 순서와 무관하게 완성된다.
    // 이어받는 임시 파일은 이미 그 크기이므로 새로 만들 때만 초기화한다.
    let needs_create = match tokio::fs::metadata(tmp).await {
        Ok(m) => m.len() != size,
        Err(_) => true,
    };
    if needs_create {
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(tmp)
            .await?;
        file.set_len(size).await?;
        drop(file);
        // 크기가 다른 임시 파일 위에서는 완료 표시를 신뢰할 수 없다.
        run.done_chunks.clear();
    }

    if size == 0 {
        progress.on_progress(0, 0);
        return Ok(());
    }

    // 이전에 받아 둔 청크를 초기 진행률로 합산한다.
    let mut transferred: u64 = run
        .done_chunks
        .iter()
        .map(|off| opts.chunk_size.max(1).min(size.saturating_sub(*off)))
        .sum();
    if transferred > 0 {
        progress.on_progress(transferred, size);
    }

    // 동시 요청 수는 세마포어로 제한한다. 청크 수만큼 태스크를 만들지만
    // 실제 네트워크 요청은 concurrency개까지다.
    let sem = Arc::new(Semaphore::new(opts.concurrency.max(1)));
    let mut set = JoinSet::new();
    for (offset, len) in chunk_ranges(size, opts.chunk_size) {
        if run.done_chunks.contains(&offset) {
            continue;
        }
        let backend = backend.clone();
        let key = key.to_string();
        let tmp = tmp.to_path_buf();
        let cancel = cancel.clone();
        let sem = Arc::clone(&sem);
        set.spawn(async move {
            let _permit = sem
                .acquire()
                .await
                .map_err(|e| Error::Other(format!("전송 세마포어 오류: {e}")))?;
            fetch_chunk(&backend, &key, &tmp, offset, len, &cancel)
                .await
                .map(|bytes| (offset, bytes))
        });
    }

    loop {
        let joined = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                set.abort_all();
                return Err(cancelled_error());
            }
            joined = set.join_next() => joined,
        };
        match joined {
            // 모든 청크 완료.
            None => break,
            Some(Ok(Ok((offset, done)))) => {
                run.done_chunks.insert(offset);
                transferred += done;
                progress.on_progress(transferred, size);
                if cancel.is_cancelled() {
                    set.abort_all();
                    return Err(cancelled_error());
                }
            }
            Some(Ok(Err(err))) => {
                set.abort_all();
                return Err(err);
            }
            Some(Err(err)) => {
                set.abort_all();
                return Err(Error::Other(format!("다운로드 작업 실패: {err}")));
            }
        }
    }
    Ok(())
}

/// size를 chunk_size 이하의 (offset, len) 범위 목록으로 나눈다.
fn chunk_ranges(size: u64, chunk_size: u64) -> Vec<(u64, u64)> {
    let chunk = chunk_size.max(1);
    let mut ranges = Vec::new();
    let mut offset = 0u64;
    while offset < size {
        let len = chunk.min(size - offset);
        ranges.push((offset, len));
        offset += len;
    }
    ranges
}

/// 폴더 다운로드 결과.
#[derive(Debug)]
pub struct FolderDownload {
    /// 실제로 만든 로컬 폴더 경로(dest_dir/<폴더 이름> 또는 " (N)"이 붙은 이름).
    pub local_path: PathBuf,
    /// 다운로드한 파일 수(폴더 마커·빈 폴더 제외).
    pub file_count: u64,
    /// 다운로드한 파일들의 총 바이트.
    pub total_bytes: u64,
}

/// prefix 폴더의 하위 객체를 모두 dest_dir 아래 `<폴더 이름>` 디렉터리에
/// 상대 경로 구조 그대로 다운로드한다.
///
/// - 하위 전체 목록을 받은 직후 progress(0, total_bytes)를 한 번 호출하고,
///   이후 완료 바이트 누적으로 호출한다.
/// - 파일들은 여러 개를 동시에 받되, 모든 청크 요청을 합쳐 총 동시 요청 수는
///   opts.concurrency를 넘지 않는다(파일 간 공유 세마포어).
/// - 실패·취소 시 진행 중 요청을 중단하고 미완료 임시 파일을 지운다.
///   이미 완료(rename된) 파일은 그대로 둔다.
pub async fn download_folder<B>(
    backend: &B,
    prefix: &str,
    dest_dir: &Path,
    opts: &TransferOptions,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<FolderDownload, Error>
where
    B: Backend + Clone + Send + Sync + 'static,
{
    let mut state = None;
    match download_folder_resumable(
        backend, prefix, dest_dir, opts, &mut state, progress, cancel,
    )
    .await
    {
        Ok(result) => Ok(result),
        // 이어받기 정보는 쓰지 않으므로 미완료 임시 파일을 지운다.
        Err(err) => {
            discard_folder_download(&mut state).await;
            Err(err)
        }
    }
}

/// 폴더 다운로드의 이어받기 상태.
///
/// `download_folder_resumable`이 일시정지·실패로 중단될 때 채운다.
/// 완료(rename)된 파일의 임시 파일은 없고, 진행 중 파일의 임시 파일은
/// 남는다 — 다시 호출해 이어받거나 `discard_folder_download`로 정리한다.
#[derive(Debug, Clone, Default)]
pub struct ResumableFolderDownload {
    /// 처음에 확정된 대상 디렉터리(` (N)` 접미사 포함). 재개 시 같은
    /// 디렉터리로 이어간다.
    pub target: PathBuf,
    /// 다운로드를 마친 파일의 S3 키.
    pub done: HashSet<String>,
    /// 진행 중이던 파일의 청크 상태(키 → 상태).
    pub in_progress: HashMap<String, ResumableDownload>,
}

/// `download_folder`와 같은 규칙으로 폴더를 받되 `state`로 이어받는다.
///
/// `state`가 Some이면 처음에 확정된 대상 디렉터리를 재사용하고, 완료된
/// 파일을 건너뛰며, 진행 중이던 파일은 남은 청크만 받는다. 파일의 원격
/// ETag·크기가 바뀌었거나 임시 파일이 없으면 그 파일만 처음부터 받는다.
/// 중단되면 `state`를 갱신하고 Err를 반환한다. 성공하면 `state`를 비운다.
pub async fn download_folder_resumable<B>(
    backend: &B,
    prefix: &str,
    dest_dir: &Path,
    opts: &TransferOptions,
    state: &mut Option<ResumableFolderDownload>,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<FolderDownload, Error>
where
    B: Backend + Clone + Send + Sync + 'static,
{
    if cancel.is_cancelled() {
        return Err(cancelled_error());
    }
    let prefix = key::normalize_prefix(prefix);
    // 루트("")는 폴더 이름을 만들 수 없어 거부한다.
    if prefix.is_empty() {
        return Err(Error::Other("루트 폴더는 다운로드할 수 없다".to_string()));
    }

    // 하위 전체 목록. 재귀 목록에는 CommonPrefixes가 없어 폴더 마커 객체가
    // 파일로 올라올 수 있으므로 끝 '/'도 폴더로 취급한다.
    let items: Vec<Item> = backend
        .list_recursive(&prefix)
        .await?
        .into_iter()
        .filter(|item| item.key != prefix && item.key.starts_with(&prefix))
        .collect();

    // 대상 디렉터리는 처음 확정한 것을 그대로 쓴다 — 재개 때 새 " (N)"
    // 경로를 뽑으면 이전에 받은 파일과 어긋난다.
    let prev = state.take().unwrap_or_default();
    let target = if prev.target.as_os_str().is_empty() {
        // 대상 폴더명은 마지막 키 구간을 안전한 로컬 이름으로 바꾼 값이다.
        let folder_name = safe_rel_path(key::name(&prefix))
            .to_string_lossy()
            .into_owned();
        unique_target_dir(dest_dir, &folder_name)?
    } else {
        prev.target
    };
    let plan = plan_layout(&target, &prefix, &items)?;

    // 이전 상태를 파일 인덱스로 옮긴다. 목록에서 사라졌거나 원격이
    // 바뀐 파일의 임시 파일은 버리고 처음부터 받는다.
    let index_by_key: HashMap<&str, usize> = plan
        .files
        .iter()
        .enumerate()
        .map(|(i, f)| (f.key.as_str(), i))
        .collect();
    let mut run = FolderDlState {
        done_chunks: vec![HashSet::new(); plan.files.len()],
        finished: vec![false; plan.files.len()],
    };
    for key in &prev.done {
        if let Some(&idx) = index_by_key.get(key.as_str()) {
            run.finished[idx] = true;
        }
    }
    for (key, rd) in prev.in_progress {
        let Some(&idx) = index_by_key.get(key.as_str()) else {
            let _ = tokio::fs::remove_file(&rd.tmp_path).await;
            continue;
        };
        let job = &plan.files[idx];
        let usable = rd.etag == job.etag
            && rd.size == job.size
            && rd.tmp_path == job.tmp
            && matches!(tokio::fs::metadata(&rd.tmp_path).await, Ok(m) if m.len() == job.size);
        if usable {
            run.done_chunks[idx] = rd.done_chunks;
        } else {
            let _ = tokio::fs::remove_file(&rd.tmp_path).await;
        }
    }

    let total_bytes: u64 = plan.files.iter().map(|f| f.size).sum();
    let file_count = plan.files.len() as u64;
    match run_folder_download(backend, &plan, &target, opts, progress, cancel, &mut run).await {
        Ok(()) => {
            *state = None;
            Ok(FolderDownload {
                local_path: target,
                file_count,
                total_bytes,
            })
        }
        Err(err) => {
            // 중단 지점을 상태로 남긴다 — 완료 파일 목록과 진행 중
            // 파일의 청크·임시 파일 정보를 모두 보존한다.
            let mut resume = ResumableFolderDownload {
                target,
                done: HashSet::new(),
                in_progress: HashMap::new(),
            };
            for (idx, job) in plan.files.iter().enumerate() {
                if run.finished[idx] {
                    resume.done.insert(job.key.clone());
                } else {
                    resume.in_progress.insert(
                        job.key.clone(),
                        ResumableDownload {
                            etag: job.etag.clone(),
                            size: job.size,
                            done_chunks: std::mem::take(&mut run.done_chunks[idx]),
                            tmp_path: job.tmp.clone(),
                        },
                    );
                }
            }
            *state = Some(resume);
            Err(err)
        }
    }
}

/// 폴더 다운로드 이어받기를 포기하고 진행 중 파일의 임시 파일을 지운다.
/// 완료(rename된) 파일과 대상 디렉터리는 그대로 둔다.
pub async fn discard_folder_download(state: &mut Option<ResumableFolderDownload>) {
    if let Some(prev) = state.take() {
        for rd in prev.in_progress.values() {
            let _ = tokio::fs::remove_file(&rd.tmp_path).await;
        }
    }
}

/// 폴더 다운로드의 로컬 경로 계획.
struct FolderPlan {
    /// 만들 디렉터리(절대 경로).
    dirs: Vec<PathBuf>,
    /// 다운로드할 파일 작업.
    files: Vec<FileJob>,
}

/// 파일 하나의 다운로드 작업.
struct FileJob {
    /// S3 키.
    key: String,
    size: u64,
    /// 원격 ETag — 이어받기 때 같은 객체인지 확인하는 데 쓴다.
    etag: Option<String>,
    /// 임시 파일(`.<이름>.pierdownload`).
    tmp: PathBuf,
    /// 최종 경로.
    dest: PathBuf,
}

/// 재귀 목록 items를 target 아래 안전한 로컬 경로로 배치한다.
/// 디렉터리(명시적 마커 + 파일의 조상)를 모으고, 파일·디렉터리 경로가
/// 충돌하면 파일 이름을 바꿔 피한다.
fn plan_layout(target: &Path, prefix: &str, items: &[Item]) -> Result<FolderPlan, Error> {
    let mut dir_set: HashSet<PathBuf> = HashSet::new();
    let mut file_rels: Vec<(&str, u64, Option<String>, PathBuf)> = Vec::new();

    for item in items {
        let rel_key = &item.key[prefix.len()..];
        if item.is_dir || item.key.ends_with('/') {
            // 폴더 마커의 끝 '/' 하나만 제거한다(`a//`는 빈 이름 폴더).
            let trimmed = rel_key.strip_suffix('/').unwrap_or(rel_key);
            let rel = safe_rel_path(trimmed);
            insert_ancestors(&mut dir_set, &rel);
            dir_set.insert(rel);
        } else {
            let rel = safe_rel_path(rel_key);
            insert_ancestors(&mut dir_set, &rel);
            file_rels.push((item.key.as_str(), item.size, item.etag.clone(), rel));
        }
    }

    // 디렉터리는 이미 점유된 경로로 보고 파일 목적지를 배정한다.
    let mut taken = dir_set.clone();
    let mut files = Vec::with_capacity(file_rels.len());
    for (key, size, etag, rel) in file_rels {
        let base = if dir_set.contains(&rel) {
            // 객체 `a`와 폴더 `a/`가 공존하면 파일이 폴더를 가리지 않게
            // 파일 이름 뒤에 ' (파일)'을 붙인다.
            rename_with_suffix(&rel, " (파일)")
        } else {
            rel.clone()
        };
        let mut dest_rel = base.clone();
        let mut n = 2;
        while taken.contains(&dest_rel) {
            dest_rel = rename_with_suffix(&base, &format!(" ({n})"));
            n += 1;
        }
        taken.insert(dest_rel.clone());

        let dest = target.join(&dest_rel);
        // 모든 구성 요소가 Normal이라 이탈은 없지만 방어적으로 확인한다.
        if !dest.starts_with(target) {
            return Err(Error::Other(format!("대상 폴더 밖 경로가 계산됐다: {key}")));
        }
        files.push(FileJob {
            key: key.to_string(),
            size,
            etag,
            tmp: temp_path(&dest),
            dest,
        });
    }

    let mut dirs: Vec<PathBuf> = dir_set.into_iter().map(|rel| target.join(rel)).collect();
    dirs.sort();
    Ok(FolderPlan { dirs, files })
}

/// S3 상대 키를 대상 폴더 기준 안전한 로컬 상대 경로로 바꾼다.
/// 경로 이탈을 막기 위해 '.', '..', 빈 구간(연속 슬래시·양 끝 슬래시)은
/// '_dot_', '_dotdot_', '_' 같은 안전한 이름으로 치환한다.
/// 반환 경로의 모든 구성 요소는 Normal이라 대상 폴더 밖으로 나가지 않는다.
fn safe_rel_path(rel_key: &str) -> PathBuf {
    let mut path = PathBuf::new();
    for seg in rel_key.split('/') {
        let safe = match seg {
            "" => "_",
            "." => "_dot_",
            ".." => "_dotdot_",
            s => s,
        };
        path.push(safe);
    }
    path
}

/// rel 경로의 모든 조상 디렉터리를 dir_set에 추가한다.
fn insert_ancestors(dir_set: &mut HashSet<PathBuf>, rel: &Path) {
    let mut cur = rel.parent();
    while let Some(p) = cur {
        if p.as_os_str().is_empty() {
            break;
        }
        dir_set.insert(p.to_path_buf());
        cur = p.parent();
    }
}

/// 상대 경로의 마지막 구성 요소(파일 이름) 뒤에 suffix를 붙인다.
fn rename_with_suffix(rel: &Path, suffix: &str) -> PathBuf {
    let name = rel
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    rel.with_file_name(format!("{name}{suffix}"))
}

/// dest_dir 아래에 `<이름>`, `<이름> (2)`, `(3)`… 순으로 없는 경로를 찾는다.
/// 존재 여부는 symlink_metadata로 확인해 깨진 심볼릭 링크도 점유로 본다.
fn unique_target_dir(dest_dir: &Path, name: &str) -> Result<PathBuf, Error> {
    let mut n = 1u32;
    loop {
        let candidate = if n == 1 {
            dest_dir.join(name)
        } else {
            dest_dir.join(format!("{name} ({n})"))
        };
        match std::fs::symlink_metadata(&candidate) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(candidate),
            Ok(_) => n += 1,
            Err(e) => return Err(e.into()),
        }
    }
}

/// target부터 path까지의 각 구성 요소가 심볼릭 링크가 아닌지 확인한다.
/// 대상 안에 미리 만들어 둔 심볼릭 링크를 따라가 버킷 밖에 쓰는 것을 막는다.
fn check_symlinks(target: &Path, path: &Path) -> Result<(), Error> {
    let rel = path
        .strip_prefix(target)
        .map_err(|_| Error::Other(format!("대상 폴더 밖 경로다: {}", path.display())))?;
    let mut cur = target.to_path_buf();
    for comp in rel.components() {
        cur.push(comp);
        match std::fs::symlink_metadata(&cur) {
            Ok(md) if md.file_type().is_symlink() => {
                return Err(Error::Other(format!(
                    "심볼릭 링크는 따라가지 않는다: {}",
                    cur.display()
                )));
            }
            // 아직 없는 구성 요소 아래는 더 확인할 것이 없다.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Ok(_) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// 폴더 다운로드의 파일별 진행 상태. 재개 입력이자 중단 시 출력이다.
struct FolderDlState {
    /// 파일 인덱스 → 받은 청크 오프셋.
    done_chunks: Vec<HashSet<u64>>,
    /// 파일 인덱스 → 완료(rename까지 끝남) 여부.
    finished: Vec<bool>,
}

/// 디렉터리 생성과 파일 다운로드를 실행한다.
/// 모든 파일의 청크를 하나의 JoinSet에 넣고 공유 세마포어로 총 동시 요청
/// 수를 opts.concurrency로 제한한다. 파일이 끝날 때마다 tmp를 dest로 rename한다.
/// `run`에 이전 실행의 완료 청크가 있으면 건너뛰고, 실행 중 완료한 청크를
/// 누적해 호출자가 중단 지점을 재구성할 수 있게 한다.
#[allow(clippy::too_many_arguments)]
async fn run_folder_download<B>(
    backend: &B,
    plan: &FolderPlan,
    target: &Path,
    opts: &TransferOptions,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
    run: &mut FolderDlState,
) -> Result<(), Error>
where
    B: Backend + Clone + Send + Sync + 'static,
{
    tokio::fs::create_dir_all(target).await?;
    for dir in &plan.dirs {
        check_symlinks(target, dir)?;
        tokio::fs::create_dir_all(dir).await?;
    }

    let files_total = plan.files.len() as u64;
    let mut files_done = run.finished.iter().filter(|f| **f).count() as u64;
    progress.on_files(files_done, files_total);

    // 0바이트 파일은 청크가 없으므로 임시 파일을 만들어 바로 완료시킨다.
    for (idx, job) in plan.files.iter().enumerate() {
        if job.size != 0 || run.finished[idx] {
            continue;
        }
        tokio::fs::write(&job.tmp, b"").await?;
        tokio::fs::rename(&job.tmp, &job.dest).await?;
        run.finished[idx] = true;
        files_done += 1;
        progress.on_files(files_done, files_total);
    }

    // 파일별 남은 바이트. 모두 받으면 tmp를 dest로 rename한다.
    let mut remaining: Vec<u64> = plan.files.iter().map(|f| f.size).collect();
    let sem = Arc::new(Semaphore::new(opts.concurrency.max(1)));
    let mut set = JoinSet::new();
    for (idx, job) in plan.files.iter().enumerate() {
        if run.finished[idx] {
            remaining[idx] = 0;
            continue;
        }
        if job.size == 0 {
            continue;
        }
        // 이어받는 임시 파일은 이미 목적 크기라 그대로 쓰고, 아니면
        // 목적 크기로 새로 만든다(희소 파일).
        let keep = !run.done_chunks[idx].is_empty()
            && matches!(tokio::fs::metadata(&job.tmp).await, Ok(m) if m.len() == job.size);
        if !keep {
            run.done_chunks[idx].clear();
            let file = tokio::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&job.tmp)
                .await?;
            file.set_len(job.size).await?;
            drop(file);
        }
        // 미리 받아 둔 청크는 remaining에서 뺀다.
        let pre: u64 = run.done_chunks[idx]
            .iter()
            .map(|off| opts.chunk_size.max(1).min(job.size.saturating_sub(*off)))
            .sum();
        remaining[idx] = job.size - pre;
        if remaining[idx] == 0 {
            // 모든 청크가 있던 파일은 바로 완료한다.
            tokio::fs::rename(&job.tmp, &job.dest).await?;
            run.finished[idx] = true;
            files_done += 1;
            progress.on_files(files_done, files_total);
            continue;
        }

        for (offset, len) in chunk_ranges(job.size, opts.chunk_size) {
            if run.done_chunks[idx].contains(&offset) {
                continue;
            }
            let backend = backend.clone();
            let key = job.key.clone();
            let tmp = job.tmp.clone();
            let cancel = cancel.clone();
            let sem = Arc::clone(&sem);
            set.spawn(async move {
                let _permit = sem
                    .acquire()
                    .await
                    .map_err(|e| Error::Other(format!("전송 세마포어 오류: {e}")))?;
                fetch_chunk(&backend, &key, &tmp, offset, len, &cancel)
                    .await
                    .map(|bytes| (idx, offset, bytes))
            });
        }
    }

    let total_bytes: u64 = plan.files.iter().map(|f| f.size).sum();
    // 완료 파일과 미리 받은 청크를 초기 진행률로 합산한다.
    let mut done: u64 = plan
        .files
        .iter()
        .enumerate()
        .map(|(idx, f)| f.size - remaining[idx])
        .sum();
    progress.on_progress(done, total_bytes);
    loop {
        let joined = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                set.abort_all();
                return Err(cancelled_error());
            }
            joined = set.join_next() => joined,
        };
        match joined {
            // 모든 청크 완료.
            None => break,
            Some(Ok(Ok((idx, offset, bytes)))) => {
                done += bytes;
                remaining[idx] -= bytes;
                run.done_chunks[idx].insert(offset);
                progress.on_progress(done, total_bytes);
                if remaining[idx] == 0 {
                    let job = &plan.files[idx];
                    if let Err(err) = tokio::fs::rename(&job.tmp, &job.dest).await {
                        set.abort_all();
                        return Err(err.into());
                    }
                    run.finished[idx] = true;
                    files_done += 1;
                    progress.on_files(files_done, files_total);
                }
                if cancel.is_cancelled() {
                    set.abort_all();
                    return Err(cancelled_error());
                }
            }
            Some(Ok(Err(err))) => {
                set.abort_all();
                return Err(err);
            }
            Some(Err(err)) => {
                set.abort_all();
                return Err(Error::Other(format!("다운로드 작업 실패: {err}")));
            }
        }
    }
    Ok(())
}

/// 청크 하나: 범위 읽기 → 취소 감시 → 임시 파일의 offset에 쓰기.
/// 완료 시 쓴 바이트 수(len)를 반환한다.
async fn fetch_chunk<B: Backend + Send + Sync>(
    backend: &B,
    key: &str,
    tmp: &Path,
    offset: u64,
    len: u64,
    cancel: &CancelToken,
) -> Result<u64, Error> {
    if cancel.is_cancelled() {
        return Err(cancelled_error());
    }
    // 읽기 도중 취소되면 select가 read_range future를 버리고 즉시 반환한다.
    let data = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(cancelled_error()),
        result = backend.read_range(key, offset, len) => result?,
    };
    if data.len() as u64 != len {
        return Err(Error::Other(format!(
            "크기 불일치: {key} @ {offset} — 기대 {len}B, 실제 {}B",
            data.len()
        )));
    }
    write_at(tmp, offset, &data).await?;
    Ok(len)
}

/// tmp 파일의 offset 위치에 data를 쓴다. 청크마다 별도 파일 핸들을 열어
/// 독립적인 오프셋으로 seek하므로 태스크 간 병렬 쓰기가 가능하다.
async fn write_at(path: &Path, offset: u64, data: &[u8]) -> Result<(), Error> {
    let mut file = tokio::fs::OpenOptions::new().write(true).open(path).await?;
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    file.write_all(data).await?;
    // write_all은 백그라운드 blocking 작업으로 넘기므로 flush를 await해
    // 쓰기 완료(와 오류 전파)를 확인한 뒤 핸들을 닫는다.
    file.flush().await?;
    Ok(())
}

/// dest와 같은 디렉터리의 임시 파일 경로(`.<이름>.pierdownload`).
fn temp_path(dest: &Path) -> PathBuf {
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "download".to_string());
    dest.with_file_name(format!(".{name}.pierdownload"))
}

fn cancelled_error() -> Error {
    Error::Cancelled("다운로드가 취소됐다".to_string())
}

// ===== 업로드 (SPEC 3.4, 4.4) =====

/// 업로드 충돌 모드 (SPEC 3.4).
#[derive(Debug, Clone)]
pub enum UploadMode {
    /// 대상이 이미 있으면 Error::Conflict.
    Fail,
    /// 덮어쓴다. `if_match`가 Some이면 그 ETag와 현재 ETag가 같을 때만
    /// 쓰고, 다르거나 없으면 Error::Conflict. None이면 무조건 덮어쓴다.
    Replace { if_match: Option<String> },
    /// 비어 있는 이름(`이름 (2).확장자`…)을 골라 함께 올린다.
    KeepBoth,
}

/// 폴더 업로드 결과.
#[derive(Debug)]
pub struct FolderUpload {
    /// 업로드한 파일 수(폴더 마커 제외).
    pub uploaded: u64,
    /// 제외 규칙·심볼릭 링크로 건너뛴 항목 수.
    pub ignored: u64,
    /// 충돌 모드 때문에 건너뛴 파일 수(Fail·KeepBoth의 Conflict).
    pub skipped: u64,
    /// 업로드 대상 파일들의 총 바이트.
    pub total_bytes: u64,
}

/// src 파일 하나를 parent 아래 name으로 업로드하고 원격 stat 결과를 반환한다.
///
/// - `mode`가 KeepBoth면 parent 목록에서 비어 있는 이름을 먼저 고른다.
/// - 성공 후 stat 결과를 Item으로 반환한다(stat 실패 시 에러 전파).
#[allow(clippy::too_many_arguments)] // FFI 계약상 시그니처가 고정돼 있다.
pub async fn upload<B>(
    backend: &B,
    src: &Path,
    parent: &str,
    name: &str,
    mode: &UploadMode,
    opts: &TransferOptions,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<Item, Error>
where
    B: Backend + Send + Sync,
{
    if cancel.is_cancelled() {
        return Err(cancelled_error());
    }
    if name.is_empty() || name.contains('/') {
        return Err(Error::Other(format!("업로드할 수 없는 이름이다: {name}")));
    }
    let meta = tokio::fs::metadata(src).await?;
    if !meta.is_file() {
        return Err(Error::Other(format!("{}는 파일이 아니다", src.display())));
    }
    let size = meta.len();

    let parent = key::normalize_prefix(parent);
    let resolved = resolve_name(backend, &parent, name, mode).await?;
    let remote_key = key::child_key(&parent, &resolved, false);

    let condition = mode_condition(mode);
    let popts = PutOptions {
        condition,
        part_size: part_size_for(size),
        concurrency: opts.concurrency,
    };
    backend
        .put_file(&remote_key, src, popts, progress, cancel)
        .await?;
    backend.stat(&remote_key).await
}

/// src_dir의 이름으로 parent 아래 폴더를 만들고 내용을 재귀 업로드한다.
/// 대상 폴더가 이미 있으면 그 안에 병합한다(파일 단위 충돌은 mode 적용).
///
/// - `ignore::is_ignored`에 걸리는 항목과 심볼릭 링크는 건너뛰고
///   ignored로 센다(링크는 따라가지 않는다).
/// - 내용이 전혀 없는 디렉터리는 폴더 마커 객체로 만든다.
/// - 총 동시 요청 수는 opts.concurrency를 넘지 않는다(파일별 파트 수만큼
///   세마포어를 잡는 가중 세마포어).
pub async fn upload_folder<B>(
    backend: &B,
    src_dir: &Path,
    parent: &str,
    mode: &UploadMode,
    opts: &TransferOptions,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<FolderUpload, Error>
where
    B: Backend + Clone + Send + Sync + 'static,
{
    let mut state = None;
    // 이어올리기 백엔드(raw)가 없으면 큰 파일도 put_file 멀티파트로
    // 처리하고, 취소·실패 시 그 안에서 abort한다 — 기존 동작과 같다.
    upload_folder_resumable(
        backend, None, src_dir, parent, mode, opts, &mut state, progress, cancel,
    )
    .await
}

/// 폴더 업로드의 이어올리기 상태.
///
/// `upload_folder_resumable`이 일시정지·실패로 중단될 때 채운다.
/// `in_progress`의 멀티파트 업로드는 abort되지 않고 남는다 — 다시 호출해
/// 이어올리거나 호출자가 `s3_raw::abort_upload`로 정리한다.
#[derive(Debug, Clone, Default)]
pub struct ResumableFolderUpload {
    /// 업로드를 마친 파일의 상대 경로(rel).
    pub done: HashSet<String>,
    /// 파일별로 확정된 원격 키(rel → 키). KeepBoth로 고른 이름이 재개 때
    /// 바뀌지 않게 계획 시점에 확정해 둔다.
    pub resolved: HashMap<String, String>,
    /// 진행 중이던 파일의 멀티파트 상태(rel → 상태).
    pub in_progress: HashMap<String, ResumableUpload>,
}

/// `upload_folder`와 같은 규칙으로 올리되 `state`로 이어올린다.
///
/// `state`가 Some이면 확정된 원격 키를 재사용하고 완료된 파일을 건너뛰며,
/// 진행 중이던 파일은 `upload_resumable`로 이어 올린다. `raw`가 None이면
/// 큰 파일도 put_file 멀티파트로 올린다(중단 시 그 파일은 처음부터).
/// 중단되면 `state`를 갱신하고 Err를 반환한다. 성공하면 `state`를 비운다.
#[allow(clippy::too_many_arguments)]
pub async fn upload_folder_resumable<B>(
    backend: &B,
    raw: Option<Arc<S3Raw>>,
    src_dir: &Path,
    parent: &str,
    mode: &UploadMode,
    opts: &TransferOptions,
    state: &mut Option<ResumableFolderUpload>,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<FolderUpload, Error>
where
    B: Backend + Clone + Send + Sync + 'static,
{
    if cancel.is_cancelled() {
        return Err(cancelled_error());
    }
    let meta = tokio::fs::metadata(src_dir).await?;
    if !meta.is_dir() {
        return Err(Error::Other(format!(
            "{}는 디렉터리가 아니다",
            src_dir.display()
        )));
    }
    let folder_name = src_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .ok_or_else(|| Error::Other(format!("폴더 이름을 알 수 없다: {}", src_dir.display())))?;

    let parent = key::normalize_prefix(parent);
    let target_prefix = key::child_key(&parent, &folder_name, true);

    // 로컬 트리 스캔: 파일 목록, 빈 디렉터리 마커 목록, 무시 수.
    let mut tree = LocalTree::default();
    let has_content = scan_dir(src_dir, "", &mut tree, cancel).await?;

    // 업로드 대상이 하나도 없으면 대상 폴더 마커만 만들고 끝낸다.
    if !has_content {
        if cancel.is_cancelled() {
            return Err(cancelled_error());
        }
        backend.create_dir(&target_prefix).await?;
        *state = None;
        return Ok(FolderUpload {
            uploaded: 0,
            ignored: tree.ignored,
            skipped: 0,
            total_bytes: 0,
        });
    }

    let mut resume = state.take().unwrap_or_default();
    // 완료로 표시된 파일이 실제로는 지워졌으면 done에서 뺀다.
    let on_disk: HashSet<&str> = tree.files.iter().map(|f| f.rel.as_str()).collect();
    resume.done.retain(|rel| on_disk.contains(rel.as_str()));

    // 파일별 최종 키를 확정한다. 이미 확정된 것은 재사용하고, 새로
    // 필요한 파일만 KeepBoth 이름을 배치로 고른다.
    let unresolved: Vec<LocalFile> = tree
        .files
        .iter()
        .filter(|f| !resume.resolved.contains_key(&f.rel))
        .cloned()
        .collect();
    if !unresolved.is_empty() {
        // 확정된 키의 파일명은 KeepBoth 해석에서 점유로 본다 — 진행 중
        // 멀티파트가 잡은 키를 다른 파일이 다시 고르는 일이 없게 한다.
        let mut reserved: HashMap<String, HashSet<String>> = HashMap::new();
        for remote_key in resume.resolved.values() {
            reserved
                .entry(key::parent(remote_key).to_string())
                .or_default()
                .insert(key::name(remote_key).to_string());
        }
        let mini = LocalTree {
            files: unresolved,
            markers: Vec::new(),
            ignored: 0,
        };
        for job in
            resolve_upload_jobs(backend, &mini, &target_prefix, mode, opts, &reserved).await?
        {
            resume.resolved.insert(job.rel.clone(), job.remote_key);
        }
    }

    // 내용이 없는 디렉터리의 폴더 마커를 만든다. 순차 실행이지만
    // 마커 수는 디렉터리 수에 불과해 동시성 제한 없이 처리한다.
    for rel in &tree.markers {
        if cancel.is_cancelled() {
            *state = Some(resume);
            return Err(cancelled_error());
        }
        backend
            .create_dir(&format!("{target_prefix}{rel}/"))
            .await?;
    }

    let total_bytes: u64 = tree.files.iter().map(|f| f.size).sum();
    // 남은 파일만 작업으로 만든다.
    let jobs: Vec<UploadJob> = tree
        .files
        .iter()
        .filter(|f| !resume.done.contains(&f.rel))
        .map(|f| UploadJob {
            src: f.path.clone(),
            rel: f.rel.clone(),
            remote_key: resume
                .resolved
                .get(&f.rel)
                .cloned()
                .unwrap_or_else(|| format!("{target_prefix}{}", f.rel)),
            size: f.size,
            weight: upload_weight(f.size, opts.concurrency),
            slot: Arc::new(AtomicU64::new(0)),
        })
        .collect();

    match run_upload_jobs(
        backend,
        raw,
        jobs,
        mode,
        *opts,
        tree.ignored,
        total_bytes,
        progress,
        cancel,
        &mut resume,
    )
    .await
    {
        Ok(result) => {
            *state = None;
            Ok(result)
        }
        Err(err) => {
            *state = Some(resume);
            Err(err)
        }
    }
}

/// parent 아래 names 중 이미 존재하는 항목만 반환한다(stat 기준).
/// 파일 키와 폴더 마커 키를 모두 확인하고, 없는 이름은 결과에서 빠진다.
pub async fn existing<B>(backend: &B, parent: &str, names: &[String]) -> Result<Vec<Item>, Error>
where
    B: Backend + Send + Sync,
{
    let parent = key::normalize_prefix(parent);
    let mut seen = HashSet::with_capacity(names.len());
    let mut found = Vec::new();
    for name in names {
        if name.is_empty() || name.contains('/') || !seen.insert(name.as_str()) {
            continue;
        }
        for is_dir in [false, true] {
            let k = key::child_key(&parent, name, is_dir);
            match backend.stat(&k).await {
                Ok(item) => {
                    found.push(item);
                    break;
                }
                Err(Error::NotFound(_)) => {}
                Err(e) => return Err(e),
            }
        }
    }
    Ok(found)
}

/// 멀티파트 파트 크기: max(16MB, ceil(size/10,000)을 1MB 단위 올림).
/// S3 멀티파트는 파트가 최대 10,000개·각 5GB 미만이어야 한다.
fn part_size_for(size: u64) -> u64 {
    const MIN_PART: u64 = 16 * 1024 * 1024;
    const MAX_PARTS: u64 = 10_000;
    const MIB: u64 = 1024 * 1024;
    let needed = size.div_ceil(MAX_PARTS);
    let rounded = needed.div_ceil(MIB) * MIB;
    MIN_PART.max(rounded)
}

/// UploadMode를 WriteCondition으로 변환한다.
pub(crate) fn mode_condition(mode: &UploadMode) -> WriteCondition {
    match mode {
        UploadMode::Fail | UploadMode::KeepBoth => WriteCondition::NotExists,
        UploadMode::Replace { if_match } => match if_match {
            Some(etag) => WriteCondition::IfMatch(etag.clone()),
            None => WriteCondition::Any,
        },
    }
}

/// KeepBoth일 때만 원격 목록으로 비어 있는 이름을 고른다.
/// 나머지 모드는 요청된 이름을 그대로 쓴다.
async fn resolve_name<B>(
    backend: &B,
    parent: &str,
    name: &str,
    mode: &UploadMode,
) -> Result<String, Error>
where
    B: Backend + Send + Sync,
{
    if !matches!(mode, UploadMode::KeepBoth) {
        return Ok(name.to_string());
    }
    let taken: HashSet<String> = backend
        .list_folder(parent)
        .await?
        .iter()
        .map(|i| i.name.clone())
        .collect();
    Ok(key::available_name(&taken, name))
}

/// 로컬 디렉터리 스캔 결과.
#[derive(Default)]
struct LocalTree {
    /// 업로드할 파일. rel은 src_dir 기준 '/' 구분 상대 경로.
    files: Vec<LocalFile>,
    /// 내용이 없는 디렉터리의 rel 경로('/' 없음, 마커 생성용).
    markers: Vec<String>,
    /// 제외 규칙·심볼릭 링크로 건너뛴 항목 수.
    ignored: u64,
}

#[derive(Clone)]
struct LocalFile {
    path: PathBuf,
    /// src_dir 기준 상대 경로('/' 구분, 디렉터리 포함 가능).
    rel: String,
    size: u64,
}

/// dir 아래를 재귀 스캔한다. 이 디렉터리의 내용 중 업로드 대상(파일,
/// 내용 있는 하위 디렉터리, 마커가 필요한 빈 디렉터리)이 하나라도
/// 있으면 true를 반환한다. rel_prefix는 현재 dir의 상대 경로 접두사
/// ('/'로 끝남, 루트는 "").
async fn scan_dir(
    dir: &Path,
    rel_prefix: &str,
    tree: &mut LocalTree,
    cancel: &CancelToken,
) -> Result<bool, Error> {
    let mut entries = Vec::new();
    let mut rd = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = rd.next_entry().await? {
        entries.push(entry);
    }
    entries.sort_by_key(|e| e.file_name());

    let mut produced = false;
    for entry in entries {
        if cancel.is_cancelled() {
            return Err(cancelled_error());
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if ignore::is_ignored(&name, &[]) {
            tree.ignored += 1;
            continue;
        }
        let file_type = entry.file_type().await?;
        let rel = format!("{rel_prefix}{name}");
        if file_type.is_symlink() || (!file_type.is_dir() && !file_type.is_file()) {
            // 심볼릭 링크는 따라가지 않고, 일반 파일·디렉터리가 아닌
            // 특수 파일도 업로드할 수 없어 건너뛴다.
            tree.ignored += 1;
            continue;
        }
        if file_type.is_dir() {
            // 재귀 async fn은 미래 크기가 무한대라 Box::pin으로 끊는다.
            let child_has =
                Box::pin(scan_dir(&entry.path(), &format!("{rel}/"), tree, cancel)).await?;
            if !child_has {
                // 내용이 없는 디렉터리는 폴더 마커로 보존한다.
                tree.markers.push(rel.clone());
            }
        } else {
            let size = entry.metadata().await?.len();
            tree.files.push(LocalFile {
                path: entry.path(),
                rel,
                size,
            });
        }
        produced = true;
    }
    Ok(produced)
}

/// 파일별 업로드 작업.
struct UploadJob {
    src: PathBuf,
    /// src_dir 기준 상대 경로('/' 구분). 이어올리기 상태의 키로 쓴다.
    rel: String,
    remote_key: String,
    size: u64,
    /// 이 파일이 잡는 세마포어 허가 수(파트 수 상한, 1~concurrency).
    weight: u32,
    /// put_file이 쓰는 진행 바이트. 부모 태스크가 합산해 보고한다.
    slot: Arc<AtomicU64>,
}

/// 파일 작업 목록을 만들고 KeepBoth 이름을 확정한다.
/// `reserved`는 KeepBoth 해석에서 이미 점유된 것으로 볼
/// (부모 접두사 → 이름 집합) — 재개 때 확정된 키와 충돌하지 않게 한다.
async fn resolve_upload_jobs<B>(
    backend: &B,
    tree: &LocalTree,
    target_prefix: &str,
    mode: &UploadMode,
    opts: &TransferOptions,
    reserved: &HashMap<String, HashSet<String>>,
) -> Result<Vec<UploadJob>, Error>
where
    B: Backend + Send + Sync,
{
    // 각 파일의 (원격 부모 접두사, 파일 이름, 기본 키).
    struct Pending {
        src: PathBuf,
        rel: String,
        parent_prefix: String,
        name: String,
        base_key: String,
        size: u64,
    }
    let mut pending = Vec::with_capacity(tree.files.len());
    for f in &tree.files {
        // rel의 마지막 '/' 기준으로 부모 접두사와 이름을 나눈다.
        let (dir_rel, name) = match f.rel.rfind('/') {
            Some(i) => (&f.rel[..=i], &f.rel[i + 1..]),
            None => ("", f.rel.as_str()),
        };
        let parent_prefix = format!("{target_prefix}{dir_rel}");
        pending.push(Pending {
            src: f.path.clone(),
            rel: f.rel.clone(),
            parent_prefix,
            name: name.to_string(),
            base_key: format!("{target_prefix}{}", f.rel),
            size: f.size,
        });
    }

    // KeepBoth: 원격 부모 접두사별 점유 이름 집합을 미리 읽는다.
    let mut taken_by_prefix: HashMap<String, HashSet<String>> = HashMap::new();
    if matches!(mode, UploadMode::KeepBoth) {
        let mut prefixes: Vec<String> = pending.iter().map(|p| p.parent_prefix.clone()).collect();
        prefixes.sort();
        prefixes.dedup();
        for prefix in prefixes {
            let mut taken: HashSet<String> = backend
                .list_folder(&prefix)
                .await?
                .iter()
                .map(|i| i.name.clone())
                .collect();
            if let Some(extra) = reserved.get(&prefix) {
                taken.extend(extra.iter().cloned());
            }
            taken_by_prefix.insert(prefix, taken);
        }
    }

    // 이름을 순서대로 확정한다. 확정한 이름은 taken에 넣어 같은 배치의
    // 다른 파일과도 충돌하지 않게 한다.
    let mut jobs = Vec::with_capacity(pending.len());
    for p in pending {
        let remote_key = if matches!(mode, UploadMode::KeepBoth) {
            let taken = taken_by_prefix
                .get_mut(&p.parent_prefix)
                .expect("KeepBoth 접두사 집합이 없다");
            let resolved = key::available_name(taken, &p.name);
            taken.insert(resolved.clone());
            format!("{}{resolved}", p.parent_prefix)
        } else {
            p.base_key
        };
        jobs.push(UploadJob {
            src: p.src,
            rel: p.rel,
            remote_key,
            size: p.size,
            weight: upload_weight(p.size, opts.concurrency),
            slot: Arc::new(AtomicU64::new(0)),
        });
    }
    Ok(jobs)
}

/// 파일 하나가 잡아야 할 세마포어 허가 수.
/// 단일 PUT은 1, 멀티파트는 min(파트 수, concurrency)다. 허가 수만큼
/// 파트 동시 업로드 수도 제한되므로 총 요청 수가 concurrency를 넘지 않는다.
fn upload_weight(size: u64, concurrency: usize) -> u32 {
    let part = part_size_for(size);
    if size <= part {
        return 1;
    }
    let parts = size.div_ceil(part);
    parts.min(concurrency.max(1) as u64).min(u32::MAX as u64) as u32
}

/// 파일별 진행률 슬롯. put_file이 파트 완료마다 누적 바이트를 쓰고
/// 부모 태스크가 주기적으로 합산해 ProgressSink에 보고한다.
#[derive(Default)]
struct SlotProgress {
    done: Arc<AtomicU64>,
}

impl ProgressSink for SlotProgress {
    fn on_progress(&self, transferred: u64, _total: u64) {
        self.done.store(transferred, Ordering::SeqCst);
    }
}

/// 파일 업로드 작업들을 공유 세마포어로 실행하고 진행률을 합산 보고한다.
/// Conflict는 mode에 따라 파일 단위로 건너뛴다(Fail·KeepBoth → skipped,
/// Replace는 전체 실패). 그 외 오류·취소는 남은 작업에 stop을 보내고
/// 각 태스크가 돌려주는 상태까지 다 거둔다 — 멀티파트 ID가 든 상태를
/// 버리면 서버에 고아 업로드가 남기 때문이다.
///
/// `resume`의 in_progress에 든 상태는 같은 rel의 큰 파일에 이어 쓰고,
/// 태스크가 돌려준 상태는 다시 `resume`에 넣는다(성공 파일은 제외).
#[allow(clippy::too_many_arguments)]
async fn run_upload_jobs<B>(
    backend: &B,
    raw: Option<Arc<S3Raw>>,
    jobs: Vec<UploadJob>,
    mode: &UploadMode,
    opts: TransferOptions,
    ignored: u64,
    total_bytes: u64,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
    resume: &mut ResumableFolderUpload,
) -> Result<FolderUpload, Error>
where
    B: Backend + Clone + Send + Sync + 'static,
{
    let skip_on_conflict = matches!(mode, UploadMode::Fail | UploadMode::KeepBoth);
    let condition = mode_condition(mode);
    let sem = Arc::new(Semaphore::new(opts.concurrency.max(1)));
    let uploaded = Arc::new(AtomicU64::new(0));
    let skipped = Arc::new(AtomicU64::new(0));
    // 이미 완료된 파일 바이트가 진행률의 시작점이다.
    let baseline_bytes: u64 = total_bytes.saturating_sub(jobs.iter().map(|j| j.size).sum::<u64>());
    // 진행률 합산용으로 슬롯 Arc를 spawn 전에 모아 둔다.
    let slots: Vec<Arc<AtomicU64>> = jobs.iter().map(|j| Arc::clone(&j.slot)).collect();
    let files_total = (resume.done.len() + jobs.len()) as u64;
    let files_done = Arc::new(AtomicU64::new(resume.done.len() as u64));
    progress.on_files(files_done.load(Ordering::SeqCst), files_total);
    // stop은 호출자 취소와 첫 실패를 태스크에 전달하는 내부 토큰이다.
    let stop = CancelToken::new();
    let mut set = JoinSet::new();
    // 이전 실행의 멀티파트 상태를 rel로 태스크에 배정한다.
    let mut part_states = std::mem::take(&mut resume.in_progress);

    for job in jobs {
        let backend = backend.clone();
        let stop = stop.clone();
        let sem = Arc::clone(&sem);
        let condition = condition.clone();
        let raw = raw.clone();
        let part_state = part_states.remove(&job.rel);
        set.spawn(async move {
            // 이 파일의 예상 파트 수만큼 허가를 잡는다. 허가 수 =
            // min(파트 수, concurrency)라 총 동시 요청이 상한을 넘지 않는다.
            // 허가 대기도 취소에 응답하게 한다.
            let permit = tokio::select! {
                biased;
                _ = stop.cancelled() => return (job, part_state, Err(cancelled_error())),
                p = sem.acquire_many(job.weight) => {
                    p.map_err(|e| Error::Other(format!("전송 세마포어 오류: {e}")))
                }
            };
            let _permit = match permit {
                Ok(p) => p,
                Err(e) => return (job, part_state, Err(e)),
            };
            let sink = SlotProgress {
                done: Arc::clone(&job.slot),
            };
            let mut part_state = part_state;
            // 큰 파일 + raw 백엔드가 있을 때만 이어올리기 경로를 쓴다.
            let result = match (raw, job.size > RESUMABLE_UPLOAD_MIN) {
                (Some(raw), true) => {
                    s3_raw::upload_resumable(
                        &raw,
                        &job.src,
                        &job.remote_key,
                        condition.clone(),
                        &opts,
                        &mut part_state,
                        &sink,
                        &stop,
                    )
                    .await
                }
                _ => {
                    let popts = PutOptions {
                        condition: condition.clone(),
                        part_size: part_size_for(job.size),
                        concurrency: job.weight as usize,
                    };
                    backend
                        .put_file(&job.remote_key, &job.src, popts, &sink, &stop)
                        .await
                }
            };
            (job, part_state, result)
        });
    }

    // 진행률은 슬롯 합산을 100ms마다 보고한다.
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
    let mut first_err: Option<Error> = None;
    let mut stop_sent = false;
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled(), if !stop_sent => {
                stop.cancel();
                stop_sent = true;
            }
            _ = tick.tick() => {
                let done: u64 = slots.iter().map(|s| s.load(Ordering::SeqCst)).sum::<u64>()
                    + baseline_bytes;
                progress.on_progress(done, total_bytes);
            }
            joined = set.join_next() => {
                let (job, part_state, result) = match joined {
                    None => break,
                    Some(Ok(out)) => out,
                    Some(Err(err)) => {
                        if first_err.is_none() {
                            first_err = Some(Error::Other(format!("업로드 작업 실패: {err}")));
                        }
                        stop.cancel();
                        stop_sent = true;
                        continue;
                    }
                };
                match result {
                    Ok(()) => {
                        resume.done.insert(job.rel.clone());
                        uploaded.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(Error::Conflict(_)) if skip_on_conflict => {
                        // 이미 있는 객체는 완료로 간주해 재개 때 재시도하지 않는다.
                        resume.done.insert(job.rel.clone());
                        skipped.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(e) => {
                        if let Some(state) = part_state {
                            resume.in_progress.insert(job.rel.clone(), state);
                        }
                        if first_err.is_none() {
                            first_err = Some(e);
                        }
                        stop.cancel();
                        stop_sent = true;
                        continue;
                    }
                }
                let now = files_done.fetch_add(1, Ordering::SeqCst) + 1;
                progress.on_files(now, files_total);
            }
        }
    }
    let done: u64 = slots.iter().map(|s| s.load(Ordering::SeqCst)).sum::<u64>() + baseline_bytes;
    progress.on_progress(done, total_bytes);
    if let Some(err) = first_err {
        return Err(err);
    }
    if cancel.is_cancelled() {
        return Err(cancelled_error());
    }
    Ok(FolderUpload {
        uploaded: uploaded.load(Ordering::SeqCst),
        ignored,
        skipped: skipped.load(Ordering::SeqCst),
        total_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;

    use crate::item::Item;

    /// 메모리 상의 데이터를 내주는 가짜 백엔드.
    /// read_range 호출 횟수와 실패 주입을 기록한다.
    #[derive(Clone)]
    struct FakeBackend {
        data: Arc<Vec<u8>>,
        calls: Arc<AtomicUsize>,
        fail: Arc<AtomicBool>,
    }

    impl FakeBackend {
        fn new(size: usize) -> Self {
            // 결정적 패턴: i % 256
            let data = (0..size).map(|i| (i % 256) as u8).collect();
            Self {
                data: Arc::new(data),
                calls: Arc::new(AtomicUsize::new(0)),
                fail: Arc::new(AtomicBool::new(false)),
            }
        }

        fn read_calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl Backend for FakeBackend {
        async fn list_folder(&self, _prefix: &str) -> Result<Vec<Item>, Error> {
            Ok(vec![])
        }

        async fn list_recursive(&self, _prefix: &str) -> Result<Vec<Item>, Error> {
            Ok(vec![])
        }

        async fn stat(&self, key: &str) -> Result<Item, Error> {
            if key == "obj" {
                Ok(Item::file(key, self.data.len() as u64, None, None))
            } else {
                Err(Error::NotFound(key.to_string()))
            }
        }

        async fn read_range(&self, key: &str, offset: u64, len: u64) -> Result<Vec<u8>, Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                return Err(Error::Other("read failed".to_string()));
            }
            let start = offset as usize;
            let end = start + len as usize;
            if key != "obj" || end > self.data.len() {
                return Err(Error::NotFound(key.to_string()));
            }
            Ok(self.data[start..end].to_vec())
        }
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

    /// 테스트마다 고유한 임시 디렉터리. Drop에서 지운다.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "pier-xfer-{}-{}-{tag}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn dest(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }

        /// 임시 다운로드 파일(`.name.pierdownload`)이 남아 있으면 true.
        fn has_temp_file(&self) -> bool {
            std::fs::read_dir(&self.0)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .any(|e| e.file_name().to_string_lossy().contains("pierdownload"))
                })
                .unwrap_or(false)
        }

        /// 폴더 다운로드는 임시 파일이 하위 디렉터리에 생기므로 재귀로 검사한다.
        fn has_temp_file_deep(&self) -> bool {
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
                        return true;
                    }
                }
            }
            false
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn opts(chunk_size: u64, concurrency: usize) -> TransferOptions {
        TransferOptions {
            chunk_size,
            concurrency,
        }
    }

    async fn run(
        backend: &FakeBackend,
        dest: &Path,
        options: &TransferOptions,
        progress: &dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<u64, Error> {
        download(backend, "obj", dest, options, progress, cancel).await
    }

    #[tokio::test]
    async fn empty_file() {
        let backend = FakeBackend::new(0);
        let dir = TempDir::new("empty");
        let dest = dir.dest("empty.bin");
        let progress = RecProgress::default();
        let written = run(&backend, &dest, &opts(8, 2), &progress, &CancelToken::new())
            .await
            .unwrap();
        assert_eq!(written, 0);
        assert_eq!(std::fs::metadata(&dest).unwrap().len(), 0);
        assert_eq!(backend.read_calls(), 0, "0바이트는 읽기 요청이 없다");
        assert_eq!(progress.last(), Some((0, 0)));
        assert!(!dir.has_temp_file());
    }

    #[tokio::test]
    async fn exact_chunk_size_is_single_request() {
        let backend = FakeBackend::new(100);
        let dir = TempDir::new("exact");
        let dest = dir.dest("exact.bin");
        let progress = RecProgress::default();
        let written = run(
            &backend,
            &dest,
            &opts(100, 4),
            &progress,
            &CancelToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(written, 100);
        assert_eq!(backend.read_calls(), 1);
        assert_eq!(std::fs::read(&dest).unwrap(), backend.data.as_slice());
        assert_eq!(progress.last(), Some((100, 100)));
    }

    #[tokio::test]
    async fn chunk_size_plus_one_is_two_requests() {
        let backend = FakeBackend::new(101);
        let dir = TempDir::new("plus1");
        let dest = dir.dest("plus1.bin");
        let written = run(
            &backend,
            &dest,
            &opts(100, 4),
            &RecProgress::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(written, 101);
        assert_eq!(backend.read_calls(), 2);
        assert_eq!(std::fs::read(&dest).unwrap(), backend.data.as_slice());
    }

    #[tokio::test]
    async fn multi_chunk_parallel_write() {
        // 25B를 10B 청크로 → 10,10,5 세 청크. 동시성 2.
        let backend = FakeBackend::new(25);
        let dir = TempDir::new("multi");
        let dest = dir.dest("multi.bin");
        let progress = RecProgress::default();
        let written = run(
            &backend,
            &dest,
            &opts(10, 2),
            &progress,
            &CancelToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(written, 25);
        assert_eq!(backend.read_calls(), 3);
        assert_eq!(std::fs::read(&dest).unwrap(), backend.data.as_slice());
        // 진행률은 단조 증가하고 마지막 값이 전체 크기다.
        let calls = progress.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert!(calls.windows(2).all(|w| w[0].0 <= w[1].0));
        assert_eq!(calls.last(), Some(&(25, 25)));
    }

    #[tokio::test]
    async fn cancel_before_start() {
        let backend = FakeBackend::new(1000);
        let dir = TempDir::new("precancel");
        let dest = dir.dest("pre.bin");
        let cancel = CancelToken::new();
        cancel.cancel();
        let err = run(
            &backend,
            &dest,
            &opts(10, 2),
            &RecProgress::default(),
            &cancel,
        )
        .await
        .expect_err("Cancelled여야 한다");
        assert!(matches!(err, Error::Cancelled(_)), "{err:?}");
        assert!(!dest.exists());
        assert!(!dir.has_temp_file());
        assert_eq!(backend.read_calls(), 0);
    }

    #[tokio::test]
    async fn cancel_mid_download() {
        // 100B를 10B 청크·동시성 2로 → 첫 진행률에서 취소하면 나머지가 남는다.
        let backend = FakeBackend::new(100);
        let dir = TempDir::new("midcancel");
        let dest = dir.dest("mid.bin");
        let cancel = CancelToken::new();
        let progress = CancelOnProgress {
            token: cancel.clone(),
        };
        let err = run(&backend, &dest, &opts(10, 2), &progress, &cancel)
            .await
            .expect_err("Cancelled여야 한다");
        assert!(matches!(err, Error::Cancelled(_)), "{err:?}");
        assert!(!dest.exists(), "취소된 다운로드가 dest를 만들었다");
        assert!(!dir.has_temp_file(), "취소 후 임시 파일이 남았다");
    }

    #[tokio::test]
    async fn failing_chunk_cleans_up() {
        let backend = FakeBackend::new(100);
        backend.fail.store(true, Ordering::SeqCst);
        let dir = TempDir::new("fail");
        let dest = dir.dest("fail.bin");
        let err = run(
            &backend,
            &dest,
            &opts(10, 4),
            &RecProgress::default(),
            &CancelToken::new(),
        )
        .await
        .expect_err("실패해야 한다");
        assert!(matches!(err, Error::Other(_)), "{err:?}");
        assert!(!dest.exists());
        assert!(!dir.has_temp_file());
    }

    #[tokio::test]
    async fn replaces_existing_dest() {
        let backend = FakeBackend::new(50);
        let dir = TempDir::new("replace");
        let dest = dir.dest("replace.bin");
        std::fs::write(&dest, b"old content that is longer").unwrap();
        let written = run(
            &backend,
            &dest,
            &opts(10, 2),
            &RecProgress::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(written, 50);
        assert_eq!(std::fs::read(&dest).unwrap(), backend.data.as_slice());
    }

    #[tokio::test]
    async fn cancelled_token_wakes_waiter() {
        let token = CancelToken::new();
        let waiter = token.clone();
        let handle = tokio::spawn(async move { waiter.cancelled().await });
        // 아직 취소 전에는 끝나지 않아야 한다.
        tokio::task::yield_now().await;
        assert!(!handle.is_finished());
        token.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("cancelled()가 깨어나지 않았다")
            .unwrap();
    }

    /// 폴더 다운로드 테스트용 가짜 백엔드.
    /// items를 재귀 목록으로 주고 files의 데이터를 범위 읽기로 돌려준다.
    /// read_range 동시 진입 수를 세어 공유 세마포어 상한을 검증한다.
    #[derive(Clone)]
    struct FakeFolderBackend {
        items: Arc<Vec<Item>>,
        files: Arc<HashMap<String, Vec<u8>>>,
        in_flight: Arc<AtomicUsize>,
        max_in_flight: Arc<AtomicUsize>,
    }

    impl FakeFolderBackend {
        /// files: (키, 내용), dirs: 끝 '/'인 폴더 키.
        fn new(files: &[(&str, &[u8])], dirs: &[&str]) -> Self {
            let mut items = Vec::new();
            let mut map = HashMap::new();
            for (key, data) in files {
                items.push(Item::file(*key, data.len() as u64, None, None));
                map.insert((*key).to_string(), data.to_vec());
            }
            for key in dirs {
                items.push(Item::folder(*key));
            }
            items.sort_by(|a, b| a.key.cmp(&b.key));
            Self {
                items: Arc::new(items),
                files: Arc::new(map),
                in_flight: Arc::new(AtomicUsize::new(0)),
                max_in_flight: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn max_concurrent(&self) -> usize {
            self.max_in_flight.load(Ordering::SeqCst)
        }
    }

    impl Backend for FakeFolderBackend {
        async fn list_folder(&self, _prefix: &str) -> Result<Vec<Item>, Error> {
            Ok(vec![])
        }

        async fn list_recursive(&self, prefix: &str) -> Result<Vec<Item>, Error> {
            Ok(self
                .items
                .iter()
                .filter(|i| i.key.starts_with(prefix))
                .cloned()
                .collect())
        }

        async fn stat(&self, key: &str) -> Result<Item, Error> {
            Err(Error::NotFound(key.to_string()))
        }

        async fn read_range(&self, key: &str, offset: u64, len: u64) -> Result<Vec<u8>, Error> {
            let cur = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(cur, Ordering::SeqCst);
            // 짧게 기다려 여러 청크의 동시 진입이 겹치게 만든다.
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            let result = (|| {
                let data = self
                    .files
                    .get(key)
                    .ok_or_else(|| Error::NotFound(key.to_string()))?;
                let start = offset as usize;
                let end = start + len as usize;
                if end > data.len() {
                    return Err(Error::Other(format!("범위 초과: {key}")));
                }
                Ok(data[start..end].to_vec())
            })();
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            result
        }
    }

    /// (키, 내용) 쌍을 (&str, &[u8]) 슬라이스로 빌려주는 테스트 헬퍼.
    fn folder_files(files: &[(String, Vec<u8>)]) -> Vec<(&str, &[u8])> {
        files
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_slice()))
            .collect()
    }

    #[test]
    fn safe_rel_path_neutralizes_escapes() {
        assert_eq!(
            safe_rel_path("a/../../etc/passwd"),
            PathBuf::from("a/_dotdot_/_dotdot_/etc/passwd")
        );
        assert_eq!(safe_rel_path("./x"), PathBuf::from("_dot_/x"));
        assert_eq!(safe_rel_path("a//b"), PathBuf::from("a/_/b"));
        assert_eq!(safe_rel_path(""), PathBuf::from("_"));
        // 한글·공백·아주 긴 이름은 그대로 둔다.
        assert_eq!(
            safe_rel_path("사진/한글 파일.txt"),
            PathBuf::from("사진/한글 파일.txt")
        );
        let long = "가".repeat(300);
        assert_eq!(safe_rel_path(&long), PathBuf::from(long));
    }

    #[tokio::test]
    async fn folder_download_nested_structure_and_markers() {
        let backend = FakeFolderBackend::new(
            &[
                ("p/a.txt", b"aaa".as_slice()),
                ("p/d/b.txt", b"bb".as_slice()),
            ],
            &["p/empty/"],
        );
        let dir = TempDir::new("folder");
        let progress = RecProgress::default();
        let result = download_folder(
            &backend,
            "p/",
            &dir.0,
            &opts(8, 4),
            &progress,
            &CancelToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(result.local_path, dir.0.join("p"));
        assert_eq!(result.file_count, 2);
        assert_eq!(result.total_bytes, 5);
        assert_eq!(std::fs::read(dir.0.join("p/a.txt")).unwrap(), b"aaa");
        assert_eq!(std::fs::read(dir.0.join("p/d/b.txt")).unwrap(), b"bb");
        // 폴더 마커는 빈 디렉터리로 만들어진다.
        let empty = dir.0.join("p/empty");
        assert!(empty.is_dir());
        assert_eq!(std::fs::read_dir(&empty).unwrap().count(), 0);
        // 진행률: 첫 값 (0,total), 마지막 값 total.
        let calls = progress.calls.lock().unwrap();
        assert_eq!(calls.first(), Some(&(0, 5)));
        assert_eq!(calls.last(), Some(&(5, 5)));
        drop(calls);
        assert!(!dir.has_temp_file_deep());
    }

    #[tokio::test]
    async fn folder_download_respects_shared_concurrency() {
        // 파일 6개 × 100B를 10B 청크로 → 파일당 10청크, 총 60청크.
        let files: Vec<(String, Vec<u8>)> = (0..6)
            .map(|i| (format!("c/f{i}.bin"), vec![i as u8; 100]))
            .collect();
        let backend = FakeFolderBackend::new(&folder_files(&files), &[]);
        let dir = TempDir::new("conc");
        let result = download_folder(
            &backend,
            "c/",
            &dir.0,
            &opts(10, 2),
            &RecProgress::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.file_count, 6);
        assert_eq!(result.total_bytes, 600);
        // 파일을 넘어 합산해도 총 동시 read_range가 concurrency(2)를 넘지 않는다.
        assert_eq!(backend.max_concurrent(), 2);
    }

    #[tokio::test]
    async fn folder_download_cancel_cleans_up() {
        let files: Vec<(String, Vec<u8>)> = (0..8)
            .map(|i| (format!("x/f{i}.bin"), vec![7u8; 500]))
            .collect();
        let backend = FakeFolderBackend::new(&folder_files(&files), &[]);
        let dir = TempDir::new("fcancel");
        let cancel = CancelToken::new();
        let progress = CancelOnProgress {
            token: cancel.clone(),
        };
        let err = download_folder(&backend, "x/", &dir.0, &opts(10, 4), &progress, &cancel)
            .await
            .expect_err("Cancelled여야 한다");
        assert!(matches!(err, Error::Cancelled(_)), "{err:?}");
        assert!(!dir.has_temp_file_deep(), "취소 후 임시 파일이 남았다");
    }

    #[tokio::test]
    async fn folder_download_uses_numbered_name_on_conflict() {
        let backend = FakeFolderBackend::new(&[("p/a.txt", b"x".as_slice())], &[]);
        let dir = TempDir::new("fconflict");
        // p를 미리 만들어 두면 "p (2)", 그것도 있으면 "p (3)"을 쓴다.
        std::fs::create_dir_all(dir.0.join("p")).unwrap();
        let r1 = download_folder(
            &backend,
            "p/",
            &dir.0,
            &opts(8, 2),
            &RecProgress::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(r1.local_path, dir.0.join("p (2)"));
        assert!(dir.0.join("p (2)/a.txt").is_file());

        let r2 = download_folder(
            &backend,
            "p/",
            &dir.0,
            &opts(8, 2),
            &RecProgress::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(r2.local_path, dir.0.join("p (3)"));
        assert!(dir.0.join("p (3)/a.txt").is_file());
    }

    #[tokio::test]
    async fn folder_download_file_dir_name_collision() {
        // 객체 p/a(파일)와 폴더 p/a/가 공존 → 파일은 'a (파일)'로 저장된다.
        let backend = FakeFolderBackend::new(
            &[
                ("p/a", b"file".as_slice()),
                ("p/a/inner.txt", b"in".as_slice()),
            ],
            &["p/a/"],
        );
        let dir = TempDir::new("fcollide");
        let result = download_folder(
            &backend,
            "p/",
            &dir.0,
            &opts(8, 2),
            &RecProgress::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.file_count, 2);
        assert_eq!(std::fs::read(dir.0.join("p/a (파일)")).unwrap(), b"file");
        assert!(dir.0.join("p/a").is_dir());
        assert_eq!(std::fs::read(dir.0.join("p/a/inner.txt")).unwrap(), b"in");
    }

    #[tokio::test]
    async fn folder_download_sanitizes_hostile_keys() {
        // '../'·'//'가 섞인 키도 대상 폴더 안에 안전한 이름으로 저장된다.
        let backend = FakeFolderBackend::new(
            &[
                ("p/../escape.txt", b"e".as_slice()),
                ("p/a//b.txt", b"b".as_slice()),
            ],
            &[],
        );
        let dir = TempDir::new("fsanitize");
        download_folder(
            &backend,
            "p/",
            &dir.0,
            &opts(8, 2),
            &RecProgress::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read(dir.0.join("p/_dotdot_/escape.txt")).unwrap(),
            b"e"
        );
        assert_eq!(std::fs::read(dir.0.join("p/a/_/b.txt")).unwrap(), b"b");
        // 대상 폴더 밖으로 빠져나간 파일은 없다.
        assert!(!dir.0.join("escape.txt").exists());
        assert!(!dir.0.join("p/../escape.txt").exists());
    }

    #[tokio::test]
    async fn folder_download_rejects_root() {
        let backend = FakeFolderBackend::new(&[], &[]);
        let dir = TempDir::new("froot");
        for prefix in ["", "/"] {
            let err = download_folder(
                &backend,
                prefix,
                &dir.0,
                &opts(8, 2),
                &RecProgress::default(),
                &CancelToken::new(),
            )
            .await
            .expect_err("루트는 거부돼야 한다");
            assert!(matches!(err, Error::Other(_)), "{err:?}");
        }
    }

    // ===== 업로드 단위 테스트 =====

    /// 업로드 테스트용 가짜 백엔드.
    /// put_file이 WriteCondition을 실제로 검사해 Conflict를 재현하고,
    /// 성공하면 objects에 Item을 기록한다. create_dir은 dirs에 기록한다.
    #[derive(Clone, Default)]
    struct FakeUploadBackend {
        objects: Arc<Mutex<HashMap<String, Item>>>,
        /// (키, 바이트) — put_file 호출 기록.
        puts: Arc<Mutex<Vec<(String, u64)>>>,
        /// create_dir 호출 기록.
        dirs: Arc<Mutex<Vec<String>>>,
        /// list_folder이 돌려줄 추가 항목.
        list_items: Arc<Mutex<Vec<Item>>>,
    }

    impl FakeUploadBackend {
        fn seed(&self, key: &str, size: u64, etag: Option<&str>) {
            self.objects.lock().unwrap().insert(
                key.to_string(),
                Item::file(key, size, etag.map(str::to_string), None),
            );
        }

        fn put_keys(&self) -> Vec<String> {
            self.puts
                .lock()
                .unwrap()
                .iter()
                .map(|(k, _)| k.clone())
                .collect()
        }
    }

    impl Backend for FakeUploadBackend {
        async fn list_folder(&self, _prefix: &str) -> Result<Vec<Item>, Error> {
            let mut items: Vec<Item> = self
                .objects
                .lock()
                .unwrap()
                .values()
                .cloned()
                .chain(self.list_items.lock().unwrap().iter().cloned())
                .collect();
            items.sort_by(|a, b| a.key.cmp(&b.key));
            Ok(items)
        }

        async fn list_recursive(&self, _prefix: &str) -> Result<Vec<Item>, Error> {
            Ok(vec![])
        }

        async fn stat(&self, key: &str) -> Result<Item, Error> {
            self.objects
                .lock()
                .unwrap()
                .get(key)
                .cloned()
                .ok_or_else(|| Error::NotFound(key.to_string()))
        }

        async fn create_dir(&self, key: &str) -> Result<(), Error> {
            self.dirs.lock().unwrap().push(key.to_string());
            Ok(())
        }

        async fn put_file(
            &self,
            key: &str,
            src: &Path,
            opts: PutOptions,
            _progress: &dyn ProgressSink,
            _cancel: &CancelToken,
        ) -> Result<(), Error> {
            let size = tokio::fs::metadata(src).await?.len();
            {
                let objects = self.objects.lock().unwrap();
                match &opts.condition {
                    WriteCondition::Any => {}
                    WriteCondition::NotExists => {
                        if objects.contains_key(key) {
                            return Err(Error::Conflict(format!("{key}가 이미 있다")));
                        }
                    }
                    WriteCondition::IfMatch(etag) => {
                        let ok =
                            objects.get(key).and_then(|i| i.etag.as_deref()) == Some(etag.as_str());
                        if !ok {
                            return Err(Error::Conflict(format!("{key}의 ETag가 다르다")));
                        }
                    }
                }
            }
            self.puts.lock().unwrap().push((key.to_string(), size));
            self.objects.lock().unwrap().insert(
                key.to_string(),
                Item::file(key, size, Some("etag".to_string()), None),
            );
            Ok(())
        }
    }

    fn write_src(dir: &TempDir, name: &str, data: &[u8]) -> PathBuf {
        let path = dir.dest(name);
        std::fs::write(&path, data).unwrap();
        path
    }

    #[test]
    fn part_size_for_cases() {
        const MIB: u64 = 1024 * 1024;
        const MIN_PART: u64 = 16 * MIB;
        assert_eq!(part_size_for(0), MIN_PART);
        assert_eq!(part_size_for(MIN_PART), MIN_PART);
        // 16MB+1 → 16MB 파트 2개.
        assert_eq!(part_size_for(MIN_PART + 1), MIN_PART);
        // 5.1GB → 16MB 파트, 파트 수 ≤ 10,000.
        let gb51 = 5_368_709_120u64;
        assert_eq!(part_size_for(gb51), MIN_PART);
        assert!(gb51.div_ceil(part_size_for(gb51)) <= 10_000);
        // 160GB 경계 → 파트 수가 정확히 10,000 이하.
        let gb160 = 160 * 1024 * MIB;
        assert!(gb160.div_ceil(part_size_for(gb160)) <= 10_000);
        // 1TB → 파트가 1MB 단위로 커져 파트 수 ≤ 10,000.
        let tb = 1u64 << 40;
        let ps = part_size_for(tb);
        assert_eq!(ps % MIB, 0);
        assert!(ps >= MIN_PART);
        assert!(tb.div_ceil(ps) <= 10_000);
    }

    #[test]
    fn upload_weight_cases() {
        const MIB: u64 = 1024 * 1024;
        // 작은 파일은 단일 PUT → 허가 1.
        assert_eq!(upload_weight(100, 8), 1);
        // 20MB → 16MB 파트 2개 → min(2, 8) = 2.
        assert_eq!(upload_weight(20 * MIB, 8), 2);
        // 동시성 1이면 허가도 1.
        assert_eq!(upload_weight(20 * MIB, 1), 1);
        // 큰 파일은 파트 수와 동시성 중 작은 쪽.
        assert_eq!(upload_weight(160 * 1024 * MIB, 4), 4);
    }

    #[tokio::test]
    async fn upload_fail_mode_conflict_on_existing() {
        let backend = FakeUploadBackend::default();
        let dir = TempDir::new("upfail");
        let src = write_src(&dir, "a.txt", b"hello");
        let opts = opts(8, 2);
        let progress = RecProgress::default();
        let cancel = CancelToken::new();

        let item = upload(
            &backend,
            &src,
            "docs/",
            "a.txt",
            &UploadMode::Fail,
            &opts,
            &progress,
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(item.key, "docs/a.txt");
        assert_eq!(item.size, 5);

        // 같은 이름을 다시 올리면 Conflict.
        let err = upload(
            &backend,
            &src,
            "docs/",
            "a.txt",
            &UploadMode::Fail,
            &opts,
            &progress,
            &cancel,
        )
        .await
        .expect_err("Conflict여야 한다");
        assert!(matches!(err, Error::Conflict(_)), "{err:?}");
        assert_eq!(backend.put_keys(), vec!["docs/a.txt"]);
    }

    #[tokio::test]
    async fn upload_keep_both_picks_free_name() {
        let backend = FakeUploadBackend::default();
        backend.seed("docs/a.txt", 3, Some("e0"));
        let dir = TempDir::new("upkeep");
        let src = write_src(&dir, "a.txt", b"new!");
        let item = upload(
            &backend,
            &src,
            "docs/",
            "a.txt",
            &UploadMode::KeepBoth,
            &opts(8, 2),
            &RecProgress::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(item.key, "docs/a (2).txt");
        assert_eq!(backend.put_keys(), vec!["docs/a (2).txt"]);
    }

    #[tokio::test]
    async fn upload_replace_if_match() {
        let backend = FakeUploadBackend::default();
        backend.seed("docs/a.txt", 3, Some("e1"));
        let dir = TempDir::new("uprep");
        let src = write_src(&dir, "a.txt", b"new!");
        let opts = opts(8, 2);
        let cancel = CancelToken::new();

        // ETag가 일치하면 성공.
        upload(
            &backend,
            &src,
            "docs/",
            "a.txt",
            &UploadMode::Replace {
                if_match: Some("e1".to_string()),
            },
            &opts,
            &RecProgress::default(),
            &cancel,
        )
        .await
        .unwrap();

        // ETag가 다르면 Conflict(쓰기 자체는 실패해 원본 유지).
        let err = upload(
            &backend,
            &src,
            "docs/",
            "a.txt",
            &UploadMode::Replace {
                if_match: Some("e0".to_string()),
            },
            &opts,
            &RecProgress::default(),
            &cancel,
        )
        .await
        .expect_err("Conflict여야 한다");
        assert!(matches!(err, Error::Conflict(_)), "{err:?}");

        // if_match=None은 무조건 덮어쓴다.
        upload(
            &backend,
            &src,
            "docs/",
            "a.txt",
            &UploadMode::Replace { if_match: None },
            &opts,
            &RecProgress::default(),
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(backend.put_keys().len(), 2);
    }

    #[tokio::test]
    async fn existing_returns_only_present_items() {
        let backend = FakeUploadBackend::default();
        backend.seed("p/a.txt", 1, None);
        backend.seed("p/b.txt", 1, None);
        // 폴더 마커도 "있는 항목"으로 본다.
        backend
            .objects
            .lock()
            .unwrap()
            .insert("p/dir/".to_string(), Item::folder("p/dir/"));
        let found = existing(
            &backend,
            "p/",
            &[
                "a.txt".to_string(),
                "b.txt".to_string(),
                "none".to_string(),
                "dir".to_string(),
            ],
        )
        .await
        .unwrap();
        let keys: Vec<&str> = found.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, vec!["p/a.txt", "p/b.txt", "p/dir/"]);
    }

    #[tokio::test]
    async fn upload_folder_uploads_tree_and_markers() {
        let backend = FakeUploadBackend::default();
        let dir = TempDir::new("upfolder");
        let src_dir = dir.0.join("srcdir");
        std::fs::create_dir_all(src_dir.join("sub")).unwrap();
        std::fs::create_dir_all(src_dir.join("empty")).unwrap();
        std::fs::write(src_dir.join("a.txt"), b"aaa").unwrap();
        std::fs::write(src_dir.join(".DS_Store"), b"junk").unwrap();
        std::fs::write(src_dir.join("sub/b.txt"), b"bb").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(src_dir.join("a.txt"), src_dir.join("link")).unwrap();
        }

        let result = upload_folder(
            &backend,
            &src_dir,
            "up/",
            &UploadMode::Fail,
            &opts(8, 2),
            &RecProgress::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(result.uploaded, 2);
        assert_eq!(result.total_bytes, 5);
        // .DS_Store와 심볼릭 링크는 무시된다.
        assert_eq!(result.ignored, 2);
        let mut keys = backend.put_keys();
        keys.sort();
        assert_eq!(keys, vec!["up/srcdir/a.txt", "up/srcdir/sub/b.txt"]);
        // 빈 디렉터리는 폴더 마커로 만든다.
        assert_eq!(
            backend.dirs.lock().unwrap().as_slice(),
            ["up/srcdir/empty/"]
        );
    }

    #[tokio::test]
    async fn upload_folder_fail_mode_skips_conflicts() {
        let backend = FakeUploadBackend::default();
        backend.seed("up/srcdir/a.txt", 3, None);
        let dir = TempDir::new("upfolderfail");
        let src_dir = dir.0.join("srcdir");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(src_dir.join("a.txt"), b"aaa").unwrap();
        std::fs::write(src_dir.join("b.txt"), b"b").unwrap();

        let result = upload_folder(
            &backend,
            &src_dir,
            "up/",
            &UploadMode::Fail,
            &opts(8, 2),
            &RecProgress::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(result.uploaded, 1);
        assert_eq!(result.skipped, 1);
        assert_eq!(backend.put_keys(), vec!["up/srcdir/b.txt"]);
    }

    #[tokio::test]
    async fn upload_folder_empty_dir_creates_marker_only() {
        let backend = FakeUploadBackend::default();
        let dir = TempDir::new("upempty");
        let src_dir = dir.0.join("emptydir");
        std::fs::create_dir_all(&src_dir).unwrap();

        let result = upload_folder(
            &backend,
            &src_dir,
            "up/",
            &UploadMode::Fail,
            &opts(8, 2),
            &RecProgress::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(result.uploaded, 0);
        assert_eq!(result.total_bytes, 0);
        assert!(backend.put_keys().is_empty());
        assert_eq!(backend.dirs.lock().unwrap().as_slice(), ["up/emptydir/"]);
    }
}
