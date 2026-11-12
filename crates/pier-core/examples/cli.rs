use pier_core::backend::opendal_s3::OpendalS3;
use pier_core::config::{ConnectionConfig, Provider};

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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("ls") => {
            let secret = env_or("PIER_SECRET_KEY", "minioadmin");
            let backend = OpendalS3::new(&minio_config(), &secret)?;
            let entries = backend.list_root().await?;
            for entry in &entries {
                let kind = if entry.is_dir { "dir " } else { "file" };
                println!("{kind} {:>12}  {}", entry.size, entry.key);
            }
            eprintln!("{}개 항목", entries.len());
            Ok(())
        }
        _ => {
            eprintln!("사용법: cli ls");
            std::process::exit(2);
        }
    }
}
