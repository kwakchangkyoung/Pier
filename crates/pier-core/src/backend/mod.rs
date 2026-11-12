pub mod opendal_s3;

use std::time::SystemTime;

/// 목록/메타데이터에 쓰는 최소 아이템 표현.
/// key는 루트 기준 상대 S3 키이며, 폴더는 끝에 '/'가 붙는다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub etag: Option<String>,
    pub last_modified: Option<SystemTime>,
}
