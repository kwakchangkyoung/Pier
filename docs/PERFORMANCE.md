# Pier 전송 성능 측정 (Phase 5)

SPEC 1장 성능 목표 "대용량 파일(1GB 이상) 다운로드·업로드: `aws s3 cp` 대비 처리량
90% 이상"에 대한 로컬 MinIO 기준 측정 결과다. 측정은 `scripts/bench.sh`로
자동화되어 있으며 아래 결과는 2026-11-13에 실제 실행한 값이다.

## 판정

| 방향 | Pier | aws s3 cp | Pier / aws | 목표(90%) |
|---|---:|---:|---:|:---:|
| 업로드 | 454.3 MB/s | 264.9 MB/s | **171.5%** | 충족 |
| 다운로드 | 681.3 MB/s | 473.0 MB/s | **144.0%** | 충족 |

로컬 MinIO 조건에서 Pier는 `aws s3 cp`를 업로드·다운로드 모두 상회한다
(중앙값 기준). 단, 아래 "한계"에 적은 대로 실제 클라우드 대상 측정은 별도로
필요하다.

## 측정 환경

| 항목 | 값 |
|---|---|
| 맥 | MacBook Air (MacBookAir10,1), Apple M1, 16GB |
| macOS | 27.0.1 (26A434) |
| Docker | Docker Desktop, Engine 29.9.3, 8 CPU, 7.75GiB |
| MinIO | `pier-minio:RELEASE.2025-10-15T17-29-55Z` (docker/minio.Dockerfile 소스 빌드) |
| Pier | 워킹 트리 기준 커밋 `47af6e8` + 진행 중 변경, Linux/aarch64 release 빌드 |
| Pier 전송 설정 | 기본값 — 청크 8MiB, 동시성 8 |
| aws-cli | `amazon/aws-cli:2.40.1` (기본 multipart 8MiB, 최대 동시 요청 10) |
| rclone | `rclone/rclone:1.71.2` (기본 chunk 5MiB, upload_concurrency 4, transfers 4) |

## 측정 방법

- 파일: 1GiB(1,073,741,824B) 고엔트로피 결정적 데이터. 고정 키 AES-256-CTR
  스트림을 파일로 저장해 매 실행 내용이 같고 서버 측 압축·중복제거로 속도가
  부풀려지지 않는다.
- 절차: 도구별로 업로드 3회 → 다운로드 3회, 각 회 고유 키/파일 사용
  (`s3://pier-test/docs/p5-perf/<도구>/<회차>/`). 측정 후 업로드 객체와 로컬
  파일은 전부 삭제한다.
- 공정성: Pier CLI는 호스트 프로세스라 `127.0.0.1:9000`(Docker Desktop 포트
  포워더) 경유 경로를 타고, 비교 도구(컨테이너)는 `pier_default` 브리지로
  MinIO에 직결되어 네트워크 경로가 다르다. 이를 맞추기 위해 **Pier도 같은
  네트워크의 컨테이너 안에서** 실행한다 — `rust:1.99.1-bookworm` 이미지에서
  `cargo build --release -p pier-core --example cli`로 빌드한 Linux 바이너리.
  결과적으로 세 도구 모두 컨테이너→MinIO 경로를 쓰고, 읽기·쓰기 대상도 같은
  호스트 디렉터리의 바인드 마운트(virtiofs)라 디스크 경로도 동일하다.
- 시간 측정: `docker exec`로 상주 컨테이너 안에서 `time`을 실행해 컨테이너
  기동·exec 오버헤드를 제외하고 전송 명령 시간만 잰다. MB/s는 MiB/s다.
- 재현: `./scripts/bench.sh` (크기 `BENCH_SIZE_MB`, 반복 `BENCH_RUNS`, 도구
  `BENCH_TOOLS`, Pier 설정 `PIER_CHUNK_MB`/`PIER_CONCURRENCY`로 조정 가능).

## 원시 결과 (1GiB, 3회, MB/s)

| 도구 | 방향 | 실행1 | 실행2 | 실행3 | 중앙값 |
|---|---|---:|---:|---:|---:|
| pier | 업로드 | 479.0 | 326.0 | 454.3 | 454.3 |
| pier | 다운로드 | 726.8 | 676.4 | 681.3 | 681.3 |
| aws | 업로드 | 271.0 | 264.9 | 217.9 | 264.9 |
| aws | 다운로드 | 504.4 | 473.0 | 447.6 | 473.0 |
| rclone | 업로드 | 120.0 | 143.8 | 146.9 | 143.8 |
| rclone | 다운로드 | 256.6 | 269.5 | 266.7 | 266.7 |

(각 실행 시간 — pier 업로드 2.14/3.14/2.25s, 다운로드 1.41/1.51/1.50s;
aws 업로드 3.78/3.87/4.70s, 다운로드 2.03/2.17/2.29s;
rclone 업로드 8.53/7.12/6.97s, 다운로드 3.99/3.80/3.84s)

## 해석

- Pier의 병렬 범위 읽기(다운로드)와 멀티파트 업로드는 기본값(8MiB/8)으로도
  로컬 MinIO에서 aws-cli 기본값(8MiB 멀티파트, 동시 요청 10)을 상회한다.
- rclone은 기본 업로드 동시성이 4로 낮아 이 조건에서 가장 느리다. 튜닝하면
  달라질 수 있으나 이번 측정은 "설정 없이 빠른" 기본값 비교가 목적이다.
- 실행 간 변동(pier 업로드 326~479MB/s)은 Docker VM 내부 스케줄링·virtiofs
  캐시 영향으로 보이며, 중앙값으로 완충했다.

## 한계와 후속 측정

- **로컬 MinIO 측정이다.** 모든 트래픽이 Docker VM 내부를 돌아 실제
  클라우드(AWS S3, R2, Wasabi)의 지연·대역폭 조건과 다르다. SPEC 성능 목표의
  최종 검증은 실제 클라우드에서 필요하다.
- Pier는 실제 앱이 쓰는 macOS 바이너리가 아니라 동일 소스의 Linux 빌드로
  측정했다. 비교 조건을 맞추기 위한 선택이며, 절대값은 앱과 다를 수 있다.
- 실제 클라우드 측정 방법(사용자 수동): 같은 리전 버킷에 대해 호스트에서
  `PIER_ENDPOINT=https://s3.<region>.amazonaws.com PIER_PATH_STYLE=false
  PIER_BUCKET=<버킷> PIER_ACCESS_KEY/PIER_SECRET_KEY=<키>`를 지정하고
  `cargo run --release -p pier-core --example cli -- put/get`을 실행,
  같은 파일로 `aws s3 cp`와 번갈아 3회씩 측정해 중앙값을 비교한다.
  청크·동시성은 `PIER_CHUNK_MB`/`PIER_CONCURRENCY`로 맞춰 볼 수 있다.
- 업로드 편차가 큰 경우 원인 후보로 virtiofs 바인드 마운트 읽기 지연,
  Docker VM CPU 스로틀, 멀티파트 파트 크기(8MiB 고정) 등을 볼 수 있다.
  필요하면 `PIER_CHUNK_MB=16 PIER_CONCURRENCY=8 ./scripts/bench.sh`처럼
  환경변수로 조합을 바꿔 같은 스크립트로 재측정할 수 있다.
