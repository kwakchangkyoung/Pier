#!/usr/bin/env bash
# MinIO pier-test 버킷 생성 및 테스트 객체 시드 (멱등 — 여러 번 실행해도 안전)
#
# 호스트에 aws-cli/mc를 요구하지 않고, compose 네트워크에 amazon/aws-cli
# 일회용 컨테이너를 띄워 S3 API를 호출한다. (MinIO 공식 이미지 배포 중단으로
# mc 계열 이미지를 쓰지 않고 aws-cli만 사용한다.)
set -euo pipefail

cd "$(dirname "$0")/.."

COMPOSE_FILE="docker/compose.yml"
AWSCLI_IMAGE="amazon/aws-cli:2.40.1"
BUCKET="pier-test"
ENDPOINT_URL="http://minio:9000"
# compose 네트워크 내부에서 서비스 이름으로 접근

MINIO_CID="$(docker compose -f "$COMPOSE_FILE" ps -q minio)"
if [ -z "$MINIO_CID" ]; then
    echo "오류: minio 컨테이너가 없습니다. 먼저 'docker compose -f $COMPOSE_FILE up -d'를 실행하세요." >&2
    exit 1
fi

NETWORK="$(docker inspect -f '{{range $k, $v := .NetworkSettings.Networks}}{{$k}}{{"\n"}}{{end}}' "$MINIO_CID" | head -n1)"
if [ -z "$NETWORK" ]; then
    echo "오류: minio 컨테이너의 네트워크를 찾을 수 없습니다." >&2
    exit 1
fi

# MinIO가 healthy가 될 때까지 최대 60초 대기
HEALTH=""
for _ in $(seq 1 60); do
    HEALTH="$(docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}' "$MINIO_CID" 2>/dev/null || true)"
    if [ "$HEALTH" = "healthy" ]; then
        break
    fi
    sleep 1
done
if [ "$HEALTH" != "healthy" ]; then
    echo "오류: MinIO가 healthy 상태가 아닙니다 (상태: ${HEALTH:-unknown})." >&2
    echo "      docker compose -f $COMPOSE_FILE logs minio 로 로그를 확인하세요." >&2
    exit 1
fi

# 시드 파일은 호스트 임시 디렉터리에 만들고 컨테이너에 읽기 전용으로 마운트한다.
SEED_DIR="$(mktemp -d /tmp/pier-seed.XXXXXX)"
trap 'rm -rf "$SEED_DIR"' EXIT

mkdir -p "$SEED_DIR/photos/2024" "$SEED_DIR/docs"

printf 'Hello from Pier!\n' > "$SEED_DIR/hello.txt"
printf '안녕하세요, Pier 테스트 파일입니다.\n' > "$SEED_DIR/한글 파일.txt"
printf 'special chars & (paren) [bracket] +plus=equal .txt\n' > "$SEED_DIR/with space & special(1).txt"
printf '\xff\xd8\xff\xe0Pier dummy jpeg bytes\n' > "$SEED_DIR/photos/a.jpg"
head -c 512 /dev/urandom >> "$SEED_DIR/photos/a.jpg"
printf '\xff\xd8\xff\xe0Pier dummy jpeg bytes\n' > "$SEED_DIR/photos/2024/b.jpg"
head -c 512 /dev/urandom >> "$SEED_DIR/photos/2024/b.jpg"
printf '%%PDF-1.4\nPier dummy pdf bytes\n' > "$SEED_DIR/docs/report.pdf"
head -c 512 /dev/urandom >> "$SEED_DIR/docs/report.pdf"

aws_run() {
    docker run --rm \
        --network "$NETWORK" \
        -v "$SEED_DIR:/seed:ro" \
        -e AWS_ACCESS_KEY_ID=minioadmin \
        -e AWS_SECRET_ACCESS_KEY=minioadmin \
        -e AWS_DEFAULT_REGION=us-east-1 \
        -e AWS_EC2_METADATA_DISABLED=true \
        "$AWSCLI_IMAGE" \
        --endpoint-url "$ENDPOINT_URL" "$@"
}

# 버킷 생성 (이미 있으면 건너뜀)
if aws_run s3api head-bucket --bucket "$BUCKET" >/dev/null 2>&1; then
    echo "버킷 이미 존재: $BUCKET"
else
    echo "버킷 생성: $BUCKET"
    aws_run s3 mb "s3://${BUCKET}"
fi

# 객체 업로드 (s3 cp는 매번 덮어쓰므로 멱등)
aws_run s3 cp "/seed/hello.txt" "s3://${BUCKET}/hello.txt"
aws_run s3 cp "/seed/한글 파일.txt" "s3://${BUCKET}/한글 파일.txt"
aws_run s3 cp "/seed/with space & special(1).txt" "s3://${BUCKET}/with space & special(1).txt"
aws_run s3 cp "/seed/photos/a.jpg" "s3://${BUCKET}/photos/a.jpg"
aws_run s3 cp "/seed/photos/2024/b.jpg" "s3://${BUCKET}/photos/2024/b.jpg"
aws_run s3 cp "/seed/docs/report.pdf" "s3://${BUCKET}/docs/report.pdf"

# 0바이트 폴더 마커 객체 (키가 '/'로 끝남)
aws_run s3api put-object --bucket "$BUCKET" --key "empty-folder/" >/dev/null

echo
echo "== ${BUCKET} 루트 목록 =="
aws_run s3 ls "s3://${BUCKET}"
