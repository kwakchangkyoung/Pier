use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use pier_core::config::{ConnectionConfig, Provider};
use pier_core::enumerate::Connection;
use pier_core::transfer::{self, CancelToken, ProgressSink, TransferOptions, UploadMode};
use pier_core::{Backend, Item, OpendalS3, S3Raw};

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn minio_config() -> ConnectionConfig {
    ConnectionConfig {
        id: String::new(),
        display_name: env_or("PIER_BUCKET", "pier-test"),
        provider: Provider::Custom,
        endpoint: Some(env_or("PIER_ENDPOINT", "http://127.0.0.1:9000")),
        region: env_or("PIER_REGION", "us-east-1"),
        bucket: env_or("PIER_BUCKET", "pier-test"),
        prefix: None,
        path_style: env_or("PIER_PATH_STYLE", "true") == "true",
        access_key_id: env_or("PIER_ACCESS_KEY", "minioadmin"),
        chunk_size_mb: None,
        concurrency: None,
    }
}

fn cache_dir() -> PathBuf {
    std::env::var_os("PIER_CACHE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/pier-cli-cache"))
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// 퍼센트가 바뀔 때만 진행률을 찍는 CLI용 싱크.
#[derive(Default)]
struct CliProgress(AtomicU64);

impl ProgressSink for CliProgress {
    fn on_progress(&self, transferred: u64, total: u64) {
        let pct = transferred
            .saturating_mul(100)
            .checked_div(total)
            .unwrap_or(100);
        let last = self.0.load(Ordering::Relaxed);
        if pct != last {
            self.0.store(pct, Ordering::Relaxed);
            eprintln!("진행률 {pct}% ({transferred}/{total} bytes)");
        }
    }
}

fn transfer_opts() -> TransferOptions {
    TransferOptions {
        chunk_size: env_u64("PIER_CHUNK_MB", 8).saturating_mul(1024 * 1024),
        concurrency: env_u64("PIER_CONCURRENCY", 8) as usize,
    }
}

/// `--keep-both`/`--replace` 플래그를 UploadMode로 변환한다.
fn upload_mode(args: &[String]) -> UploadMode {
    if args.iter().any(|a| a == "--keep-both") {
        UploadMode::KeepBoth
    } else if args.iter().any(|a| a == "--replace") {
        UploadMode::Replace { if_match: None }
    } else {
        UploadMode::Fail
    }
}

fn print_item(item: &Item) {
    let kind = if item.is_dir { "dir " } else { "file" };
    println!("{kind} {:>12}  {}", item.size, item.key);
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let secret = env_or("PIER_SECRET_KEY", "minioadmin");
    let conn = Connection::open(&minio_config(), &secret, &cache_dir())?;
    match args.get(1).map(String::as_str) {
        Some("ls") => {
            let prefix = args.get(2).map(String::as_str).unwrap_or("");
            let mut cursor = None;
            let mut count = 0usize;
            // 첫 페이지의 from_cache가 이번 목록이 캐시를 썼는지 나타낸다.
            let mut from_cache = false;
            loop {
                let page = conn.list_folder(prefix, cursor.as_deref()).await?;
                if count == 0 {
                    from_cache = page.from_cache;
                }
                for item in &page.items {
                    print_item(item);
                }
                count += page.items.len();
                match page.next_cursor {
                    Some(next) => cursor = Some(next),
                    None => break,
                }
            }
            let source = if from_cache { "cache" } else { "remote" };
            eprintln!("{count}개 항목 ({source})");
            Ok(())
        }
        Some("refresh") => {
            let prefix = args.get(2).map(String::as_str).unwrap_or("");
            let changes = conn.refresh(prefix).await?;
            for item in &changes.upserted {
                print_item(item);
            }
            for key in &changes.removed {
                println!("removed {key}");
            }
            eprintln!(
                "upserted {}개, removed {}개",
                changes.upserted.len(),
                changes.removed.len()
            );
            Ok(())
        }
        Some("stat") => {
            let key = args.get(2).ok_or("사용법: cli stat <key>")?;
            let item = conn.stat(key).await?;
            println!("key: {}", item.key);
            println!("parent: {}", item.parent);
            println!("name: {}", item.name);
            println!("is_dir: {}", item.is_dir);
            println!("size: {}", item.size);
            println!("etag: {:?}", item.etag);
            println!("last_modified_ms: {:?}", item.last_modified_ms);
            Ok(())
        }
        Some("info") => {
            let key = args.get(2).ok_or("사용법: cli info <key>")?;
            let item = conn.stat(key).await?;
            // 스토리지 클래스는 목록에 안 나오므로 HEAD로 따로 가져온다.
            let raw = S3Raw::new(&minio_config(), &secret)?;
            let storage_class = raw.head_storage_class(key).await?;
            println!("key: {}", item.key);
            println!("size: {}", item.size);
            println!("etag: {:?}", item.etag);
            println!("storage_class: {storage_class}");
            println!("last_modified_ms: {:?}", item.last_modified_ms);
            Ok(())
        }
        Some("presign") => {
            let key = args.get(2).ok_or("사용법: cli presign <key> [초]")?;
            let secs = match args.get(3) {
                Some(s) => s.parse::<u64>().map_err(|_| "만료 초는 숫자여야 한다")?,
                None => 3600,
            };
            // URL은 사용자가 요청한 출력이라 stdout에 찍는다 — 로그에는
            // 남기지 않는다(SPEC 4.6).
            let url = conn
                .backend()
                .presign_read(key, Duration::from_secs(secs))
                .await?;
            println!("{url}");
            Ok(())
        }
        Some("get") => {
            let key = args.get(2).ok_or("사용법: cli get <key> <dest>")?;
            let dest = args.get(3).ok_or("사용법: cli get <key> <dest>")?;
            // 다운로드는 캐시를 쓰지 않으므로 백엔드를 직접 연다.
            let backend = OpendalS3::new(&minio_config(), &secret)?;
            let opts = transfer_opts();
            let progress = CliProgress::default();
            let start = std::time::Instant::now();
            let written = transfer::download(
                &backend,
                key,
                &PathBuf::from(dest),
                &opts,
                &progress,
                &CancelToken::new(),
            )
            .await?;
            let secs = start.elapsed().as_secs_f64();
            let mb = written as f64 / (1024.0 * 1024.0);
            eprintln!(
                "{written} bytes -> {dest} ({secs:.2}s, {:.1} MB/s)",
                mb / secs
            );
            Ok(())
        }
        Some("get-dir") => {
            let prefix = args
                .get(2)
                .ok_or("사용법: cli get-dir <prefix> <dest_dir>")?;
            let dest = args
                .get(3)
                .ok_or("사용법: cli get-dir <prefix> <dest_dir>")?;
            // 폴더 다운로드도 캐시를 쓰지 않으므로 백엔드를 직접 연다.
            let backend = OpendalS3::new(&minio_config(), &secret)?;
            let opts = transfer_opts();
            let progress = CliProgress::default();
            let start = std::time::Instant::now();
            let result = transfer::download_folder(
                &backend,
                prefix,
                &PathBuf::from(dest),
                &opts,
                &progress,
                &CancelToken::new(),
            )
            .await?;
            let secs = start.elapsed().as_secs_f64();
            let mb = result.total_bytes as f64 / (1024.0 * 1024.0);
            eprintln!(
                "{}개 파일, {} bytes -> {} ({secs:.2}s, {:.1} MB/s)",
                result.file_count,
                result.total_bytes,
                result.local_path.display(),
                mb / secs
            );
            Ok(())
        }
        Some("put") => {
            let src = args
                .get(2)
                .ok_or("사용법: cli put <src> <parent> [--keep-both|--replace]")?;
            let parent = args
                .get(3)
                .ok_or("사용법: cli put <src> <parent> [--keep-both|--replace]")?;
            let src_path = PathBuf::from(src);
            let name = src_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .filter(|n| !n.is_empty())
                .ok_or("파일 이름을 알 수 없다")?;
            if src_path.is_dir() {
                return Err("put은 파일만 받는다. 폴더는 put-dir를 쓴다".into());
            }
            // 업로드도 캐시를 쓰지 않으므로 백엔드를 직접 연다.
            let backend = OpendalS3::new(&minio_config(), &secret)?;
            let opts = transfer_opts();
            let mode = upload_mode(&args);
            let progress = CliProgress::default();
            let start = std::time::Instant::now();
            let item = transfer::upload(
                &backend,
                &src_path,
                parent,
                &name,
                &mode,
                &opts,
                &progress,
                &CancelToken::new(),
            )
            .await?;
            let secs = start.elapsed().as_secs_f64();
            let mb = item.size as f64 / (1024.0 * 1024.0);
            eprintln!(
                "{} bytes -> {} ({secs:.2}s, {:.1} MB/s)",
                item.size,
                item.key,
                mb / secs
            );
            Ok(())
        }
        Some("put-dir") => {
            let src = args
                .get(2)
                .ok_or("사용법: cli put-dir <src_dir> <parent> [--keep-both|--replace]")?;
            let parent = args
                .get(3)
                .ok_or("사용법: cli put-dir <src_dir> <parent> [--keep-both|--replace]")?;
            let src_path = PathBuf::from(src);
            if !src_path.is_dir() {
                return Err("put-dir은 디렉터리만 받는다".into());
            }
            let backend = OpendalS3::new(&minio_config(), &secret)?;
            let opts = transfer_opts();
            let mode = upload_mode(&args);
            let progress = CliProgress::default();
            let start = std::time::Instant::now();
            let result = transfer::upload_folder(
                &backend,
                &src_path,
                parent,
                &mode,
                &opts,
                &progress,
                &CancelToken::new(),
            )
            .await?;
            let secs = start.elapsed().as_secs_f64();
            let mb = result.total_bytes as f64 / (1024.0 * 1024.0);
            eprintln!(
                "{}개 업로드, {}개 제외, {}개 건너뜀, {} bytes ({secs:.2}s, {:.1} MB/s)",
                result.uploaded,
                result.ignored,
                result.skipped,
                result.total_bytes,
                mb / secs
            );
            Ok(())
        }
        Some("mkdir") => {
            let parent = args.get(2).ok_or("사용법: cli mkdir <parent> <name>")?;
            let name = args.get(3).ok_or("사용법: cli mkdir <parent> <name>")?;
            let item = conn.create_folder(parent, name).await?;
            print_item(&item);
            Ok(())
        }
        Some("mv") => {
            let key = args
                .get(2)
                .ok_or("사용법: cli mv <key> <new_parent> <new_name>")?;
            let new_parent = args
                .get(3)
                .ok_or("사용법: cli mv <key> <new_parent> <new_name>")?;
            let new_name = args
                .get(4)
                .ok_or("사용법: cli mv <key> <new_parent> <new_name>")?;
            let item = conn.rename(key, new_parent, new_name).await?;
            print_item(&item);
            Ok(())
        }
        Some("rm") => {
            let key = args.get(2).ok_or("사용법: cli rm <key>")?;
            conn.delete(key).await?;
            println!("deleted {key}");
            Ok(())
        }
        _ => {
            eprintln!("사용법: cli ls [폴더] | cli refresh [폴더] | cli stat <key> | cli info <key> | cli presign <key> [초(기본 3600)] | cli get <key> <dest> | cli get-dir <prefix> <dest_dir> | cli put <src> <parent> [--keep-both|--replace] | cli put-dir <src_dir> <parent> [--keep-both|--replace] | cli mkdir <parent> <name> | cli mv <key> <new_parent> <new_name> | cli rm <key>");
            std::process::exit(2);
        }
    }
}
