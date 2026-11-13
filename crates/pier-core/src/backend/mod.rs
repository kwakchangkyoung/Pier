pub mod opendal_s3;

use std::future::Future;

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
    /// prefix 아래 모든 항목을 재귀적으로 반환한다(폴더 다운로드용).
    /// 폴더 마커(끝 '/')도 is_dir 항목으로 포함하고, 결과는 키 정렬이다.
    async fn list_recursive(&self, prefix: &str) -> Result<Vec<Item>, Error>;
    /// 단일 아이템 메타데이터. 없으면 Error::NotFound.
    async fn stat(&self, key: &str) -> Result<Item, Error>;
    /// key의 [offset, offset+len) 범위를 읽어 Vec<u8>로 반환한다.
    /// 병렬 다운로드(transfer.rs)가 청크별로 호출한다.
    ///
    /// impl은 `async fn`으로 써도 된다. 다만 transfer가 여러 청크를
    /// 동시에 돌리려면 반환 future가 Send여야 하므로 시그니처에
    /// `+ Send`를 명시한다.
    fn read_range<'a>(
        &'a self,
        key: &'a str,
        offset: u64,
        len: u64,
    ) -> impl Future<Output = Result<Vec<u8>, Error>> + Send + 'a {
        let _ = (key, offset, len);
        async move {
            Err(Error::Other(
                "이 백엔드는 범위 읽기를 지원하지 않는다".to_string(),
            ))
        }
    }
}
