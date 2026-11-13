//! SPEC 4.4 다운로드: 범위 읽기 병렬 전송, 취소, 진행률.
//!
//! 객체 크기가 chunk_size 이하면 범위 요청 1개, 초과하면 chunk_size 범위를
//! 최대 concurrency개 동시에 요청해 임시 파일의 정해진 오프셋에 직접 쓴다.
//! 모든 작업은 dest와 같은 디렉터리의 임시 파일(`.<이름>.pierdownload`)에
//! 쓰고, 크기 검증 후 rename한다. 실패·취소 시 임시 파일을 지우고 dest는
//! 건드리지 않는다.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Notify, Semaphore};
use tokio::task::JoinSet;

use crate::backend::Backend;
use crate::error::Error;
use crate::item::Item;
use crate::key;

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
    if cancel.is_cancelled() {
        return Err(cancelled_error());
    }
    // 크기 확인을 겸해 없는 키는 여기서 NotFound가 난다.
    let item = backend.stat(key).await?;
    if item.is_dir {
        return Err(Error::Other(format!("{key}는 폴더라 다운로드할 수 없다")));
    }
    let size = item.size;

    let tmp = temp_path(dest);
    if let Err(err) = download_inner(backend, key, &tmp, size, opts, progress, cancel).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(err);
    }
    if let Err(err) = tokio::fs::rename(&tmp, dest).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(err.into());
    }
    Ok(size)
}

/// 임시 파일에 전체 바이트를 쓴다. 호출자가 성공·실패 모두 임시 파일을
/// 정리할 책임을 진다.
async fn download_inner<B>(
    backend: &B,
    key: &str,
    tmp: &Path,
    size: u64,
    opts: &TransferOptions,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<(), Error>
where
    B: Backend + Clone + Send + Sync + 'static,
{
    // 목적 크기로 미리 길이를 잡는다(희소 파일). 청크 태스크는 각자
    // 파일 핸들을 열어 오프셋에 직접 쓰므로 순서와 무관하게 완성된다.
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(tmp)
        .await?;
    file.set_len(size).await?;
    drop(file);

    if size == 0 {
        progress.on_progress(0, 0);
        return Ok(());
    }

    // 동시 요청 수는 세마포어로 제한한다. 청크 수만큼 태스크를 만들지만
    // 실제 네트워크 요청은 concurrency개까지다.
    let sem = Arc::new(Semaphore::new(opts.concurrency.max(1)));
    let mut set = JoinSet::new();
    for (offset, len) in chunk_ranges(size, opts.chunk_size) {
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
            fetch_chunk(&backend, &key, &tmp, offset, len, &cancel).await
        });
    }

    let mut transferred = 0u64;
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
            Some(Ok(Ok(done))) => {
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

    // 대상 폴더명은 마지막 키 구간을 안전한 로컬 이름으로 바꾼 값이다.
    let folder_name = safe_rel_path(key::name(&prefix))
        .to_string_lossy()
        .into_owned();
    let target = unique_target_dir(dest_dir, &folder_name)?;
    let plan = plan_layout(&target, &prefix, &items)?;

    let total_bytes: u64 = plan.files.iter().map(|f| f.size).sum();
    let file_count = plan.files.len() as u64;
    // 목록을 받은 직후 진행률 초기값을 한 번 보낸다.
    progress.on_progress(0, total_bytes);

    // 실패·취소 시 미완료 임시 파일을 지운다. 이미 rename된 파일은 둔다.
    let result = run_folder_download(backend, &plan, &target, opts, progress, cancel).await;
    if result.is_err() {
        for job in &plan.files {
            let _ = tokio::fs::remove_file(&job.tmp).await;
        }
    }
    result?;

    Ok(FolderDownload {
        local_path: target,
        file_count,
        total_bytes,
    })
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
    let mut file_rels: Vec<(&str, u64, PathBuf)> = Vec::new();

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
            file_rels.push((item.key.as_str(), item.size, rel));
        }
    }

    // 디렉터리는 이미 점유된 경로로 보고 파일 목적지를 배정한다.
    let mut taken = dir_set.clone();
    let mut files = Vec::with_capacity(file_rels.len());
    for (key, size, rel) in file_rels {
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

/// 디렉터리 생성과 파일 다운로드를 실행한다.
/// 모든 파일의 청크를 하나의 JoinSet에 넣고 공유 세마포어로 총 동시 요청
/// 수를 opts.concurrency로 제한한다. 파일이 끝날 때마다 tmp를 dest로 rename한다.
async fn run_folder_download<B>(
    backend: &B,
    plan: &FolderPlan,
    target: &Path,
    opts: &TransferOptions,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<(), Error>
where
    B: Backend + Clone + Send + Sync + 'static,
{
    tokio::fs::create_dir_all(target).await?;
    for dir in &plan.dirs {
        check_symlinks(target, dir)?;
        tokio::fs::create_dir_all(dir).await?;
    }

    // 0바이트 파일은 청크가 없으므로 임시 파일을 만들어 바로 완료시킨다.
    for job in plan.files.iter().filter(|f| f.size == 0) {
        tokio::fs::write(&job.tmp, b"").await?;
        tokio::fs::rename(&job.tmp, &job.dest).await?;
    }

    // 파일별 남은 바이트. 모두 받으면 tmp를 dest로 rename한다.
    let mut remaining: Vec<u64> = plan.files.iter().map(|f| f.size).collect();
    let sem = Arc::new(Semaphore::new(opts.concurrency.max(1)));
    let mut set = JoinSet::new();
    for (idx, job) in plan.files.iter().enumerate() {
        if job.size == 0 {
            continue;
        }
        // 목적 크기로 미리 길이를 잡는다(희소 파일).
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&job.tmp)
            .await?;
        file.set_len(job.size).await?;
        drop(file);

        for (offset, len) in chunk_ranges(job.size, opts.chunk_size) {
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
                    .map(|bytes| (idx, bytes))
            });
        }
    }

    let total_bytes: u64 = plan.files.iter().map(|f| f.size).sum();
    let mut done = 0u64;
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
            Some(Ok(Ok((idx, bytes)))) => {
                done += bytes;
                remaining[idx] -= bytes;
                progress.on_progress(done, total_bytes);
                if remaining[idx] == 0 {
                    let job = &plan.files[idx];
                    if let Err(err) = tokio::fs::rename(&job.tmp, &job.dest).await {
                        set.abort_all();
                        return Err(err.into());
                    }
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
}
