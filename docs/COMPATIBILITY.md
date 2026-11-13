# S3 호환성 표

Pier 코어가 사용하는 S3 기능을 서비스별로 정리한 표다.
검증 도구는 `scripts/compat-test.sh`이고, 기능별로 `pier-core` 통합 테스트를 실행한다.

- **MinIO**: 로컬에서 실측한 값이다(`./scripts/compat-test.sh [--big]`).
- **AWS S3, Cloudflare R2, Wasabi**: 각 서비스 공식 문서 기준이다.
  **실측하지 않았다.** 직접 확인하려면 아래 "직접 검증하기"의 명령을 쓴다.
- 확실하지 않은 항목은 "확인 필요"로 표시한다.

## 직접 검증하기

```bash
# 로컬 MinIO (시드 후 실행)
docker compose -f docker/compose.yml up -d
./scripts/seed-minio.sh
./scripts/compat-test.sh          # --big를 붙이면 5GB 초과 복사까지

# AWS S3 / R2 / Wasabi — 키는 환경변수로만 넘긴다.
# 테스트 객체는 PIER_TEST_PREFIX(기본 pier-compat-test/) 아래에만 만들고
# 스크립트가 끝에 그 접두사만 삭제한다.
PIER_TEST_BUCKET=<버킷> PIER_TEST_REGION=<리전> \
PIER_TEST_ENDPOINT=<엔드포인트> \
PIER_TEST_ACCESS_KEY=<키> PIER_TEST_SECRET_KEY=<시크릿> \
./scripts/compat-test.sh
```

R2는 `PIER_TEST_REGION=auto`, `PIER_TEST_ENDPOINT=https://<account>.r2.cloudflarestorage.com`.
Wasabi는 `PIER_TEST_ENDPOINT=https://s3.<리전>.wasabisys.com` + 실제 리전.
자세한 옵션은 `scripts/compat-test.sh` 파일 맨 위 주석을 본다.

## 기능별 지원 여부

| 기능 | MinIO (실측) | AWS S3 (문서) | Cloudflare R2 (문서) | Wasabi (문서) |
|---|---|---|---|---|
| 목록·페이지네이션 (ListObjectsV2) | 통과 | 지원 | 지원 | 지원 |
| 범위 읽기·병렬 다운로드 (Range GET) | 통과 | 지원 | 지원 | 지원 |
| 멀티파트 업로드 | 통과 | 지원 | 지원¹ | 지원 |
| 이어올리기 (중단 후 파트 재개·abort) | 통과 | 지원 | 지원 | 지원 |
| 서버 측 복사 (CopyObject) | 통과 | 지원 | 지원 | 지원 |
| 5GB 초과 복사 (UploadPartCopy) | 통과 | 지원 | 지원 | 확인 필요² |
| 배치 삭제 (DeleteObjects) | 통과 | 지원(1,000/요청) | 지원³ | 지원 |
| 조건부 쓰기 (If-Match / If-None-Match) | 통과 | 지원(2024-08~, PutObject·CompleteMultipartUpload) | 지원 | 확인 필요⁴ |
| presign (SigV4 GET, ≤7일) | 통과 | 지원 | 지원 | 지원 |
| HEAD 스토리지 클래스 | 통과(STANDARD) | 지원 | 해당 없음⁵ | 해당 없음⁵ |

1. R2는 마지막을 제외한 모든 멀티파트 파트 크기가 정확히 같아야 한다.
   Pier는 파트 크기를 명시해 업로드하므로 구조상 충족한다(아래 "알려진 위험" 3번 참고).
2. Wasabi 공식 문서에 UploadPartCopy 지원 여부가 명확히 기재돼 있지 않다.
   실패하면 `conn.rename`이 에러를 그대로 올린다 — 다운로드+재업로드 폴백은 **미구현**.
3. opendal 문서는 "R2는 DeleteObjects 배치가 700을 넘으면 Internal Error"라고 경고한다.
   R2 공식 문서의 한계는 1,000이다. 어느 쪽이든 Pier 코드는 배치 크기를 별도로
   제한하지 않는다(아래 "알려진 위험" 2번).
4. Wasabi의 PutObject/Complete 시 If-Match·If-None-Match 지원 여부가 문서에
   명확하지 않다. OpenDAL capability가 항상 "지원"을 보고하므로 미지원이면
   Pier의 폴백이 발동하지 않는다(아래 "알려진 위험" 1번).
5. R2·Wasabi는 스토리지 클래스가 사실상 하나뿐이라 `x-amz-storage-class`
   헤더를 돌려주지 않을 수 있다. Pier는 헤더가 없으면 `STANDARD`로 표시한다
   (`s3_raw.rs` `head_storage_class`의 `unwrap_or("STANDARD")`).

## Pier의 대체 경로 (코드에 실제 있는 것만)

| 상황 | 대체 경로 | 위치 |
|---|---|---|
| 백엔드의 조건부 쓰기 헤더 지원 여부와 무관 | 쓰기 직전 `stat`(HEAD)으로 존재 여부·ETag를 항상 비교해 Conflict 판정 + 조건부 헤더도 함께 전송 | `opendal_s3.rs` `condition_options`, `s3_raw.rs` `check_write_condition` |
| 폴더 마커 생성(`create_dir`) 미지원 | OpenDAL SimulateLayer가 0바이트 `키/` 객체 쓰기로 흉내 | `opendal_s3.rs` `create_dir` 주석 |
| `UNSIGNED-PAYLOAD` 미지원 서비스 | 모든 직접 서명 요청에 실제 본문 SHA-256(`x-amz-content-sha256`)을 항상 전송 | `s3_raw.rs` `send` |
| 점(`.`) 포함 버킷 + 가상 호스트 스타일 | TLS 와일드카드 불일치를 피해 자동으로 path-style 유지 | `opendal_s3.rs` `use_virtual_host_style`, `s3_raw.rs` `new` |
| `x-amz-storage-class` 헤더 없음 | `STANDARD`로 간주 | `s3_raw.rs` `head_storage_class` |

아래는 코드에 **없는** 경로다(미지원 시 에러가 그대로 올라간다):

- 배치 삭제(DeleteObjects) 미지원 시 개별 삭제 폴백 — **미구현**
- `stat` 기반 ETag 비교가 불가한 경우(HEAD 자체 미지원)의 조건부 쓰기 — **미구현**
- UploadPartCopy 미지원 시 다운로드 후 재업로드 — **미구현**

## 알려진 위험 (R2·Wasabi에서 깨질 수 있는 코드 지점)

코드 수정 없이 기록만 한다. 실측하지 않았으므로 사용 전 `compat-test.sh`로 확인한다.

1. **조건부 쓰기 폴백이 발동하지 않을 수 있다 — 완화됨.**
   opendal-service-s3 0.59.5는 `write_with_if_match /
   write_with_if_not_exists` capability를 모든 S3 계열 서비스에 `true`로
   하드코딩한다(`backend.rs`). capability를 신뢰할 수 없으므로 Pier는
   조건부 업로드일 때 **항상 업로드 직전 HEAD 비교를 수행**한다
   (`opendal_s3.rs` `condition_options`, `s3_raw.rs`
   `check_write_condition` — `upload_resumable`의 Create 직전과
   Complete 직전). 헤더를 조용히 무시하는 백엔드에서도 충돌을 감지한다.
   남은 틈은 HEAD~쓰기(또는 Complete) 사이의 경쟁 구간인데, 조건부 헤더는
   계속 함께 보내므로 지원하는 백엔드에서는 서버가 그 구간까지 막는다.
   헤더를 지원하지 않는 백엔드에서는 그 경쟁 구간이 열려 있다 —
   완전한 원자성 보장은 백엔드 지원 여부에 달렸다(문서 기준, 실측 필요).

2. **R2 DeleteObjects 배치 크기.**
   opendal `compatible_services.md`는 "R2는 배치가 700을 넘으면 Internal
   Error"라고 경고하지만, `delete_max_size` 빌더 옵션은 0.57에서 폐기됐고
   capability는 항상 1,000이다(`opendal-service-s3/src/backend.rs`
   `DEFAULT_BATCH_MAX_OPERATIONS = 1000`). Pier의 `delete_keys`는 OpenDAL
   `delete_iter`에 위임하므로 배치 크기를 조절할 수 없다. R2 공식 문서의
   한계는 1,000이라 실제로는 문제없을 가능성이 높지만, 만약 실패하면
   폴백이 없다 — **확인 필요**.

3. **R2 멀티파트 파트 크기 동일 요구.**
   R2는 마지막을 제외한 파트 크기가 정확히 같아야 한다(어기면
   `All non-trailing parts must have the same length`).
   `put_file`은 멀티파트 시 `WriteOptions.chunk`를 명시해 OpenDAL의
   exact 버퍼를 켜므로 파트 크기가 균일하다(`opendal_s3.rs` `wopts.chunk =
   Some(opts.part_size)`). `upload_resumable`(s3_raw)도 고정 chunk_size로
   파트를 만든다. 다만 `write_bytes`는 `chunk`를 지정하지 않아 OpenDAL이
   임의 크기로 파트를 나눌 수 있다 — 현재 대용량 경로는 `put_file`과
   `upload_resumable`만 쓰므로 영향이 작다 — **확인 필요**.

4. **R2 region은 `auto`여야 한다.**
   R2는 서명 리전으로 `auto`를 요구한다. `S3Raw::new`는
   `RequestSigner::new("s3", &config.region)`에 설정 리전을 그대로 넣고
   (`s3_raw.rs`), `OpendalS3::new`도 `.region(&config.region)`을 그대로 쓴다
   (`opendal_s3.rs`). R2 프리셋이 `auto`를 채우는지, 그리고 `auto`로 서명한
   요청이 실제로 통과하는지 — **확인 필요**.

5. **`x-amz-content-sha256` 항상 전송.**
   `S3Raw::send`는 모든 직접 서명 요청에 실제 본문 해시를 헤더로 넣는다.
   `UNSIGNED-PAYLOAD`만 받는 서비스가 있을 수 있어 넣은 장치인데, 반대로
   이 헤더를 검증해 거절하는 서비스는 알려진 바 없다 — 위험 낮음.

6. **가상 호스트 스타일.**
   `path_style=false`이고 버킷명에 점이 없으면 가상 호스트 방식을 쓴다
   (`use_virtual_host_style`, `s3_raw.rs`의 `virtual_host`). AWS·R2·Wasabi
   모두 가상 호스트 방식을 지원하지만 Wasabi는 path-style을 권장한다.
   프리셋에서 `path_style` 기본값을 확인한다 — 위험 낮음.

7. **R2의 일부 체크섬·응답 헤더.**
   OpenDAL은 `checksum_algorithm`을 설정할 때만 `x-amz-checksum-*` 헤더를
   보내는데, Pier는 이를 설정하지 않으므로 체크섬 헤더는 나가지 않는다.
   opendal 문서상 R2가 지원하지 않는 `stat_with_override_*`(응답 헤더
   재정의 쿼리)도 Pier는 사용하지 않는다 — 위험 낮음.

## MinIO 실측 결과

`./scripts/compat-test.sh --big` (MinIO RELEASE.2025-10-15, docker compose):

```text
===== 호환성 결과 =====
기능                       결과 비고
----                         ----   ----
연결·인증               통과 2개 테스트
목록·페이지네이션   통과 6개 테스트
범위 읽기·병렬 다운로드 통과 4개 테스트
멀티파트 업로드       통과 5개 테스트
이어올리기              통과 6개 테스트
서버 측 복사            통과 4개 테스트
배치 삭제                통과 2개 테스트
조건부 쓰기             통과 3개 테스트
presign                      통과 2개 테스트
HEAD 스토리지 클래스  통과 1개 테스트
5GB 초과 복사            통과 2개 테스트

통과 11개 / 실패 0개 / 건너뜀 0개 기능
```

`--big` 없이 실행하면 "5GB 초과 복사"는 건너뛰고 나머지 10개 기능이 통과한다.

## 테스트 대상 지정 환경변수

통합 테스트 공용 헬퍼(`crates/pier-core/tests/common/mod.rs`)가 읽는 변수:

| 변수 | 기본값 | 설명 |
|---|---|---|
| `PIER_TEST_MINIO` | — | `1`이면 로컬 MinIO(127.0.0.1:9000, pier-test)로 실행 |
| `PIER_TEST_BUCKET` | `pier-test` | 설정하면 원격 모드 |
| `PIER_TEST_ENDPOINT` | MinIO: `http://127.0.0.1:9000`, 원격: AWS 기본 | 엔드포인트 URL |
| `PIER_TEST_REGION` | `us-east-1` | 서명 리전 (R2는 `auto`) |
| `PIER_TEST_ACCESS_KEY` | `minioadmin` | 액세스 키 |
| `PIER_TEST_SECRET_KEY` | `minioadmin` | 시크릿 키 |
| `PIER_TEST_PATH_STYLE` | MinIO: true, 원격: false | `1`/`true`/`yes`면 path-style |
| `PIER_TEST_PREFIX` | `pier-compat-test/` | 원격 테스트 객체 접두사(끝 `/` 자동 보정) |
| `PIER_TEST_BIG` | — | `1`이면 5GB급 테스트 활성(`--big`) |

루트 시드 데이터(`seed-minio.sh`의 hello.txt, empty-folder/ 등)에 의존하는
테스트는 원격 대상에서 자동으로 건너뛴다(`local_minio()` 게이트).
원격 모드에서는 모든 테스트 객체가 `PIER_TEST_PREFIX` 아래에만 만들어지고,
`compat-test.sh`가 끝에 그 접두사만 삭제한다.
