#!/usr/bin/env bash
# Pier 코어의 S3 호환성 검증 스크립트.
#
# 기능별로 pier-core 통합 테스트를 실행하고 통과/실패/건너뜀을 표로 출력한다.
# 대상은 환경변수로만 지정하며, 키는 절대 인수나 출력에 넣지 않는다.
#
# 사용법:
#   ./scripts/compat-test.sh [--big]
#
#   --big   5GB 초과 복사 등 대용량 테스트까지 실행한다(PIER_TEST_BIG=1).
#           수 GB의 업로드·디스크가 필요하므로 기본 실행에서는 건너뛴다.
#
# 로컬 MinIO(기본 대상):
#   docker compose -f docker/compose.yml up -d
#   ./scripts/seed-minio.sh        # 루트 시드 데이터(일부 테스트가 의존)
#   ./scripts/compat-test.sh
#
# 원격 대상은 PIER_TEST_BUCKET을 설정하면 된다. 필요한 변수:
#   PIER_TEST_BUCKET        버킷 이름(설정하면 원격 모드)
#   PIER_TEST_ACCESS_KEY    액세스 키
#   PIER_TEST_SECRET_KEY    시크릿 키
#   PIER_TEST_ENDPOINT      엔드포인트(없으면 AWS S3 기본)
#   PIER_TEST_REGION        리전(기본 us-east-1; R2는 auto)
#   PIER_TEST_PATH_STYLE    1/true면 path-style 요청(기본: 가상 호스트)
#   PIER_TEST_PREFIX        테스트 객체 접두사(기본 pier-compat-test/)
#                           이 접두사 아래에만 객체를 만들고, 끝에 접두사만 지운다.
#
# 예시(키는 셸 환경변수로만):
#
#   # AWS S3
#   PIER_TEST_BUCKET=my-bucket PIER_TEST_REGION=ap-northeast-2 \
#   PIER_TEST_ACCESS_KEY="$AWS_ACCESS_KEY_ID" \
#   PIER_TEST_SECRET_KEY="$AWS_SECRET_ACCESS_KEY" \
#   ./scripts/compat-test.sh
#
#   # Cloudflare R2 (region은 auto, endpoint 필수)
#   PIER_TEST_BUCKET=my-bucket \
#   PIER_TEST_ENDPOINT="https://<account-id>.r2.cloudflarestorage.com" \
#   PIER_TEST_REGION=auto \
#   PIER_TEST_ACCESS_KEY="$R2_ACCESS_KEY_ID" \
#   PIER_TEST_SECRET_KEY="$R2_SECRET_ACCESS_KEY" \
#   ./scripts/compat-test.sh
#
#   # Wasabi
#   PIER_TEST_BUCKET=my-bucket \
#   PIER_TEST_ENDPOINT="https://s3.ap-northeast-2.wasabisys.com" \
#   PIER_TEST_REGION=ap-northeast-2 \
#   PIER_TEST_ACCESS_KEY="$WASABI_ACCESS_KEY" \
#   PIER_TEST_SECRET_KEY="$WASABI_SECRET_KEY" \
#   ./scripts/compat-test.sh

set -uo pipefail
cd "$(dirname "$0")/.."

BIG=0
for arg in "$@"; do
    case "$arg" in
        --big) BIG=1 ;;
        -h | --help)
            sed -n '2,60p' "$0"
            exit 0
            ;;
        *)
            echo "알 수 없는 인수: $arg (--big 만 지원)" >&2
            exit 2
            ;;
    esac
done

REMOTE=0
if [ -n "${PIER_TEST_BUCKET:-}" ]; then
    REMOTE=1
    if [ -z "${PIER_TEST_ACCESS_KEY:-}" ] || [ -z "${PIER_TEST_SECRET_KEY:-}" ]; then
        echo "오류: 원격 대상에는 PIER_TEST_ACCESS_KEY와 PIER_TEST_SECRET_KEY가 필요합니다." >&2
        exit 2
    fi
    PREFIX="${PIER_TEST_PREFIX:-pier-compat-test/}"
    echo "대상: 원격 버킷 ${PIER_TEST_BUCKET} (엔드포인트: ${PIER_TEST_ENDPOINT:-AWS 기본}, 리전: ${PIER_TEST_REGION:-us-east-1})"
    echo "테스트 객체는 ${PREFIX} 아래에만 만들고 끝에 정리합니다."
else
    export PIER_TEST_MINIO=1
    echo "대상: 로컬 MinIO (http://127.0.0.1:9000, pier-test)"
    if ! curl -sf -o /dev/null --max-time 3 http://127.0.0.1:9000/minio/health/live; then
        echo "오류: MinIO에 연결할 수 없습니다." >&2
        echo "  docker compose -f docker/compose.yml up -d && ./scripts/seed-minio.sh" >&2
        exit 2
    fi
fi

if [ "$BIG" -eq 1 ]; then
    export PIER_TEST_BIG=1
fi

# 기능별 테스트 목록. "바이너리:필터,필터,...;바이너리:필터,..." 형식.
# 루트 시드 데이터(seed-minio.sh)에 의존하는 테스트는 로컬에서만 필터에 넣는다.
LIST_FILTERS="paging_minio,bugcheck_regression_minio,refresh_detects_add_and_delete_minio"
if [ "$REMOTE" -eq 0 ]; then
    LIST_FILTERS="${LIST_FILTERS},root_first_page_minio,marker_only_folder_lists_empty_minio,stat_and_read_minio"
fi

FEATURES=(
    "연결·인증|ops:test_connection_minio;s3_raw:wrong_secret_is_authentication"
    "목록·페이지네이션|minio_list:${LIST_FILTERS}"
    "범위 읽기·병렬 다운로드|transfer:download_big_minio,download_cancel_minio,download_folder_minio,download_missing_key_minio"
    "멀티파트 업로드|upload:upload_small_file_minio,upload_multipart_40mb_minio,upload_cancel_minio,upload_folder_minio,create_dir_and_list_minio"
    "이어올리기|s3_raw:resumable_upload_pause_and_resume,abort_after_pause_leaves_no_mpu,cleanup_pending_uploads_removes_leftover;queue:download_pause_resume_minio,download_cancel_removes_tmp_minio,folder_download_pause_resume_minio"
    "서버 측 복사|ops:rename_file_move_minio,rename_file_conflict_minio,rename_folder_30_minio,rename_folder_over_limit_minio"
    "배치 삭제|upload:delete_keys_batches_minio;ops:delete_folder_minio"
    "조건부 쓰기|upload:upload_conflict_modes_minio;ops:create_folder_list_existing_conflict_minio;s3_raw:complete_conditional_conflict"
    "presign|misc:presign_expiry_bounds,presigned_url_get_minio"
    "HEAD 스토리지 클래스|s3_raw:head_storage_class_standard"
)

N_PASS=0
N_FAIL=0
N_SKIP=0
declare -a ROWS

# 기능 하나를 실행해 표 행을 기록한다. 인수: 이름|세그먼트들
run_feature() {
    local name="$1" spec="$2"
    local ran=0 passed=0 failed_names=""
    local segment bin filters out rc n
    local IFS_SAVE="$IFS"
    IFS=';'
    for segment in $spec; do
        IFS="$IFS_SAVE"
        bin="${segment%%:*}"
        filters="${segment#*:}"
        # shellcheck disable=SC2086
        out=$(cargo test -p pier-core --test "$bin" -- ${filters//,/ } 2>&1)
        rc=$?
        n=$(printf '%s' "$out" | sed -n 's/.*test result: ok\. \([0-9]*\) passed.*/\1/p' | tail -1)
        n=${n:-0}
        if [ "$rc" -ne 0 ]; then
            failed_names="${failed_names}$(printf '%s\n' "$out" | sed -n 's/^test \([^ ]*\) \.\.\. FAILED/\1/p')"
        fi
        ran=$((ran + n))
        IFS=';'
    done
    IFS="$IFS_SAVE"
    if [ -n "$failed_names" ]; then
        ROWS+=("${name}|실패|$(printf '%s' "$failed_names" | tr '\n' ' ')")
        N_FAIL=$((N_FAIL + 1))
    else
        ROWS+=("${name}|통과|${ran}개 테스트")
        N_PASS=$((N_PASS + 1))
    fi
}

echo
for entry in "${FEATURES[@]}"; do
    name="${entry%%|*}"
    spec="${entry#*|}"
    echo "== $name"
    run_feature "$name" "$spec"
done

if [ "$BIG" -eq 1 ]; then
    echo "== 5GB 초과 복사"
    run_feature "5GB 초과 복사" "ops:rename_big_file_minio;upload:upload_big_51gb_minio"
else
    ROWS+=("5GB 초과 복사|건너뜀|--big 없음")
    N_SKIP=$((N_SKIP + 1))
fi

# 원격이면 테스트 객체가 든 접두사를 정리한다(로컬 MinIO는 시드 보호를 위해 안 건드림).
if [ "$REMOTE" -eq 1 ]; then
    echo
    echo "== 원격 접두사 정리 (${PREFIX})"
    if cargo test -p pier-core --test ops remote_prefix_cleanup -- --exact >/dev/null 2>&1; then
        echo "정리 완료"
    else
        echo "경고: 접두사 정리에 실패했습니다. ${PREFIX} 아래 객체를 수동으로 확인하세요." >&2
    fi
fi

echo
echo "===== 호환성 결과 ====="
printf '%-28s %-6s %s\n' "기능" "결과" "비고"
printf '%-28s %-6s %s\n' "----" "----" "----"
for row in "${ROWS[@]}"; do
    IFS='|' read -r name result note <<<"$row"
    printf '%-28s %-6s %s\n' "$name" "$result" "$note"
done
echo
echo "통과 ${N_PASS}개 / 실패 ${N_FAIL}개 / 건너뜀 ${N_SKIP}개 기능"

if [ "$N_FAIL" -gt 0 ]; then
    exit 1
fi
