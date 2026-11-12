use pier_core::backend::opendal_s3::OpendalS3;
use pier_core::config::{ConnectionConfig, Provider};

fn minio_config() -> ConnectionConfig {
    ConnectionConfig {
        id: String::new(),
        display_name: "pier-test".to_string(),
        provider: Provider::MinIO,
        endpoint: Some("http://127.0.0.1:9000".to_string()),
        region: "us-east-1".to_string(),
        bucket: "pier-test".to_string(),
        prefix: None,
        path_style: true,
        access_key_id: "minioadmin".to_string(),
    }
}

#[tokio::test]
async fn list_root_minio() {
    if std::env::var("PIER_TEST_MINIO").as_deref() != Ok("1") {
        return;
    }

    let backend = OpendalS3::new(&minio_config(), "minioadmin").expect("연결 생성 실패");
    let entries = backend.list_root().await.expect("루트 목록 조회 실패");

    for entry in &entries {
        assert!(!entry.name.is_empty(), "name이 비어 있음: {entry:?}");
        if entry.is_dir {
            assert!(
                entry.key.ends_with('/'),
                "폴더 key에 '/' 없음: {}",
                entry.key
            );
        }
    }
    eprintln!("MinIO 루트 항목 {}개", entries.len());
}
