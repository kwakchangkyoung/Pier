# Pier 작업지시서 (SPEC)

## 1. 목표

S3 호환 버킷을 탐색하고 파일을 주고받는 macOS 독립 앱을 만든다. 일반 창 앱(Dock에 표시)과 메뉴바 아이콘을 함께 제공한다. Cyberduck, Transmit 같은 GUI 클라이언트, CloudMounter/Mountain Duck 같은 마운트 앱, rclone 같은 CLI와 비교해 다음으로 차별화한다.

- **메인 창**: 사이드바(연결 목록) + 파일 테이블(이름, 크기, 수정 시각) + 경로 표시줄(breadcrumb) + 툴바(새로고침, 업로드, 새 폴더, 다운로드).
- **메뉴바**: 진행 중인 전송과 속도, 연결 목록에서 창 열기, 현재 폴더 새로고침, 설정 열기.
- **무료·오픈소스**이면서 **설정 없이 빠른** 앱: 서비스별 최적 전송 설정을 프리셋으로 기본 제공한다.
- **체감 속도**: 한 번 열어본 폴더는 캐시에서 즉시 표시하고 백그라운드에서 갱신한다. 대용량 파일은 병렬 범위 다운로드·멀티파트 업로드로 대역폭을 채운다.
- **S3 사용자를 위한 편의 기능**: presigned URL 복사, 객체 정보(스토리지 클래스, ETag) 확인, 서비스 프리셋.
- **네이티브 macOS 경험**: Finder와 양방향 드래그 앤 드롭, Quick Look 미리보기.

### 성능 목표

| 항목 | 목표 |
|---|---|
| 캐시된 폴더를 앱에서 열기 | 100ms 이내 표시 |
| 객체 1,000개 폴더 최초 열기 | 첫 페이지 1초 이내 (같은 리전 기준) |
| 대용량 파일(1GB 이상) 다운로드·업로드 | `aws s3 cp` 대비 처리량 90% 이상 |
| 유휴 상태 CPU | 거의 0 (자동 폴링 없음) |

### MVP 범위 밖

S3 외 백엔드(OneDrive, Google Drive 등), 버전 관리 UI, 스토리지 클래스 변경, 클라이언트 측 암호화, 버킷 생성/삭제, Finder 마운트/File Provider 통합(이후 후보로 이동), App Store 배포, App Sandbox, Intel 맥. 단, 코어 구조는 나중에 OpenDAL의 다른 백엔드를 붙일 수 있게 설계한다.

## 2. 프로젝트 구조

```
pier/
├── CLAUDE.md
├── docs/SPEC.md
├── Cargo.toml                    # workspace: crates/pier-core, crates/pier-ffi
├── crates/
│   ├── pier-core/                # 순수 Rust, Apple 의존성 없음
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── error.rs
│   │   │   ├── config.rs         # 연결 설정, 서비스 프리셋
│   │   │   ├── key.rs            # S3 키 ↔ 경로/부모/이름 변환 유틸
│   │   │   ├── item.rs           # Item 모델, 버전 계산
│   │   │   ├── backend/
│   │   │   │   ├── mod.rs        # Backend trait
│   │   │   │   ├── opendal_s3.rs # 데이터 경로 (list/stat/read/write/copy/delete/presign)
│   │   │   │   └── s3_admin.rs   # aws-sdk-s3 기반 S3 전용 기능
│   │   │   ├── metadata.rs       # SQLite 메타데이터 캐시
│   │   │   ├── enumerate.rs      # 폴더 목록, 페이지네이션, 백그라운드 갱신
│   │   │   ├── transfer.rs       # 병렬 다운로드, 멀티파트 업로드, 진행률
│   │   │   └── ignore.rs         # 폴더 업로드 제외 규칙 (.DS_Store 등)
│   │   ├── examples/cli.rs       # ls/stat/get/put/mv/rm/presign 수동 검증용
│   │   └── tests/                # MinIO 통합 테스트
│   └── pier-ffi/                 # UniFFI 래퍼 (staticlib)
│       └── src/lib.rs
├── apple/
│   ├── project.yml               # XcodeGen 정의
│   ├── .swift-format
│   ├── PierApp/                  # 창 앱 + 메뉴바 + 설정 창 (SwiftUI)
│   └── Generated/                # UniFFI가 생성한 Swift 바인딩 (git 제외)
├── scripts/
│   ├── build-core.sh             # cargo build → XCFramework + Swift 바인딩 생성
│   └── seed-minio.sh             # 테스트용 버킷/객체 생성
├── docker/compose.yml            # MinIO
└── testdata/                     # git 제외
```

## 3. S3를 파일 탐색 모델에 맞추는 규칙

S3와 파일시스템의 차이를 메우는 핵심 규칙이다. 모든 구현은 이 규칙을 따른다.

### 3.1 식별자와 계층

- **연결 1개 = 사이드바 항목 1개.** 연결은 `버킷` 또는 `버킷/접두사`를 루트로 갖는다.
- **아이템 식별자 = 루트 기준 상대 S3 키.** 파일은 `photos/a.jpg`, 폴더는 끝에 `/`를 붙인 `photos/`. 루트는 빈 문자열이다.
- 부모 식별자는 키의 마지막 구분자 앞부분으로 계산한다 (`photos/2024/a.jpg` → `photos/2024/`).
- 삭제는 즉시 삭제다. 앱이 확인 대화상자를 띄운 뒤 실행한다.

### 3.2 폴더

- 폴더는 `delimiter=/` 목록의 공통 접두사(CommonPrefixes)와 0바이트 `폴더명/` 마커 객체 두 가지로 존재할 수 있다. 둘 다 같은 폴더로 취급한다.
- 폴더 생성 = 0바이트 `폴더명/` 마커 객체 업로드.
- 폴더 삭제 = 해당 접두사 아래 모든 객체 삭제. 가능하면 배치 삭제(요청당 최대 1,000개)를 사용한다.

### 3.3 이름 변경과 이동

- S3에는 rename이 없다. 파일 이름 변경·이동 = 서버 측 복사 후 원본 삭제. 복사가 성공한 뒤에만 원본을 삭제한다.
- 단일 복사 API는 5GB 제한이 있으므로, 5GB를 넘는 객체는 멀티파트 복사로 처리한다. OpenDAL이 이를 지원하지 않으면 `s3_admin.rs`(aws-sdk-s3)로 처리한다.
- 폴더 이름 변경은 하위 객체 전체 복사+삭제다. MVP에서는 하위 객체가 `MAX_FOLDER_RENAME = 1000`개를 넘으면 거부하고 사용자에게 이유를 알린다. 중간 실패 시 이미 복사된 객체와 남은 원본이 공존할 수 있으므로, 진행 상태를 SQLite에 기록해 재시도로 이어갈 수 있게 한다.

### 3.4 버전과 덮어쓰기

- 콘텐츠 버전 = ETag (불투명 문자열로만 취급, MD5로 해석하지 않는다). 메타데이터 버전 = ETag + LastModified.
- 업로드 대상 키가 이미 존재하면 앱이 확인 대화상자를 띄운다: **대치 / 둘 다 유지 / 건너뛰기 / 모두에 적용**. "둘 다 유지"는 `이름 (2).확장자` 식으로 비어 있는 이름을 골라 함께 올린다.
- 사용자가 보고 있던 목록의 ETag와 업로드 직전 원격 ETag가 다르면(그 사이 다른 클라이언트가 수정한 경우) 조건부 쓰기(If-Match)를 우선 쓰되, 백엔드의 If-Match 지원 여부와 무관하게 업로드 직전 HEAD 비교를 항상 수행해 감지하고 사용자에게 다시 묻는다. 서비스별 지원 여부는 Phase 5 호환성 표에 기록한다.
- 데이터 유실은 어떤 경우에도 허용하지 않는다.

### 3.5 변경 감지

- 폴더를 열면 캐시를 즉시 보여주고, 백그라운드로 목록을 다시 받아 SQLite와 비교해 추가·변경·삭제를 화면에 반영한다.
- 새로고침은 툴바 버튼, 메뉴바, ⌘R로 실행한다.
- 주기적 자동 폴링은 하지 않는다 (유휴 CPU 0 목표).

### 3.6 업로드 제외

`.DS_Store`, `._*`(AppleDouble), `.localized`, `Icon\r`, `.Spotlight-V100`, `.Trashes`는 업로드하지 않는다. 목록은 `ignore.rs`의 기본값으로 두고 설정에서 추가할 수 있게 한다. 폴더를 통째로 업로드할 때 적용한다.

## 4. 코어 설계 (`pier-core`)

### 4.1 연결 설정과 프리셋

```rust
pub struct ConnectionConfig {
    pub id: String,               // UUID
    pub display_name: String,
    pub provider: Provider,       // Aws, R2, Wasabi, MinIO, Custom
    pub endpoint: Option<String>, // AWS는 None
    pub region: String,
    pub bucket: String,
    pub prefix: Option<String>,   // 버킷 일부만 연결할 때
    pub path_style: bool,         // MinIO 등
    pub access_key_id: String,
}
// secret_access_key는 구조체에 넣지 않고, 연결을 열 때 별도 인자로만 받는다.
```

프리셋은 서비스별 엔드포인트 형식, 기본 리전, path-style 여부, 권장 청크 크기·동시성을 담는다. 예: R2는 `https://<account_id>.r2.cloudflarestorage.com`, 리전 `auto`.

| 프리셋 | 엔드포인트 | 추가 입력 | 기본 리전 | path-style | 청크/동시성 |
|---|---|---|---|---|---|
| AWS S3 | 없음 (입력 불필요) | 없음 | us-east-1 | 아니오 | 8MB / 8 |
| Cloudflare R2 | `https://<account_id>.r2.cloudflarestorage.com` | 계정 ID | auto | 아니오 | 8MB / 8 |
| Wasabi | `https://s3.<region>.wasabisys.com` | 없음 | us-east-1 | 아니오 | 8MB / 8 |
| MinIO | 사용자 입력 | 없음 | us-east-1 | 예 | 8MB / 8 |
| 커스텀 | 사용자 입력 | 없음 | us-east-1 | 예 | 8MB / 8 |

### 4.2 메타데이터 캐시 (SQLite)

- 위치: `~/Library/Caches/com.gajago.pier/connections/<connection_id>/metadata.sqlite`. 캐시이므로 삭제돼도 복구 가능하다. WAL 모드.
- 테이블 초안:
  - `items(key PK, parent, is_dir, size, etag, last_modified, storage_class, seen_at)`
  - `folders(prefix PK, last_listed_at, last_opened_at, complete)`
  - `state(name PK, value)` — 스키마 버전 등 일반 상태
  - `pending_ops(id PK, kind, payload, created_at)` — 폴더 이름 변경 등 재시도가 필요한 작업
- 스키마 변경은 버전 번호 기반 마이그레이션으로 관리한다.

### 4.3 목록과 페이지네이션

- 원격에서는 폴더 바로 아래 목록(`delimiter=/`)을 한 번에 모두 받아 캐시에 저장하고, FFI에는 캐시에서 키 순으로 1,000개씩 나눈 페이지를 준다. 커서는 마지막으로 반환한 키다. (S3 응답 경계를 넘는 부분 읽기는 OpenDAL 목록 순서 때문에 항목이 누락될 수 있어 쓰지 않는다.)
- 캐시에 해당 폴더가 있으면 캐시 결과를 먼저 반환하고, 백그라운드 갱신 결과는 3.5 경로로 반영한다.

### 4.4 전송

- **다운로드:** 객체 크기가 `8MB` 이하면 단일 GET, 초과하면 `8MB` 범위 요청을 기본 동시성 `8`로 병렬 수행해 임시 파일의 정해진 위치에 직접 쓴다. 완료 후 크기를 검증한다.
- **폴더 다운로드:** 재귀 목록으로 하위 객체를 모두 받은 뒤 `dest_dir/<폴더 이름>`(이름 충돌 시 ` (2)`, ` (3)`…)을 만들고 상대 경로 구조 그대로 저장한다. 모든 파일의 청크 요청을 합쳐 총 동시성 `concurrency`를 공유하고, `..`·`.`·빈 경로 구간은 안전한 이름으로 치환해 대상 밖으로 나가지 않게 한다. 폴더 마커는 빈 디렉터리로 만들고, 객체 `a`와 폴더 `a/`가 공존하면 파일은 `a (파일)`로 저장한다.
- **업로드:** `16MB` 이하면 단일 PUT, 초과하면 멀티파트 업로드. 파트 크기는 파일 크기에 따라 키워 파트 수가 10,000을 넘지 않게 한다. 실패하거나 취소되면 멀티파트 업로드를 반드시 abort한다.
- 청크 크기·동시성은 프리셋 기본값을 쓰고 설정에서 조정할 수 있게 한다.
- 전송 대기열(동시 전송 개수 제한, 일시정지/취소/재시도)은 코어가 관리하고 Swift는 표시만 한다.
- 진행률은 콜백(UniFFI 콜백 인터페이스)으로 Swift UI(대기열 패널·메뉴바)에 전달한다.
- 모든 전송은 취소 토큰을 받는다.

### 4.5 에러

코어 에러는 아래 범주로 분류하고, Swift가 사용자에게 보여줄 메시지로 변환한다.

| 코어 에러 | 앱에 표시할 사용자 메시지 |
|---|---|
| 인증 실패 (403 InvalidAccessKeyId, SignatureDoesNotMatch) | 액세스 키를 확인하라는 안내 |
| 네트워크 단절, 타임아웃 | 네트워크 연결을 확인하고 재시도하라는 안내 |
| 객체 없음 (404) | 이미 삭제됨 안내 + 목록 새로고침 |
| ETag 불일치 | 3.4 덮어쓰기 확인 대화상자 |
| 정책상 거부 (폴더 이름 변경 한도 초과 등) | 거부 이유 표시 |

### 4.6 로깅

`log` 파사드를 사용하고, FFI 층이 코어 로그를 콜백으로 받아 macOS 통합 로그(os_log)로 내보낸다. 키, 시크릿, presigned URL 전체 값은 로그에 남기지 않는다.

## 5. FFI 설계 (`pier-ffi`)

- UniFFI proc-macro 방식. 비동기 함수는 tokio 런타임으로 실행되도록 설정한다.
- 노출 API 초안 (Swift에서 보이는 형태 기준):

| API | 설명 |
|---|---|
| `Connection.open(config, secret, cacheDir)` | 연결 생성, 캐시 SQLite 열기 |
| `testConnection()` | 목록 + 쓰기 권한(임시 객체 쓰기/삭제) |
| `item(key)` | 단일 아이템 메타데이터 (stat) |
| `listPage(folder, cursor?)` | 폴더 목록 한 페이지 + 다음 커서 (캐시 여부 표시 포함) |
| `refresh(folder)` | 백그라운드 재조회 결과로 추가·변경·삭제 목록 반환 |
| `download(key, destPath, progress, cancel)` | 내려받기 |
| `upload(srcPath, parent, name, ifMatch?, progress, cancel)` | 생성/수정 업로드 (ifMatch는 조건부 쓰기용 기준 ETag) |
| `existing(parent, names)` | parent 아래 이미 존재하는 항목(파일·폴더) 조회 |
| `uploadFolder(srcDir, parent, mode, progress, cancel)` | 폴더 재귀 업로드 (3.6 제외 규칙 적용) |
| `createFolder(parent, name)` | 폴더 생성 |
| `rename(key, newParent, newName)` | 이동·이름 변경 |
| `delete(key)` | 삭제 |
| `resumePendingOps()` | 중단된 폴더 이름 변경 이어서 완료, 처리 건수 반환 |
| `presign(key, expiresSecs)` | presigned GET URL (최대 7일) |
| `presets()` | 서비스 프리셋 목록 (4.1) |
| `presetEndpoint(presetId, accountId?, region)` | 프리셋·입력으로 엔드포인트 생성. 직접 입력이 필요하거나 형식이 맞지 않으면 `nil` |
| `setLogSink(sink)` | 코어 로그를 받는 싱크 설치. 프로세스당 한 번, Info 이상만 전달 |
| `enqueueDownload(key, destPath)` / `enqueueDownloadFolder(prefix, destDir)` | 다운로드 작업을 대기열에 넣고 작업 id 반환 |
| `enqueueUpload(srcPath, parent, name, mode)` / `enqueueUploadFolder(srcDir, parent, mode)` | 업로드 작업을 대기열에 넣고 작업 id 반환 |
| `pauseJob` / `resumeJob` / `cancelJob` / `retryJob` / `removeJob` (id) | 대기열 작업 제어 (동기, 없는 id는 무시) |
| `jobs()` | 모든 작업 스냅샷을 id 순(FIFO)으로 반환 |
| `setJobListener(listener)` | 작업 변화 통지 콜백 등록. 작업당 100ms 스로틀, 상태 변화는 즉시 |
| `info(key)` | 객체 정보(메타데이터 + 스토리지 클래스) |
| `prefetch(folder)` | 최근 목록의 하위 폴더를 미리 캐시(최대 20개, 동시성 2) |
| `preview(key, progress, cancel)` | 로컬 미리보기 파일 경로 반환 (연결 캐시 디렉터리/previews, ETag 재사용, 1GB LRU) |

- `PierConfig`는 `provider`(프리셋 id)·`chunkSizeMb`·`concurrency`를 포함해 연결 전송 옵션을 결정한다.
- Rust 타입은 UniFFI Record/Enum으로 노출하고, Swift 쪽 뷰 모델로 감싸는 변환은 `PierApp`에서 한다.
- 코어가 호출하는 외부 콜백은 `PierJobListener`, `PierLogSink`, `PierProgress`의 세 가지 외부 트레이트다. 모두 백그라운드 스레드에서 호출된다.

## 6. macOS 앱 설계

### 6.1 메인 창과 연결 관리

- **메인 창:** 사이드바에 연결 목록, 본문에 파일 테이블(이름, 크기, 수정 시각), 경로 표시줄(breadcrumb), 툴바(새로고침, 업로드, 새 폴더, 다운로드). 폴더 더블클릭으로 진입하고 뒤로/앞으로 이동을 지원한다.
- **연결 관리 설정 창:** 연결 추가·편집·삭제. 프리셋 선택 → 필요한 필드만 표시 → "연결 테스트" 성공 시에만 저장. 시크릿은 로그인 키체인(`kSecClassGenericPassword`, service `com.gajago.pier`, account = connection id)에 저장하고, 설정은 `~/Library/Application Support/com.gajago.pier/connections.json`에 저장한다. 연결 삭제 시 키체인 항목과 캐시까지 삭제한다.

### 6.2 메뉴바 (`MenuBarExtra`)

- 진행 중인 전송과 속도, 연결 목록에서 창 열기, 현재 폴더 새로고침, 설정 열기.

### 6.3 Finder와의 드래그 앤 드롭

- **Finder → 앱:** 드롭한 파일·폴더를 업로드한다 (폴더 포함, 3.6 제외 규칙 적용). 다중 선택 지원.
- **앱 → Finder:** 선택한 객체를 `NSFilePromiseProvider`(파일 프로미스)로 드래그하고, Finder 드롭 위치에 내려받는다. 다중 선택 지원.

### 6.4 Quick Look 미리보기

- 파일을 선택하고 스페이스바를 누르면 Quick Look으로 미리보기한다.
- 임시 캐시 디렉터리에 내려받아 `QLPreviewPanel`로 표시한다. 대용량 파일은 다운로드 진행률을 보여준다.

### 6.5 우클릭 메뉴

- "Presigned URL 복사" (만료 1시간 / 1일 / 7일)
- "정보 보기" (키, 크기, ETag, 최종 수정 시각; 스토리지 클래스는 HEAD 요청으로 가져온다)
- "다운로드", "이름 변경", "삭제"

## 알려진 제약

- ad-hoc 서명 앱은 빌드할 때마다 코드 서명이 달라져, 개발 중 재빌드 후 키체인 접근 허용 대화상자가 다시 뜰 수 있다 (배포본은 한 번 허용하면 유지된다).
- App Sandbox를 쓰지 않는다.
- 객체가 매우 많은 폴더(수만 개 이상)는 첫 화면 표시가 전체 목록 수신 시간만큼 걸린다.
- 전송 대기열은 앱 종료 시 사라지고, 미완료 멀티파트 업로드는 다음 실행 때 정리된다.

## 7. 단계별 작업 계획

각 Phase는 계획 제시 → 승인 → 구현 → 검증 → 커밋 순서로 진행한다.

### Phase 0. 통합 검증 스파이크 (가장 중요)

가장 위험한 구간인 "Rust → UniFFI → SwiftUI 앱" 경로를 최소 기능으로 먼저 관통한다.

- Cargo workspace, `pier-core`/`pier-ffi` 골격, MinIO `docker/compose.yml`, `seed-minio.sh`.
- 코어: OpenDAL로 MinIO 버킷 루트 목록 조회 (캐시 없음).
- `build-core.sh`: XCFramework와 Swift 바인딩 생성.
- XcodeGen `project.yml`: `PierApp` 타깃.
- 앱: 하드코딩된 MinIO 연결로 창에 루트 목록만 읽기 전용 표시.
- 로컬 빌드는 서명 없이 진행한다 (`CODE_SIGNING_ALLOWED=NO` 또는 ad-hoc).
- **완료 기준:** 앱 창에 MinIO 버킷 루트의 파일·폴더 이름이 보인다.

### Phase 1. 코어 읽기 경로

- `key.rs`, `item.rs`, `metadata.rs`, `enumerate.rs` 구현 (3.1, 3.2, 4.2, 4.3).
- `cli.rs`: `ls`, `stat`, `get` 서브커맨드.
- 단위 테스트: 키 변환(한글·공백·특수문자 키 포함), 폴더 마커와 공통 접두사 병합, 페이지네이션 경계. 통합 테스트: MinIO에서 객체 2,500개 폴더 목록.
- **완료 기준:** CLI로 MinIO의 중첩 폴더를 탐색·다운로드할 수 있고, 두 번째 목록 조회는 캐시에서 응답한다.

### Phase 2. 읽기 전용 브라우저

- 폴더 탐색(breadcrumb, 뒤로/앞으로), 4.4 병렬 다운로드(다운로드 버튼·저장 위치 선택), 폴더 다운로드(하위 객체 전체, 구조 유지), 진행률, 4.5 에러 표시.
- 3.5 백그라운드 갱신·새로고침(툴바/메뉴바/⌘R).
- 연결 관리 설정 창, 로그인 키체인 저장, 연결 테스트, 다중 연결.
- **완료 기준:** 실제 AWS S3와 MinIO를 동시에 등록해 탐색·다운로드할 수 있다. 1GB 파일 다운로드 처리량이 성능 목표를 충족한다. 잘못된 키로 연결하면 명확한 오류가 표시된다. 캐시된 폴더가 100ms 이내에 표시된다.

### Phase 3. 쓰기 경로

- 업로드(버튼·Finder→앱 드롭, 폴더 포함), 새 폴더, 이름 변경, 이동(앱 내 드래그), 삭제.
- 4.4 멀티파트 업로드, 3.3 이름 변경 규칙, 3.4 덮어쓰기 확인, 3.6 업로드 제외.
- 앱→Finder 파일 프로미스 다운로드(6.3).
- **완료 기준:** 앱에서 업로드 버튼과 Finder에서 앱으로의 드래그 앤 드롭(폴더 포함)으로 업로드할 수 있고, 새 폴더, 이름 변경, 이동, 삭제가 모두 동작한다. 5GB 이상 파일 업로드·이동이 성공한다. 업로드 중 취소 시 미완료 멀티파트가 남지 않는다. 다른 클라이언트가 동시에 수정한 파일을 덮어쓰려 하면 확인 대화상자가 뜨고 데이터가 유실되지 않는다. 앱에서 Finder로 드래그한 파일이 드롭 위치에 내려받아진다. `.DS_Store`가 버킷에 올라가지 않는다.

### Phase 4. 편의 기능 (차별화)

- 전송 대기열 패널·메뉴바 전송 현황, 6.4 Quick Look 미리보기, 6.5 우클릭 메뉴(presigned URL, 정보 보기), 서비스 프리셋 완성(AWS/R2/Wasabi/MinIO), 최근 열린 폴더의 하위 폴더 목록 프리페치, 전송 설정(청크·동시성) UI.
- **완료 기준:** 우클릭으로 presigned URL을 복사해 브라우저에서 바로 열 수 있다. 각 프리셋으로 처음 연결하는 데 필요한 입력이 키 2개 + 버킷 이름(+ R2는 계정 ID) 수준이다. 스페이스바로 이미지/PDF 미리보기가 동작한다.

### Phase 5. 호환성 검증과 배포

- 호환성 표 작성: AWS S3, R2, Wasabi, MinIO 각각에 대해 목록·범위 읽기·멀티파트·서버 측 복사(5GB 초과 포함)·배치 삭제·조건부 쓰기·presign 동작 여부를 기록하고, 미지원 기능은 대체 경로로 처리.
  - 호환성 표와 검증 스크립트 사용법: [docs/COMPATIBILITY.md](COMPATIBILITY.md) (`./scripts/compat-test.sh`).
- 성능 측정: 동일 조건에서 `aws s3 cp`, rclone과 처리량 비교 결과를 README에 기록.
- 서명 없이 배포: Release 빌드를 ad-hoc 서명해 DMG(또는 zip)로 GitHub Releases에 배포한다. README에 "처음 실행 시 Gatekeeper 경고 → 시스템 설정 > 개인정보 보호 및 보안 > '확인 없이 열기'" 안내를 넣는다. 라이선스 파일(MIT/Apache-2.0 이중 라이선스 예정, 확정 필요).
- 패키징 명령: `./scripts/package.sh` — 코어 release 빌드 → xcodegen → Release 빌드 → ad-hoc 서명 → `dist/Pier-<버전>.dmg` 생성.
- **완료 기준:** 다른 맥에서 README 안내대로 설치·실행되고, 네 서비스 모두 Phase 2~4 기능이 동작하거나 제약이 문서화되어 있다.

### 이후 후보

OpenDAL 기반 다른 백엔드(OneDrive → Google Drive 순), 버전 목록·복원, 스토리지 클래스 변경, 연결된 모든 버킷 통합 검색, 클라우드 간 직접 복사, 대역폭 제한 스케줄, File Provider 기반 Finder 통합, Developer ID 서명·공증.
