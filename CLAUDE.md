# Pier (가칭)

S3 호환 버킷을 탐색하고 파일을 주고받는 macOS 독립 앱(창 + 메뉴바). Finder와 드래그 앤 드롭으로 오간다.
상세 요구사항과 단계별 계획은 `docs/SPEC.md`를 따른다. 작업 시작 전 반드시 읽는다.

## 기술 스택

- 코어: Rust (`crates/pier-core`) — Apache OpenDAL(데이터 경로), `aws-sdk-s3`(S3 전용 관리 기능), `rusqlite`(메타데이터 캐시), `tokio`
- FFI: UniFFI (`crates/pier-ffi`) — Rust 코어를 Swift async API로 노출
- macOS: Swift + SwiftUI (메인 창, 메뉴바, 설정 창)
- Xcode 프로젝트: XcodeGen (`apple/project.yml`에서 생성)
- 로컬 테스트 스토리지: Docker MinIO (`docker/compose.yml`)
- 최소 지원: macOS 14, Apple Silicon (Intel 지원은 추후)

## 아키텍처 원칙 (반드시 지킬 것)

1. **판단은 Rust, 연결은 Swift.** S3 통신, 메타데이터 캐시, 변경 감지, 덮어쓰기 판단, 전송 로직은 모두 `pier-core`에 둔다. Swift는 macOS API(키체인, UI, 드래그 앤 드롭, Quick Look)와 코어를 잇는 얇은 층만 맡는다.
2. **`pier-core`는 Apple 플랫폼을 모른다.** FFI·Swift 의존성 없이 `cargo test`와 CLI 예제만으로 검증 가능해야 한다. UniFFI 관련 코드는 `pier-ffi`에만 둔다.
3. **비밀값은 키체인에만.** Secret Access Key는 Swift가 로그인 키체인에 저장하고 실행 시 코어에 전달한다. 디스크·로그·SQLite·에러 메시지에 절대 남기지 않는다.
4. **S3는 파일시스템이 아니다.** 폴더, 이름 변경, 부분 수정, 변경 알림이 없다는 전제로 설계한다 (SPEC 3장).
5. **앱 UI를 막지 않는다.** 목록은 캐시에서 즉시 응답하고 갱신은 백그라운드에서 한다. 모든 네트워크 작업은 취소 가능해야 한다.

## 코드 스타일

- 들여쓰기: 공백 4칸 (탭 금지). Rust, Swift, TOML, JSON 모두 동일. 단 YAML은 관례에 따라 2칸을 허용한다.
- 중괄호: 여는 중괄호는 구문과 같은 줄, 닫는 중괄호는 블록 시작 위치와 같은 들여쓰기.
- Rust: `rustfmt` 기본 규칙 + `cargo clippy -- -D warnings` 통과. 코어 에러 타입은 `thiserror`로 정의한다.
- Swift: `swift format` (4칸 설정 파일 `.swift-format` 포함). 강제 언래핑(`!`)은 테스트 코드 외 금지.
- 가독성 최우선. 성능 최적화는 측정으로 병목이 확인된 경우에만 적용한다 (SPEC에 명시된 전송 설계는 예외).
- 불필요한 추상화, 트릭, 매크로 남용을 피한다.
- 코드 주석과 커밋 메시지: 한국어.
- 새 의존성(crate, Swift 패키지) 추가 시 이유를 먼저 설명하고 승인받는다.

## 작업 방식

- `docs/SPEC.md`의 Phase 순서대로 진행한다. 한 번에 하나의 Phase만 작업한다.
- 각 Phase 시작 시 구현 계획(파일 목록, 주요 타입/함수, 순서)을 먼저 제시하고 승인 후 코드를 작성한다.
- 파일이 크거나(300줄 이상) 변경 범위가 넓으면 한 번에 전체를 수정하지 않는다. 함수/섹션 단위로 나눠 순차적으로 수정하고, 각 단계마다 diff를 보여준 뒤 다음 단계로 진행한다.
- `.xcodeproj`(특히 `project.pbxproj`)는 직접 수정하지 않는다. `apple/project.yml`을 고치고 `xcodegen generate`로 재생성한다.
- 사람이 직접 해야 하는 작업(앱 실행 후 화면 확인, 실제 클라우드 계정 준비)은 명확히 "사용자 수동 작업"으로 안내하고 완료 확인을 받은 뒤 진행한다.
- Phase 완료 조건: 아래 검증 명령이 모두 통과하고, SPEC의 해당 Phase 완료 기준을 충족할 것.
- Phase가 끝나면 변경 요약을 보고하고 커밋한다 (Phase 단위 커밋).

## 주요 명령

```bash
# 로컬 MinIO 실행 (테스트용)
docker compose -f docker/compose.yml up -d

# Rust 검증
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                       # MinIO 통합 테스트는 PIER_TEST_MINIO=1 일 때만 실행

# Rust 코어 → XCFramework + Swift 바인딩 생성
./scripts/build-core.sh

# Xcode 프로젝트 생성 및 빌드 (로컬은 서명 없이)
cd apple && xcodegen generate
xcodebuild -project apple/Pier.xcodeproj -scheme Pier -configuration Debug CODE_SIGNING_ALLOWED=NO build

# 앱 로그 확인
log stream --predicate 'subsystem BEGINSWITH "com.gajago.pier"'
```

## 식별자

- 앱 번들 ID: `com.gajago.pier`
- 키체인 service: `com.gajago.pier` (account = connection id)
- 로그 subsystem: `com.gajago.pier`
