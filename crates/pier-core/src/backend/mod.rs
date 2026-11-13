pub mod opendal_s3;

use std::future::Future;
use std::path::Path;
use std::time::Duration;

use crate::error::Error;
use crate::item::Item;
use crate::transfer::{CancelToken, ProgressSink};

/// put_file의 조건부 쓰기 조건 (SPEC 3.4).
#[derive(Debug, Clone)]
pub enum WriteCondition {
    /// 조건 없음 — 대상이 있으면 덮어쓴다.
    Any,
    /// 대상이 없어야 한다(If-None-Match: *). 이미 있으면 Error::Conflict.
    NotExists,
    /// 대상 ETag가 이 값과 같아야 한다(If-Match). 다르거나 없으면
    /// Error::Conflict. 조건부 헤더 지원 여부와 무관하게 쓰기 직전
    /// stat으로 ETag를 항상 비교한다.
    IfMatch(String),
}

/// put_file 옵션.
#[derive(Debug, Clone)]
pub struct PutOptions {
    /// 조건부 쓰기 조건.
    pub condition: WriteCondition,
    /// 멀티파트 파트 크기(바이트). 파일이 이 크기 이하면 단일 PUT,
    /// 초과하면 이 크기의 파트로 나눠 업로드한다. 최소 16MB 이상이어야 한다.
    pub part_size: u64,
    /// 멀티파트 동시 파트 업로드 수 상한.
    pub concurrency: usize,
}

/// S3 호환 백엔드 공통 인터페이스.
/// 경로·키는 모두 연결 루트 기준 상대 키이며, 폴더는 끝에 '/'가 붙는다.
#[allow(async_fn_in_trait)]
pub trait Backend {
    /// prefix(폴더 키, 루트는 "") 바로 아래 전체 항목을 키의 UTF-8 바이트
    /// 오름차순으로 반환한다(비재귀). 자기 자신 마커는 제외하고, 같은 키가
    /// 중복으로 와도 하나로 병합한다(SPEC 3.2).
    ///
    /// 반환 future는 Send여야 한다 — enumerate::prefetch가 여러 폴더의
    /// 목록 요청을 동시 태스크로 돌린다.
    fn list_folder<'a>(
        &'a self,
        prefix: &'a str,
    ) -> impl Future<Output = Result<Vec<Item>, Error>> + Send + 'a;
    /// prefix 아래 모든 항목을 재귀적으로 반환한다(폴더 다운로드용).
    /// 폴더 마커(끝 '/')도 is_dir 항목으로 포함하고, 결과는 키 정렬이다.
    fn list_recursive<'a>(
        &'a self,
        prefix: &'a str,
    ) -> impl Future<Output = Result<Vec<Item>, Error>> + Send + 'a;
    /// 단일 아이템 메타데이터. 없으면 Error::NotFound.
    fn stat<'a>(&'a self, key: &'a str) -> impl Future<Output = Result<Item, Error>> + Send + 'a;
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

    /// 빈 폴더 마커 객체(`key`는 '/'로 끝남) 생성. OpenDAL SimulateLayer의
    /// create_dir 사용. 이미 있으면 성공으로 본다.
    fn create_dir<'a>(
        &'a self,
        key: &'a str,
    ) -> impl Future<Output = Result<(), Error>> + Send + 'a {
        let _ = key;
        async move {
            Err(Error::Other(
                "이 백엔드는 폴더 생성을 지원하지 않는다".to_string(),
            ))
        }
    }

    /// 서버 측 복사. `to`는 덮어쓴다. 5GB 초과는 백엔드가 멀티파트
    /// 복사로 처리한다. `from`이 없으면 Error::NotFound.
    fn copy<'a>(
        &'a self,
        from: &'a str,
        to: &'a str,
    ) -> impl Future<Output = Result<(), Error>> + Send + 'a {
        let _ = (from, to);
        async move {
            Err(Error::Other(
                "이 백엔드는 서버 측 복사를 지원하지 않는다".to_string(),
            ))
        }
    }

    /// 여러 키 삭제. 요청당 최대 1,000개씩 배치한다. 없는 키는 성공으로
    /// 간주한다(S3 DeleteObjects 의미와 같다).
    fn delete_keys<'a>(
        &'a self,
        keys: &'a [String],
    ) -> impl Future<Output = Result<(), Error>> + Send + 'a {
        let _ = keys;
        async move {
            Err(Error::Other(
                "이 백엔드는 삭제를 지원하지 않는다".to_string(),
            ))
        }
    }

    /// 작은 바이트 객체 쓰기(쓰기 권한 확인 등). `key`는 파일 키여야 한다.
    fn write_bytes<'a>(
        &'a self,
        key: &'a str,
        data: Vec<u8>,
    ) -> impl Future<Output = Result<(), Error>> + Send + 'a {
        let _ = (key, data);
        async move {
            Err(Error::Other(
                "이 백엔드는 쓰기를 지원하지 않는다".to_string(),
            ))
        }
    }

    /// 로컬 파일 src를 key로 업로드한다.
    ///
    /// - 크기가 `opts.part_size` 이하면 단일 PUT, 초과하면
    ///   `opts.part_size` 파트의 멀티파트 업로드를 한다.
    /// - `opts.condition` 불일치는 Error::Conflict다.
    /// - 진행률은 업로드가 수락된 바이트 누적으로 호출한다(파트 단위).
    /// - 실패·취소 시 미완료 멀티파트를 abort해 원격에 객체를 남기지
    ///   않는다. 호출 측 future가 drop돼도 abort가 보장된다.
    fn put_file<'a>(
        &'a self,
        key: &'a str,
        src: &'a Path,
        opts: PutOptions,
        progress: &'a dyn ProgressSink,
        cancel: &'a CancelToken,
    ) -> impl Future<Output = Result<(), Error>> + Send + 'a {
        let _ = (key, src, opts, progress, cancel);
        async move {
            Err(Error::Other(
                "이 백엔드는 업로드를 지원하지 않는다".to_string(),
            ))
        }
    }

    /// key를 GET할 수 있는 presigned URL을 반환한다.
    /// 만료는 1초~7일(604,800초)이어야 한다.
    /// 반환된 URL 전체(서명 포함)는 로그에 남기지 않는다(SPEC 4.6).
    fn presign_read<'a>(
        &'a self,
        key: &'a str,
        expires: Duration,
    ) -> impl Future<Output = Result<String, Error>> + Send + 'a {
        let _ = (key, expires);
        async move {
            Err(Error::Other(
                "이 백엔드는 presign을 지원하지 않는다".to_string(),
            ))
        }
    }
}
