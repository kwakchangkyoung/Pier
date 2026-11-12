use std::path::PathBuf;

use pier_core::config::{ConnectionConfig, Provider};
use pier_core::enumerate::Connection;
use pier_core::Item;

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
            let written = conn.download(key, &PathBuf::from(dest)).await?;
            eprintln!("{written} bytes -> {dest}");
            Ok(())
        }
        _ => {
            eprintln!("사용법: cli ls [폴더] | cli stat <key> | cli get <key> <dest>");
            std::process::exit(2);
        }
    }
}
