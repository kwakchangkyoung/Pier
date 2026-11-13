//! SPEC 3.1~3.3 객체 작업: 폴더 생성·삭제·이름 변경/이동, 보류 작업 재개,
//! 쓰기 권한 확인.
//!
//! 네트워크는 Backend가, 캐시 반영은 MetadataStore가 담당한다. 이 모듈은
//! 둘을 엮어 "원격 작업 성공 → 캐시 즉시 반영" 순서를 보장한다.
//! 잠금은 await를 넘나들지 않게 짧게 잡는다.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::SystemTime;

use crate::backend::Backend;
use crate::error::Error;
use crate::item::Item;
use crate::key;
use crate::metadata::MetadataStore;

/// 폴더 이름 변경이 허용하는 최대 하위 객체 수(SPEC 3.3).
pub const MAX_FOLDER_RENAME: usize = 1000;

/// pending_ops의 폴더 이름 변경 작업 종류.
const FOLDER_RENAME_KIND: &str = "folder_rename";

/// 폴더 이름 변경 진행 상황을 payload에 갱신하는 간격(객체 수).
const PROGRESS_EVERY: usize = 50;

/// parent 아래 name으로 새 폴더를 만들고 캐시에 반영해 반환한다.
///
/// name은 비어 있지 않고 '/'를 포함하면 안 된다. 대상 이름이 parent의
/// 직계 자식(파일·폴더 무관)으로 이미 있으면 Error::Conflict다.
/// 캐시에는 폴더 항목만 넣고, 새 폴더 자체의 목록 상태는 만들지 않는다
/// (열 때 원격에서 받는다).
pub async fn create_folder<B: Backend>(
    backend: &B,
    store: &Mutex<MetadataStore>,
    parent: &str,
    name: &str,
) -> Result<Item, Error> {
    if name.is_empty() || name.contains('/') {
        return Err(Error::Other(format!("폴더 이름이 올바르지 않다: {name}")));
    }
    let parent = key::normalize_prefix(parent);
    if child_name_taken(backend, &parent, name).await? {
        return Err(Error::Conflict(format!("{parent}{name}/가 이미 존재한다")));
    }
    let folder_key = key::child_key(&parent, name, true);
    backend.create_dir(&folder_key).await?;
    let item = Item::folder(folder_key);
    lock(store)?.upsert_item(&item, now_ms())?;
    Ok(item)
}

/// key를 삭제한다. 폴더 키(끝 '/')면 마커와 하위 객체 전체를 배치 삭제한다
/// (SPEC 3.2). 원격 삭제가 성공한 뒤에만 캐시를 지운다.
/// 없는 키는 성공으로 넘어간다(S3 삭제 의미와 같다).
pub async fn delete<B: Backend>(
    backend: &B,
    store: &Mutex<MetadataStore>,
    key: &str,
) -> Result<(), Error> {
    if key.is_empty() {
        return Err(Error::Other("루트는 삭제할 수 없다".to_string()));
    }
    // 폴더 마커 자신은 list_recursive가 제외하므로 직접 넣는다.
    // 없는 마커를 지워도 성공으로 간주되므로 존재 여부를 따로 확인하지 않는다.
    let mut keys = vec![key.to_string()];
    if key.ends_with('/') {
        let items = backend.list_recursive(key).await?;
        keys.extend(items.iter().map(|item| item.key.clone()));
    }
    backend.delete_keys(&keys).await?;
    lock(store)?.remove_item(key)?;
    Ok(())
}

/// 이름 변경·이동 공통 (SPEC 3.3). new_parent는 폴더 키(끝 '/' 또는 루트
/// "")이고 new_name에는 '/'가 없어야 한다. 폴더 키를 받으면 new_name에
/// '/'를 붙여 폴더로 만든다.
///
/// - 대상 이름이 new_parent 직계 자식으로 이미 있으면 Error::Conflict.
/// - 폴더를 자기 자신이나 하위로 옮기면 Error::Other.
/// - 동일 위치(키가 그대로)면 아무 작업 없이 현재 항목을 반환한다.
/// - 파일은 copy 성공 후에만 원본을 지운다. 폴더는 하위 전체 복사 후
///   원본 일괄 삭제이며, 1,000개를 넘으면 거부한다. 중간 실패 시
///   pending_ops에 진행 상황이 남아 resume_pending_ops가 이어서 완료한다.
pub async fn rename<B: Backend>(
    backend: &B,
    store: &Mutex<MetadataStore>,
    key: &str,
    new_parent: &str,
    new_name: &str,
) -> Result<Item, Error> {
    if key.is_empty() {
        return Err(Error::Other("루트는 이름을 바꿀 수 없다".to_string()));
    }
    if new_name.is_empty() || new_name.contains('/') {
        return Err(Error::Other(format!("이름이 올바르지 않다: {new_name}")));
    }
    let new_parent = key::normalize_prefix(new_parent);
    // 폴더 여부는 키의 끝 '/'로 판별한다(계약: 폴더 키는 '/'로 끝난다).
    let is_dir = key.ends_with('/');
    let to_key = key::child_key(&new_parent, new_name, is_dir);

    // 동일 위치면 아무것도 하지 않고 현재 항목을 반환한다.
    if to_key == key {
        return current_item(backend, key, is_dir).await;
    }
    // 폴더를 자기 자신이나 하위 폴더로는 옮길 수 없다.
    // new_parent는 '/'로 끝나므로 "a/bc/" 같은 접두 우연 일치는 없다.
    if is_dir && new_parent.starts_with(key) {
        return Err(Error::Other(format!("{key}를 자기 하위로 옮길 수 없다")));
    }
    // 대상 이름이 이미 있으면 Conflict. 묵시적 폴더(마커 없이 하위만
    // 있는 폴더)도 목록에 나타나므로 잡힌다.
    if child_name_taken(backend, &new_parent, new_name).await? {
        return Err(Error::Conflict(format!("{to_key}가 이미 존재한다")));
    }

    if is_dir {
        rename_folder(backend, store, key, &to_key).await
    } else {
        rename_file(backend, store, key, &to_key).await
    }
}

/// 중단된 폴더 이름 변경(pending_ops의 folder_rename)을 이어서 완료하고
/// 처리한 작업 수를 반환한다. 이미 복사된 키를 다시 copy해도 결과가 같으므로
/// 진행 인덱스부터 재개한다. 첫 실패에서 멈추고 에러를 전파한다 — 남은
/// 작업은 다음 호출이 다시 시도한다.
pub async fn resume_pending_ops<B: Backend>(
    backend: &B,
    store: &Mutex<MetadataStore>,
) -> Result<u32, Error> {
    let ops = lock(store)?.pending_ops()?;
    let mut resumed = 0u32;
    for op in ops {
        if op.kind != FOLDER_RENAME_KIND {
            continue;
        }
        let state = parse_folder_rename_payload(&op.payload)?;
        copy_folder_keys(
            backend,
            store,
            op.id,
            &state.keys,
            &state.from,
            &state.to,
            state.done,
        )
        .await?;
        // 원본이 이미 지워졌을 수 있으므로 없는 키 삭제는 성공으로 넘어간다.
        backend.delete_keys(&state.keys).await?;
        let mut store = lock(store)?;
        store.remove_pending_op(op.id)?;
        store.remove_item(&state.from)?;
        store.upsert_item(&Item::folder(state.to.clone()), now_ms())?;
        resumed += 1;
    }
    Ok(resumed)
}

/// `.pier-write-test-<고유값>` 객체를 쓰고 지워 쓰기 권한을 확인한다.
/// 권한 거부(Error::Authentication, PermissionDenied 계열)는 Ok(false),
/// 그 외 오류는 Err로 전파한다. 성공 시 테스트 객체는 남기지 않는다.
pub async fn test_write<B: Backend>(backend: &B) -> Result<bool, Error> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    // crate 없이 유일한 이름: 시각 밀리초 + 프로세스 id + 원자적 카운터.
    let unique = format!(
        "{}-{}-{}",
        now_ms(),
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let key = format!(".pier-write-test-{unique}");
    match backend.write_bytes(&key, b"pier".to_vec()).await {
        Ok(()) => {}
        Err(Error::Authentication(_)) => return Ok(false),
        Err(e) => return Err(e),
    }
    backend.delete_keys(&[key]).await?;
    Ok(true)
}

/// parent 직계 자식 중 name을 가진 항목이 있으면 true.
/// 파일 "b"와 폴더 "b/"가 한 목록에 같은 이름으로 나타날 수 있어
/// 파일·폴더 구분 없이 이름만 비교한다.
async fn child_name_taken<B: Backend>(
    backend: &B,
    parent: &str,
    name: &str,
) -> Result<bool, Error> {
    let parent = key::normalize_prefix(parent);
    Ok(backend
        .list_folder(&parent)
        .await?
        .iter()
        .any(|item| item.name == name))
}

/// rename의 동일 위치 반환용. 파일은 원격 stat으로 최신 메타데이터를 받고,
/// 폴더는 메타데이터가 없으므로 키로 바로 만든다(묵시적 폴더도 stat이
/// NotFound라 폴더 항목을 그대로 반환한다).
async fn current_item<B: Backend>(backend: &B, key: &str, is_dir: bool) -> Result<Item, Error> {
    if is_dir {
        Ok(Item::folder(key))
    } else {
        backend.stat(key).await
    }
}

/// 파일 이름 변경·이동. copy 성공 후에만 원본을 지운다(SPEC 3.3).
/// 5GB 초과 객체는 백엔드 copy가 멀티파트 복사로 처리한다(C-A 계약).
async fn rename_file<B: Backend>(
    backend: &B,
    store: &Mutex<MetadataStore>,
    from: &str,
    to: &str,
) -> Result<Item, Error> {
    // 원본이 없으면 여기서 NotFound가 난다.
    backend.stat(from).await?;
    backend.copy(from, to).await?;
    // 복사본 메타데이터를 먼저 읽는다(복사로 ETag가 바뀔 수 있다).
    // 이 시점까지 원본 삭제는 아직이다.
    let item = backend.stat(to).await?;
    backend.delete_keys(&[from.to_string()]).await?;
    let mut store = lock(store)?;
    store.remove_item(from)?;
    store.upsert_item(&item, now_ms())?;
    Ok(item)
}

/// 폴더 이름 변경·이동. 하위 객체 전체를 키 순으로 복사한 뒤 원본을
/// 일괄 삭제한다. 중간 실패 시 pending_ops에 진행 인덱스가 남는다.
async fn rename_folder<B: Backend>(
    backend: &B,
    store: &Mutex<MetadataStore>,
    from: &str,
    to: &str,
) -> Result<Item, Error> {
    let items = backend.list_recursive(from).await?;
    if items.len() > MAX_FOLDER_RENAME {
        return Err(Error::Other(format!(
            "폴더 안 객체가 {}개라 이름을 바꿀 수 없습니다(최대 1,000개)",
            items.len()
        )));
    }
    // 복사할 키 목록을 확정한다. 폴더 마커 자신은 list_recursive가
    // 제외하므로 stat으로 확인해 맨 앞에 넣는다. 마커가 없는 묵시적
    // 폴더는 NotFound라 건너뛴다.
    let mut keys: Vec<String> = Vec::with_capacity(items.len() + 1);
    match backend.stat(from).await {
        Ok(_) => keys.push(from.to_string()),
        Err(Error::NotFound(_)) => {}
        Err(e) => return Err(e),
    }
    keys.extend(items.iter().map(|item| item.key.clone()));
    if keys.is_empty() {
        return Err(Error::NotFound(from.to_string()));
    }

    // 진행 상황을 pending_ops에 기록한다. 앱이 죽어도 재시작 시
    // resume_pending_ops가 이어서 완료한다(SPEC 3.3).
    let payload = folder_rename_payload(from, to, &keys, 0);
    let op_id = lock(store)?.add_pending_op(FOLDER_RENAME_KIND, &payload, now_ms())?;

    // 복사 실패·취소 시 pending op은 남겨 둔다.
    copy_folder_keys(backend, store, op_id, &keys, from, to, 0).await?;
    // 전부 복사됐으므로 원본을 일괄 삭제한다(데이터 유실 방지: 복사 성공 후에만).
    backend.delete_keys(&keys).await?;
    let mut store = lock(store)?;
    store.remove_pending_op(op_id)?;
    // 캐시: 원본 폴더의 하위 캐시를 통째로 지우고 대상 폴더 항목만 넣는다.
    // 대상 폴더의 자식 목록은 없으므로 열 때 원격에서 새로 받는다.
    store.remove_item(from)?;
    store.upsert_item(&Item::folder(to.to_string()), now_ms())?;
    Ok(Item::folder(to.to_string()))
}

/// keys[start..]를 from_prefix → to_prefix로 순서대로 서버 측 복사한다.
/// 키의 from_prefix 이후 부분을 to_prefix에 붙인다. PROGRESS_EVERY개마다
/// pending op의 done 인덱스를 payload에 갱신한다.
///
/// '/'로 끝나는 키는 폴더 마커(0바이트 객체)다. OpenDAL copy는 이런
/// 경로를 디렉터리로 보고 IsADirectory로 거부하므로 create_dir로
/// 같은 결과(0바이트 `to/` 객체)를 만든다.
#[allow(clippy::too_many_arguments)]
async fn copy_folder_keys<B: Backend>(
    backend: &B,
    store: &Mutex<MetadataStore>,
    op_id: i64,
    keys: &[String],
    from_prefix: &str,
    to_prefix: &str,
    start: usize,
) -> Result<(), Error> {
    for (i, key) in keys.iter().enumerate().skip(start) {
        let to = format!("{to_prefix}{}", &key[from_prefix.len()..]);
        if key.ends_with('/') {
            backend.create_dir(&to).await?;
        } else {
            backend.copy(key, &to).await?;
        }
        let done = i + 1;
        if done % PROGRESS_EVERY == 0 || done == keys.len() {
            let payload = folder_rename_payload(from_prefix, to_prefix, keys, done);
            lock(store)?.update_pending_op(op_id, &payload)?;
        }
    }
    Ok(())
}

/// folder_rename pending op의 payload를 만든다.
/// 줄 단위 `키=값` 형식이며 키 값은 퍼센트 인코딩한다.
fn folder_rename_payload(from: &str, to: &str, keys: &[String], done: usize) -> String {
    let mut payload = String::new();
    payload.push_str("v=1\n");
    payload.push_str(&format!("from={}\n", pct_encode(from)));
    payload.push_str(&format!("to={}\n", pct_encode(to)));
    payload.push_str(&format!("done={done}\n"));
    for key in keys {
        payload.push_str(&format!("k={}\n", pct_encode(key)));
    }
    payload
}

/// folder_rename payload를 파싱한다. 형식이 깨졌으면 Err.
fn parse_folder_rename_payload(payload: &str) -> Result<FolderRenameState, Error> {
    let bad = || Error::Other("folder_rename payload가 손상됐다".to_string());
    let mut lines = payload.lines();
    if lines.next() != Some("v=1") {
        return Err(bad());
    }
    let mut from = None;
    let mut to = None;
    let mut done = 0usize;
    let mut keys = Vec::new();
    for line in lines {
        if let Some(v) = line.strip_prefix("from=") {
            from = Some(pct_decode(v)?);
        } else if let Some(v) = line.strip_prefix("to=") {
            to = Some(pct_decode(v)?);
        } else if let Some(v) = line.strip_prefix("done=") {
            done = v.parse().map_err(|_| bad())?;
        } else if let Some(v) = line.strip_prefix("k=") {
            keys.push(pct_decode(v)?);
        } else {
            return Err(bad());
        }
    }
    let (Some(from), Some(to)) = (from, to) else {
        return Err(bad());
    };
    if done > keys.len() {
        return Err(bad());
    }
    Ok(FolderRenameState {
        from,
        to,
        keys,
        done,
    })
}

/// 파싱한 folder_rename payload.
struct FolderRenameState {
    from: String,
    to: String,
    /// 복사할 원본 키 전체(폴더 마커 포함, 오름차순).
    keys: Vec<String>,
    /// 이미 복사를 끝낸 개수.
    done: usize,
}

/// S3 키를 퍼센트 인코딩한다. payload가 줄 단위 텍스트라 개행·'='·'%'
/// 같은 문자가 값 안에 있어도 파싱이 깨지지 않게, unreserved 문자
/// (A-Za-z0-9-_.~)만 그대로 두고 나머지 바이트를 %XX로 바꾼다
/// (RFC 3986). 한글 등 비ASCII는 UTF-8 바이트 단위로 인코딩한다.
fn pct_encode(input: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0xF) as usize] as char);
            }
        }
    }
    out
}

/// pct_encode의 역변환. 잘못된 % 시퀀스·UTF-8이 아니면 Err.
fn pct_decode(input: &str) -> Result<String, Error> {
    let bad = || Error::Other("folder_rename payload의 인코딩이 깨졌다".to_string());
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err(bad());
            }
            let hi = hex_value(bytes[i + 1]).ok_or_else(bad)?;
            let lo = hex_value(bytes[i + 2]).ok_or_else(bad)?;
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| bad())
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'A'..=b'F' => Some(b - b'A' + 10),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

/// 메타데이터 캐시 잠금. 포이즌이면 다른 오류로 변환한다
/// (enumerate::Connection::lock과 같은 규칙).
fn lock(store: &Mutex<MetadataStore>) -> Result<MutexGuard<'_, MetadataStore>, Error> {
    store
        .lock()
        .map_err(|_| Error::Other("메타데이터 캐시 잠금이 깨졌다".to_string()))
}

/// 현재 시각의 Unix epoch 밀리초.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;

    /// 메모리 객체 저장소를 가진 가짜 백엔드.
    /// copy/delete/write 호출을 기록하고 copy 실패를 주입할 수 있다.
    #[derive(Clone, Default)]
    struct FakeBackend {
        state: Arc<Mutex<FakeState>>,
    }

    #[derive(Default)]
    struct FakeState {
        /// key → 객체. 폴더 마커도 is_dir=true로 들어간다.
        objects: HashMap<String, FakeObj>,
        copies: Vec<(String, String)>,
        deletes: Vec<Vec<String>>,
        writes: Vec<String>,
        /// create_dir로 만든 키(폴더 마커) 기록.
        created_dirs: Vec<String>,
        /// 몇 번째 copy 호출(1부터)에서 실패할지. None이면 항상 성공.
        fail_copy_at: Option<usize>,
        /// write_bytes가 Authentication 오류를 내는지.
        deny_write: bool,
    }

    #[derive(Clone)]
    struct FakeObj {
        is_dir: bool,
        data: Vec<u8>,
    }

    impl FakeBackend {
        fn put_file(&self, key: &str, data: &[u8]) {
            self.state.lock().unwrap().objects.insert(
                key.to_string(),
                FakeObj {
                    is_dir: false,
                    data: data.to_vec(),
                },
            );
        }

        fn put_dir(&self, key: &str) {
            self.state.lock().unwrap().objects.insert(
                key.to_string(),
                FakeObj {
                    is_dir: true,
                    data: Vec::new(),
                },
            );
        }

        fn has(&self, key: &str) -> bool {
            self.state.lock().unwrap().objects.contains_key(key)
        }

        fn copies(&self) -> Vec<(String, String)> {
            self.state.lock().unwrap().copies.clone()
        }

        fn deletes(&self) -> Vec<Vec<String>> {
            self.state.lock().unwrap().deletes.clone()
        }

        fn writes(&self) -> Vec<String> {
            self.state.lock().unwrap().writes.clone()
        }

        fn created_dirs(&self) -> Vec<String> {
            self.state.lock().unwrap().created_dirs.clone()
        }

        fn fail_copy_at(&self, n: usize) {
            self.state.lock().unwrap().fail_copy_at = Some(n);
        }

        fn deny_write(&self) {
            self.state.lock().unwrap().deny_write = true;
        }
    }

    impl Backend for FakeBackend {
        async fn list_folder(&self, prefix: &str) -> Result<Vec<Item>, Error> {
            let state = self.state.lock().unwrap();
            let prefix = key::normalize_prefix(prefix);
            let mut seen: HashMap<String, Item> = HashMap::new();
            for k in state.objects.keys() {
                if !k.starts_with(&prefix) || k == &prefix {
                    continue;
                }
                let rest = &k[prefix.len()..];
                // 첫 '/'가 rest의 마지막 글자면 직계 폴더 마커,
                // 중간에 있으면 그 첫 구간이 직계 폴더, 없으면 직계 파일.
                match rest.find('/') {
                    None => {
                        let obj = &state.objects[k];
                        seen.insert(
                            k.clone(),
                            Item::file(k.clone(), obj.data.len() as u64, None, None),
                        );
                    }
                    Some(i) => {
                        let name = &rest[..i];
                        let folder_key = format!("{prefix}{name}/");
                        seen.entry(folder_key.clone())
                            .or_insert_with(|| Item::folder(folder_key));
                    }
                }
            }
            let mut items: Vec<Item> = seen.into_values().collect();
            items.sort_by(|a, b| a.key.cmp(&b.key));
            Ok(items)
        }

        async fn list_recursive(&self, prefix: &str) -> Result<Vec<Item>, Error> {
            let state = self.state.lock().unwrap();
            let mut items: Vec<Item> = state
                .objects
                .iter()
                .filter(|(k, _)| k.starts_with(prefix) && k.as_str() != prefix)
                .map(|(k, obj)| {
                    if obj.is_dir {
                        Item::folder(k.clone())
                    } else {
                        Item::file(k.clone(), obj.data.len() as u64, None, None)
                    }
                })
                .collect();
            items.sort_by(|a, b| a.key.cmp(&b.key));
            Ok(items)
        }

        async fn stat(&self, key: &str) -> Result<Item, Error> {
            let state = self.state.lock().unwrap();
            match state.objects.get(key) {
                Some(obj) if obj.is_dir => Ok(Item::folder(key)),
                Some(obj) => Ok(Item::file(key, obj.data.len() as u64, None, None)),
                None => Err(Error::NotFound(key.to_string())),
            }
        }

        async fn create_dir(&self, key: &str) -> Result<(), Error> {
            let mut state = self.state.lock().unwrap();
            state.created_dirs.push(key.to_string());
            state.objects.insert(
                key.to_string(),
                FakeObj {
                    is_dir: true,
                    data: Vec::new(),
                },
            );
            Ok(())
        }

        async fn copy(&self, from: &str, to: &str) -> Result<(), Error> {
            let mut state = self.state.lock().unwrap();
            state.copies.push((from.to_string(), to.to_string()));
            if state.fail_copy_at == Some(state.copies.len()) {
                return Err(Error::Other(format!("copy {from} 실패 주입")));
            }
            let obj = state
                .objects
                .get(from)
                .cloned()
                .ok_or_else(|| Error::NotFound(from.to_string()))?;
            state.objects.insert(to.to_string(), obj);
            Ok(())
        }

        async fn delete_keys(&self, keys: &[String]) -> Result<(), Error> {
            let mut state = self.state.lock().unwrap();
            state.deletes.push(keys.to_vec());
            for k in keys {
                state.objects.remove(k);
            }
            Ok(())
        }

        async fn write_bytes(&self, key: &str, data: Vec<u8>) -> Result<(), Error> {
            let mut state = self.state.lock().unwrap();
            if state.deny_write {
                return Err(Error::Authentication("fake: 쓰기 권한 없음".to_string()));
            }
            state.writes.push(key.to_string());
            state.objects.insert(
                key.to_string(),
                FakeObj {
                    is_dir: false,
                    data,
                },
            );
            Ok(())
        }
    }

    fn store() -> Mutex<MetadataStore> {
        Mutex::new(MetadataStore::open_in_memory().unwrap())
    }

    #[tokio::test]
    async fn create_folder_conflict_and_cache() {
        let backend = FakeBackend::default();
        backend.put_file("p/existing.txt", b"x");
        let store = store();

        let item = create_folder(&backend, &store, "p/", "newdir")
            .await
            .unwrap();
        assert_eq!(item.key, "p/newdir/");
        assert!(item.is_dir);
        assert!(backend.has("p/newdir/"));
        // 캐시의 p/ 목록에 폴더가 보인다.
        let page = store.lock().unwrap().cached_page("p/", None, 10).unwrap();
        assert!(page.iter().any(|i| i.key == "p/newdir/"));

        // 같은 이름으로 다시 만들면 Conflict.
        let err = create_folder(&backend, &store, "p/", "newdir")
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Conflict(_)), "{err:?}");

        // 파일과 같은 이름의 폴더도 Conflict.
        let err = create_folder(&backend, &store, "p/", "existing.txt")
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Conflict(_)), "{err:?}");

        // 잘못된 이름.
        assert!(create_folder(&backend, &store, "p/", "").await.is_err());
        assert!(create_folder(&backend, &store, "p/", "a/b").await.is_err());
    }

    #[tokio::test]
    async fn delete_file_and_folder() {
        let backend = FakeBackend::default();
        backend.put_file("p/a.txt", b"a");
        backend.put_file("p/b.txt", b"b");
        backend.put_dir("p/sub/");
        backend.put_file("p/sub/x.txt", b"x");
        backend.put_dir("p/sub/deep/");
        backend.put_file("p/sub/deep/y.txt", b"y");
        let store = store();
        // 캐시도 채워 둔다.
        {
            let mut s = store.lock().unwrap();
            s.replace_folder(
                "p/",
                &[
                    Item::file("p/a.txt", 1, None, None),
                    Item::file("p/b.txt", 1, None, None),
                    Item::folder("p/sub/"),
                ],
                1,
            )
            .unwrap();
            s.replace_folder(
                "p/sub/",
                &[
                    Item::file("p/sub/x.txt", 1, None, None),
                    Item::folder("p/sub/deep/"),
                ],
                1,
            )
            .unwrap();
            s.replace_folder(
                "p/sub/deep/",
                &[Item::file("p/sub/deep/y.txt", 1, None, None)],
                1,
            )
            .unwrap();
        }

        // 파일 삭제.
        delete(&backend, &store, "p/a.txt").await.unwrap();
        assert!(!backend.has("p/a.txt"));
        assert!(backend.has("p/b.txt"));
        assert!(store.lock().unwrap().get_item("p/a.txt").unwrap().is_none());

        // 폴더 삭제: 마커 + 하위 전체가 한 번의 delete_keys에 담긴다.
        delete(&backend, &store, "p/sub/").await.unwrap();
        assert!(!backend.has("p/sub/"));
        assert!(!backend.has("p/sub/x.txt"));
        assert!(!backend.has("p/sub/deep/"));
        assert!(!backend.has("p/sub/deep/y.txt"));
        assert!(backend.has("p/b.txt"));
        // 캐시도 하위까지 정리된다.
        let s = store.lock().unwrap();
        assert!(s.get_item("p/sub/").unwrap().is_none());
        assert!(s.get_item("p/sub/deep/y.txt").unwrap().is_none());
        assert!(s.folder_state("p/sub/deep/").unwrap().is_none());
        assert!(s.get_item("p/b.txt").unwrap().is_some());
    }

    #[tokio::test]
    async fn rename_file_copies_then_deletes() {
        let backend = FakeBackend::default();
        backend.put_file("a/old.txt", b"data");
        let store = store();

        let item = rename(&backend, &store, "a/old.txt", "b/", "new.txt")
            .await
            .unwrap();
        assert_eq!(item.key, "b/new.txt");
        assert_eq!(item.size, 4);
        // copy가 delete보다 먼저다.
        assert_eq!(
            backend.copies(),
            vec![("a/old.txt".to_string(), "b/new.txt".to_string())]
        );
        assert_eq!(backend.deletes(), vec![vec!["a/old.txt".to_string()]]);
        assert!(!backend.has("a/old.txt"));
        assert!(backend.has("b/new.txt"));
        // 캐시에 새 위치가 반영되고 옛 위치는 없다.
        let s = store.lock().unwrap();
        assert!(s.get_item("a/old.txt").unwrap().is_none());
        assert_eq!(s.get_item("b/new.txt").unwrap().unwrap().size, 4);
    }

    #[tokio::test]
    async fn rename_file_same_location_is_noop() {
        let backend = FakeBackend::default();
        backend.put_file("a/f.txt", b"data");
        let store = store();
        let item = rename(&backend, &store, "a/f.txt", "a/", "f.txt")
            .await
            .unwrap();
        assert_eq!(item.key, "a/f.txt");
        assert!(backend.copies().is_empty());
        assert!(backend.deletes().is_empty());
    }

    #[tokio::test]
    async fn rename_conflict_and_self_move() {
        let backend = FakeBackend::default();
        backend.put_file("a/f.txt", b"1");
        backend.put_file("b/f.txt", b"2");
        backend.put_dir("d/");
        backend.put_file("d/inner.txt", b"3");
        let store = store();

        // 대상 이름이 이미 있으면 Conflict.
        let err = rename(&backend, &store, "a/f.txt", "b/", "f.txt")
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Conflict(_)), "{err:?}");

        // 폴더를 자기 자신/하위로는 못 옮긴다.
        let err = rename(&backend, &store, "d/", "d/", "x").await.unwrap_err();
        assert!(matches!(err, Error::Other(_)), "{err:?}");
        let err = rename(&backend, &store, "d/", "d/sub/", "x")
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Other(_)), "{err:?}");

        // 없는 원본은 NotFound.
        let err = rename(&backend, &store, "a/none.txt", "b/", "x.txt")
            .await
            .unwrap_err();
        assert!(matches!(err, Error::NotFound(_)), "{err:?}");
    }

    #[tokio::test]
    async fn rename_folder_copies_all_then_deletes() {
        let backend = FakeBackend::default();
        backend.put_dir("old/");
        backend.put_file("old/a.txt", b"a");
        backend.put_dir("old/sub/");
        backend.put_file("old/sub/b.txt", b"b");
        let store = store();

        let item = rename(&backend, &store, "old/", "", "new").await.unwrap();
        assert_eq!(item.key, "new/");
        assert!(item.is_dir);

        // 파일 2개는 copy, 폴더 마커 2개(old/, old/sub/)는 create_dir로
        // 만들어진다(OpenDAL copy는 '/' 경로를 거부한다).
        let copies = backend.copies();
        assert_eq!(copies.len(), 2, "{copies:?}");
        assert!(copies.contains(&("old/a.txt".to_string(), "new/a.txt".to_string())));
        assert!(copies.contains(&("old/sub/b.txt".to_string(), "new/sub/b.txt".to_string())));
        let mut dirs = backend.created_dirs();
        dirs.sort();
        assert_eq!(dirs, ["new/", "new/sub/"]);

        // 원본 전체가 한 번에 삭제된다.
        assert_eq!(backend.deletes().len(), 1);
        for k in ["old/", "old/a.txt", "old/sub/", "old/sub/b.txt"] {
            assert!(!backend.has(k), "{k}가 남았다");
        }
        // pending op은 정리됐다.
        assert!(store.lock().unwrap().pending_ops().unwrap().is_empty());
    }

    #[tokio::test]
    async fn rename_folder_implicit_no_marker() {
        let backend = FakeBackend::default();
        // 마커 없이 하위 객체만 있는 묵시적 폴더.
        backend.put_file("src/f.txt", b"f");
        let store = store();

        rename(&backend, &store, "src/", "", "dst").await.unwrap();
        assert!(!backend.has("src/f.txt"));
        assert!(backend.has("dst/f.txt"));
        // dst/ 마커는 생기지 않는다(묵시적 폴더 그대로).
        assert!(!backend.has("dst/"));
    }

    #[tokio::test]
    async fn rename_folder_empty_marker_only() {
        let backend = FakeBackend::default();
        backend.put_dir("empty/");
        let store = store();
        let item = rename(&backend, &store, "empty/", "p/", "moved")
            .await
            .unwrap();
        assert_eq!(item.key, "p/moved/");
        assert!(!backend.has("empty/"));
        assert!(backend.has("p/moved/"));
    }

    #[tokio::test]
    async fn rename_folder_rejects_over_limit() {
        let backend = FakeBackend::default();
        backend.put_dir("big/");
        for i in 0..=MAX_FOLDER_RENAME {
            backend.put_file(&format!("big/f{i:04}.txt"), b"x");
        }
        let store = store();
        let err = rename(&backend, &store, "big/", "", "big2")
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Other(_)), "{err}");
        assert!(err.to_string().contains("1,000"), "{err}");
        // 아무것도 복사·삭제되지 않았다.
        assert!(backend.copies().is_empty());
        assert!(backend.deletes().is_empty());
        assert!(store.lock().unwrap().pending_ops().unwrap().is_empty());
    }

    #[tokio::test]
    async fn rename_folder_resume_after_failure() {
        let backend = FakeBackend::default();
        backend.put_dir("src/");
        // 진행 인덱스 갱신(50개마다)이 일어나도록 60개를 둔다.
        for i in 0..60 {
            backend.put_file(&format!("src/f{i:02}.txt"), b"x");
        }
        let store = store();

        // copy 55번째에서 실패 → 마커 + 54개 파일이 끝난 상태로 pending op이 남는다.
        backend.fail_copy_at(55);
        let err = rename(&backend, &store, "src/", "", "dst")
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Other(_)), "{err:?}");
        assert!(backend.has("src/f00.txt"), "원본이 지워지면 안 된다");
        let ops = store.lock().unwrap().pending_ops().unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].kind, "folder_rename");
        // done=50까지 payload에 기록돼 있다(마커 1 + 파일 49).
        let state = parse_folder_rename_payload(&ops[0].payload).unwrap();
        assert_eq!(state.done, 50);
        assert_eq!(state.keys.len(), 61);

        // 재시도: 진행 인덱스부터 이어서 완료된다.
        let resumed = resume_pending_ops(&backend, &store).await.unwrap();
        assert_eq!(resumed, 1);
        for i in 0..60 {
            assert!(
                !backend.has(&format!("src/f{i:02}.txt")),
                "src/f{i:02}.txt 남음"
            );
            assert!(
                backend.has(&format!("dst/f{i:02}.txt")),
                "dst/f{i:02}.txt 없음"
            );
        }
        assert!(!backend.has("src/"));
        assert!(backend.has("dst/"));
        assert!(store.lock().unwrap().pending_ops().unwrap().is_empty());
    }

    #[test]
    fn folder_rename_payload_roundtrip() {
        // 개행·'%'·'='·한글·이모지가 들어간 키도 왕복돼야 한다.
        let keys = vec![
            "src/".to_string(),
            "src/a\nb.txt".to_string(),
            "src/100%.txt".to_string(),
            "src/a=b.txt".to_string(),
            "src/한글 파일.txt".to_string(),
            "src/📷.jpg".to_string(),
        ];
        let payload = folder_rename_payload("src/", "dst/", &keys, 3);
        let state = parse_folder_rename_payload(&payload).unwrap();
        assert_eq!(state.from, "src/");
        assert_eq!(state.to, "dst/");
        assert_eq!(state.done, 3);
        assert_eq!(state.keys, keys);

        // done=0도 왕복된다.
        let payload = folder_rename_payload("src/", "dst/", &keys, 0);
        assert_eq!(parse_folder_rename_payload(&payload).unwrap().done, 0);

        // 손상된 payload는 Err.
        assert!(parse_folder_rename_payload("v=2\n").is_err());
        assert!(parse_folder_rename_payload("v=1\nfrom=%ZZ\n").is_err());
        assert!(parse_folder_rename_payload("v=1\ndone=9\nk=a\n").is_err());
    }

    #[tokio::test]
    async fn test_write_permission_denied_is_false() {
        let backend = FakeBackend::default();
        // 허용 → true, 테스트 객체는 남지 않는다.
        assert!(test_write(&backend).await.unwrap());
        assert!(backend.writes().len() == 1);
        // write 직후 delete됐는지: 객체가 없어야 한다.
        assert!(backend.state.lock().unwrap().objects.is_empty());

        // 권한 거부 → Ok(false).
        backend.deny_write();
        assert!(!test_write(&backend).await.unwrap());
    }
}
