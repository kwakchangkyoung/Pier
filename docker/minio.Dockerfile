# MinIO 서버 이미지를 공식 GitHub 소스에서 직접 빌드한다.
#
# 공식 컨테이너 이미지(minio/minio, minio/mc)는 2026-09경 Docker Hub와 Quay
# 배포가 중단되어 더 이상 pull할 수 없다. 대체 미러를 쓰지 않기 위해
# 고정 릴리스 태그의 소스를 clone해 go build로 만든다.
#
# 빌드 컨텍스트: 이 파일이 있는 docker/ 디렉터리 (compose.yml의 build.context)

# ---- 빌드 스테이지 ----
# MinIO RELEASE.2025-10-15T17-29-55Z (2025-10-15, 현재 최신 릴리스 태그).
# 해당 go.mod 요구: go 1.24.0 / toolchain go1.24.8 → golang:1.25-alpine로 충족.
FROM golang:1.25-alpine AS build

ARG MINIO_TAG=RELEASE.2025-10-15T17-29-55Z

RUN apk add --no-cache git

RUN git clone --depth 1 --branch "${MINIO_TAG}" \
        https://github.com/minio/minio /src

WORKDIR /src
RUN CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" -o /out/minio .

# ---- 런타임 스테이지 ----
FROM alpine:3.22

# healthcheck의 curl과 TLS 통신용 CA 인증서
RUN apk add --no-cache curl ca-certificates

# 비루트 사용자. named volume(/data)은 이미지 내 디렉터리 소유권을 따르므로
# 미리 minio 사용자 소유로 만들어 둔다.
RUN adduser -D -u 1000 minio \
    && mkdir -p /data \
    && chown minio:minio /data

COPY --from=build /out/minio /usr/local/bin/minio

USER minio
VOLUME ["/data"]
ENTRYPOINT ["/usr/local/bin/minio"]
