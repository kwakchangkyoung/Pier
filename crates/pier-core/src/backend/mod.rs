pub mod opendal_s3;

use std::path::Path;

use crate::error::Error;
use crate::item::Item;

/// S3 호환 백엔드 공통 인터페이스.
/// 경로·키는 모두 연결 루트 기준 상대 키이며, 폴더는 끝에 '/'가 붙는다.
#[allow(async_fn_in_trait)]
pub trait Backend {
    /// prefix(폴더 키, 루트는 "") 바로 아래 전체 항목을 키의 UTF-8 바이트
    /// 오름차순으로 반환한다(비재귀). 자기 자신 마커는 제외하고, 같은 키가
    /// 중복으로 와도 하나로 병합한다(SPEC 3.2).
    async fn list_folder(&self, prefix: &str) -> Result<Vec<Item>, Error>;
    /// 단일 아이템 메타데이터. 없으면 Error::NotFound.
    async fn stat(&self, key: &str) -> Result<Item, Error>;
    /// 객체를 dest 파일로 스트리밍 저장하고 크기를 검증해 쓴 바이트 수를 반환한다.
    /// 실패 시 부분 파일을 삭제한다.
    async fn read_to_file(&self, key: &str, dest: &Path) -> Result<u64, Error>;
}
