#!/usr/bin/env bash
# 로컬 MinIO 대상 대용량 파일 전송 처리량 비교 (SPEC 7장 Phase 5 성능 측정)
#
# 측정 대상: Pier CLI, aws s3 cp, rclone
# 공정성: 세 도구 모두 pier_default 네트워크의 컨테이너에서 실행해 MinIO까지의
# 네트워크 경로를 같게 하고, 같은 호스트 디렉터리를 바인드 마운트해 디스크 경로도
# 같게 한다. Pier는 rust 이미지에서 빌드한 Linux 바이너리를 사용한다.
#
# 사용: ./scripts/bench.sh
# 환경변수:
#   BENCH_RUNS     방향당 반복 횟수 (기본 3)
#   BENCH_SIZE_MB  파일 크기 MiB (기본 1024)
#   BENCH_TOOLS    측정할 도구 목록 (기본 "pier aws rclone")
#   PIER_CHUNK_MB / PIER_CONCURRENCY  Pier 전송 설정 (기본 8 / 8)
set -euo pipefail

cd "$(dirname "$0")/.."

RUNS=${BENCH_RUNS:-3}
SIZE_MB=${BENCH_SIZE_MB:-1024}
SIZE_BYTES=$((SIZE_MB * 1024 * 1024))
TOOLS=${BENCH_TOOLS:-"pier aws rclone"}
BUCKET=${PIER_BUCKET:-pier-test}
# 이 스크립트가 만드는 객체는 모두 이 접두사 아래에 두고 끝에 지운다.
PREFIX="docs/p5-perf"
ENDPOINT="http://minio:9000"
COMPOSE_FILE="docker/compose.yml"

AWSCLI_IMAGE="amazon/aws-cli:2.40.1"
RCLONE_IMAGE="rclone/rclone:1.71.2"
RUST_IMAGE="rust:1.99.1-bookworm"
TARGET_VOL="pier-bench-target"
CARGO_VOL="pier-bench-cargo"
PIER_BIN="/target/release/examples/cli"

PIER_CHUNK_MB=${PIER_CHUNK_MB:-8}
PIER_CONCURRENCY=${PIER_CONCURRENCY:-8}

WORK="$(mktemp -d /tmp/pier-bench.XXXXXX)"
SRC_FILE="$WORK/src/bench.bin"
declare -a CIDS=()

log() { echo "[bench] $*" >&2; }

cleanup() {
    set +e
    # 업로드한 객체 정리 (이 스크립트의 접두사 아래만). BENCH_TOOLS에 aws가
    # 없어도 지워지도록 일회용 컨테이너로 지운다.
    if [ -n "${NETWORK:-}" ]; then
        docker run --rm --network "$NETWORK" \
            -e AWS_ACCESS_KEY_ID=minioadmin -e AWS_SECRET_ACCESS_KEY=minioadmin \
            -e AWS_EC2_METADATA_DISABLED=true \
            "$AWSCLI_IMAGE" --endpoint-url "$ENDPOINT" \
            s3 rm --recursive "s3://$BUCKET/$PREFIX/" >/dev/null 2>&1
    fi
    if [ ${#CIDS[@]} -gt 0 ]; then
        docker rm -f "${CIDS[@]}" >/dev/null 2>&1
    fi
    rm -rf "$WORK"
}
trap cleanup EXIT

has_tool() { case " $TOOLS " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }

# MinIO 컨테이너와 네트워크 확인 (seed-minio.sh와 같은 방법)
MINIO_CID="$(docker compose -f "$COMPOSE_FILE" ps -q minio)"
[ -n "$MINIO_CID" ] || { log "오류: minio 컨테이너가 없습니다. 'docker compose -f $COMPOSE_FILE up -d' 먼저."; exit 1; }
HEALTH="$(docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}' "$MINIO_CID")"
[ "$HEALTH" = "healthy" ] || { log "오류: MinIO 상태가 healthy가 아닙니다: $HEALTH"; exit 1; }
NETWORK="$(docker inspect -f '{{range $k, $v := .NetworkSettings.Networks}}{{$k}}{{"\n"}}{{end}}' "$MINIO_CID" | head -n1)"
[ -n "$NETWORK" ] || { log "오류: MinIO 네트워크를 찾을 수 없습니다."; exit 1; }
log "MinIO healthy, 네트워크: $NETWORK"

# 결정적 고엔트로피 데이터 생성: 고정 키 AES-256-CTR 스트림이라 매번 같은
# 내용이 나오고, 서버 측 압축·중복제거로 속도가 부풀려지지 않는다.
mkdir -p "$WORK/src" "$WORK/dst"
if [ ! -f "$SRC_FILE" ] || [ "$(stat -f%z "$SRC_FILE")" != "$SIZE_BYTES" ]; then
    log "${SIZE_MB}MiB 결정적 데이터 생성 중..."
    head -c "$SIZE_BYTES" /dev/zero | openssl enc -aes-256-ctr -nosalt \
        -K 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f \
        -iv 000102030405060708090a0b0c0d0e0f \
        > "$SRC_FILE"
fi
log "소스 파일: $SRC_FILE ($(stat -f%z "$SRC_FILE") bytes)"

# 측정용 헬퍼 컨테이너 기동 (docker exec로 실행해 컨테이너 기동 오버헤드를
# 측정값에서 제외한다. 시간은 컨테이너 안에서 잰다)
if has_tool pier; then
    log "Pier Linux 바이너리 빌드 (캐시 볼륨 사용)..."
    docker run --rm \
        -v "$PWD:/src:ro" -v "$TARGET_VOL:/target" -v "$CARGO_VOL:/usr/local/cargo" \
        "$RUST_IMAGE" bash -c '
            command -v cmake >/dev/null 2>&1 || { apt-get update -qq && apt-get install -y -qq cmake >/dev/null; }
            cd /src && CARGO_TARGET_DIR=/target cargo build --release -p pier-core --example cli 2>&1 | tail -3'
    PIER_CID="$(docker run -d --network "$NETWORK" \
        -v "$WORK:/bench" -v "$TARGET_VOL:/target" \
        --entrypoint bash "$RUST_IMAGE" -c 'sleep infinity')"
    CIDS+=("$PIER_CID")
fi
if has_tool aws; then
    AWS_CID="$(docker run -d --network "$NETWORK" -v "$WORK:/bench" \
        -e AWS_ACCESS_KEY_ID=minioadmin -e AWS_SECRET_ACCESS_KEY=minioadmin \
        -e AWS_DEFAULT_REGION=us-east-1 -e AWS_EC2_METADATA_DISABLED=true \
        --entrypoint bash "$AWSCLI_IMAGE" -c 'sleep infinity')"
    CIDS+=("$AWS_CID")
fi
if has_tool rclone; then
    RCLONE_CID="$(docker run -d --network "$NETWORK" -v "$WORK:/bench" \
        -e RCLONE_CONFIG_MIO_TYPE=s3 -e RCLONE_CONFIG_MIO_PROVIDER=Minio \
        -e RCLONE_CONFIG_MIO_ACCESS_KEY_ID=minioadmin \
        -e RCLONE_CONFIG_MIO_SECRET_ACCESS_KEY=minioadmin \
        -e RCLONE_CONFIG_MIO_ENDPOINT="$ENDPOINT" \
        -e RCLONE_CONFIG_MIO_REGION=us-east-1 \
        -e RCLONE_CONFIG_MIO_FORCE_PATH_STYLE=true \
        --entrypoint sh "$RCLONE_IMAGE" -c 'while :; do sleep 3600; done')"
    CIDS+=("$RCLONE_CID")
fi

# 컨테이너 안에서 명령 시간을 재고 초 단위 실수를 출력한다.
# bash는 TIMEFORMAT=%R로 "1.234" 형태, busybox sh는 "real\t0m 1.23s" 형태를 출력.
timed() {
    local cid="$1" shell="$2" cmd="$3" out secs
    if [ "$shell" = bash ]; then
        if ! out="$(docker exec "$cid" bash -c "TIMEFORMAT=%R; time ( $cmd >/dev/null 2>&1 )" 2>&1)"; then
            log "실패 ($cmd): $out"; return 1
        fi
        secs="$(echo "$out" | grep -oE '^[0-9]+(\.[0-9]+)?$' | tail -1)"
    else
        # busybox sh는 time ( ... ) 문법이 안 되므로 자식 sh를 재는 형태로 쓴다.
        if ! out="$(docker exec "$cid" sh -c "time sh -c '$cmd >/dev/null 2>&1'" 2>&1)"; then
            log "실패 ($cmd): $out"; return 1
        fi
        secs="$(echo "$out" | sed -n 's/^real[[:space:]]*\([0-9]*\)m[[:space:]]*\([0-9.]*\)s$/\1 \2/p' \
            | awk '{printf "%.3f", $1*60+$2}')"
    fi
    [ -n "$secs" ] || { log "시간 파싱 실패 ($cmd): $out"; return 1; }
    echo "$secs"
}

median() {
    printf '%s\n' "$@" | sort -n | awk -v n="$#" 'NR == int((n+1)/2) {print}'
}

mbps() { awk -v mb="$SIZE_MB" -v s="$1" 'BEGIN{printf "%.1f", mb/s}'; }

# Pier CLI 환경. 측정 시간 안에 env 주입까지 포함되지만 수 ms 수준이라 무시한다.
PIER_ENV="env PIER_ENDPOINT=$ENDPOINT PIER_BUCKET=$BUCKET PIER_ACCESS_KEY=minioadmin \
PIER_SECRET_KEY=minioadmin PIER_REGION=us-east-1 PIER_PATH_STYLE=true \
PIER_CACHE_DIR=/tmp/pier-cli-cache PIER_CHUNK_MB=$PIER_CHUNK_MB \
PIER_CONCURRENCY=$PIER_CONCURRENCY"

mkdir -p "$WORK/res"
echo
echo "== 환경 =="
echo "minio:   $(docker exec "$MINIO_CID" minio --version 2>/dev/null | head -1)"
if has_tool aws;    then echo "aws:     $(docker exec "$AWS_CID" aws --version 2>&1 | head -1)"; fi
if has_tool rclone; then echo "rclone:  $(docker exec "$RCLONE_CID" rclone version 2>/dev/null | head -1)"; fi
if has_tool pier;   then echo "pier:    $(git rev-parse --short HEAD) (Linux/aarch64, chunk=${PIER_CHUNK_MB}MiB, conc=${PIER_CONCURRENCY})"; fi
echo "파일:    ${SIZE_MB}MiB, 반복: ${RUNS}회, 네트워크: $NETWORK"
echo

for tool in $TOOLS; do
    log "측정: $tool"
    for i in $(seq 1 "$RUNS"); do
        case "$tool" in
            pier)
                # parent는 폴더 키(끝 '/')여야 하므로 슬래시를 붙인다.
                up_cmd="$PIER_BIN put /bench/src/bench.bin $PREFIX/pier/$i/ --replace"
                down_cmd="$PIER_BIN get $PREFIX/pier/$i/bench.bin /bench/dst/pier-$i.bin"
                secs="$(timed "$PIER_CID" bash "$PIER_ENV $up_cmd")"
                ;;
            aws)
                up_cmd="aws --endpoint-url $ENDPOINT s3 cp /bench/src/bench.bin s3://$BUCKET/$PREFIX/aws/$i.bin"
                down_cmd="aws --endpoint-url $ENDPOINT s3 cp s3://$BUCKET/$PREFIX/aws/$i.bin /bench/dst/aws-$i.bin"
                secs="$(timed "$AWS_CID" bash "$up_cmd")"
                ;;
            rclone)
                up_cmd="rclone copyto /bench/src/bench.bin mio:$BUCKET/$PREFIX/rclone/$i.bin"
                down_cmd="rclone copyto mio:$BUCKET/$PREFIX/rclone/$i.bin /bench/dst/rclone-$i.bin"
                secs="$(timed "$RCLONE_CID" sh "$up_cmd")"
                ;;
        esac
        echo "$secs" > "$WORK/res/$tool.up.$i"
        log "  $tool 업로드 $i: ${secs}s ($(mbps "$secs") MB/s)"

        case "$tool" in
            pier)   secs="$(timed "$PIER_CID" bash "$PIER_ENV $down_cmd")" ;;
            aws)    secs="$(timed "$AWS_CID" bash "$down_cmd")" ;;
            rclone) secs="$(timed "$RCLONE_CID" sh "$down_cmd")" ;;
        esac
        echo "$secs" > "$WORK/res/$tool.down.$i"
        log "  $tool 다운로드 $i: ${secs}s ($(mbps "$secs") MB/s)"
    done
done

# 다운로드 파일 크기 검증 (내용 동일성은 고정 소스 + MinIO ETag가 보장)
for f in "$WORK"/dst/*; do
    [ -f "$f" ] || continue
    sz="$(stat -f%z "$f")"
    [ "$sz" = "$SIZE_BYTES" ] || { log "경고: $f 크기 불일치 ($sz != $SIZE_BYTES)"; }
done

echo "== 결과 (MB/s) =="
printf '%-8s %-6s' "도구" "방향"
for i in $(seq 1 "$RUNS"); do printf ' %9s' "실행$i"; done
printf ' %9s\n' "중앙값"
for tool in $TOOLS; do
    for dir in up down; do
        label=$([ "$dir" = up ] && echo 업로드 || echo 다운로드)
        printf '%-8s %-6s' "$tool" "$label"
        vals=()
        for i in $(seq 1 "$RUNS"); do
            s="$(cat "$WORK/res/$tool.$dir.$i")"
            vals+=("$(mbps "$s")")
            printf ' %9s' "$(mbps "$s")"
        done
        printf ' %9s\n' "$(median "${vals[@]}")"
    done
done
