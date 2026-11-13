use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use pier_core::config::{ConnectionConfig, Provider};
use pier_core::enumerate::Connection;
use pier_core::transfer::{self, CancelToken, ProgressSink, TransferOptions};
use pier_core::{Item, OpendalS3};

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
        Some("get") => {
            let key = args.get(2).ok_or("사용법: cli get <key> <dest>")?;
            let dest = args.get(3).ok_or("사용법: cli get <key> <dest>")?;
            // 다운로드는 캐시를 쓰지 않으므로 백엔드를 직접 연다.
            let backend = OpendalS3::new(&minio_config(), &secret)?;
            let opts = TransferOptions {
                chunk_size: env_u64("PIER_CHUNK_MB", 8).saturating_mul(1024 * 1024),
                concurrency: env_u64("PIER_CONCURRENCY", 8) as usize,
            };
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
            let opts = TransferOptions {
                chunk_size: env_u64("PIER_CHUNK_MB", 8).saturating_mul(1024 * 1024),
                concurrency: env_u64("PIER_CONCURRENCY", 8) as usize,
            };
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
        _ => {
            eprintln!("사용법: cli ls [폴더] | cli refresh [폴더] | cli stat <key> | cli get <key> <dest> | cli get-dir <prefix> <dest_dir>");
            std::process::exit(2);
        }
    }
}
