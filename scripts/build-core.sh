#!/usr/bin/env bash
# pier-ffi(Rust/UniFFI) → XCFramework + Swift 바인딩 생성
# 결과물:
#   apple/Frameworks/PierCore.xcframework  (macos-arm64 staticlib)
#   apple/Generated/pier_ffi.swift         (Swift 바인딩)
set -euo pipefail

cd "$(dirname "$0")/.."

# SPEC 최소 지원은 macOS 14. 이 변수를 설정하지 않으면 rustc가 호스트 SDK
# 버전(현재 27.2)을 오브젝트에 기록해, macOS 14 타깃으로 링크하는 앱 빌드에서
# ld 경고가 수백 건 발생한다.
export MACOSX_DEPLOYMENT_TARGET=14.0

TARGET="aarch64-apple-darwin"
LIB_DIR="target/${TARGET}/release"
DYLIB="${LIB_DIR}/libpier_ffi.dylib"
STATICLIB="${LIB_DIR}/libpier_ffi.a"

echo "== [1/4] cargo build -p pier-ffi --release --target ${TARGET} =="
cargo build -p pier-ffi --release --target "$TARGET"

if [ ! -f "$DYLIB" ]; then
    echo "오류: ${DYLIB} 이(가) 없습니다. pier-ffi의 crate-type에 cdylib이 있는지 확인하세요." >&2
    exit 1
fi
if [ ! -f "$STATICLIB" ]; then
    echo "오류: ${STATICLIB} 이(가) 없습니다. pier-ffi의 crate-type에 staticlib이 있는지 확인하세요." >&2
    exit 1
fi

GEN_DIR="$(mktemp -d)"
trap 'rm -rf "$GEN_DIR"' EXIT

echo "== [2/4] Swift 바인딩 생성 (uniffi-bindgen) =="
cargo run -p pier-ffi --bin uniffi-bindgen -- generate \
    --library "$DYLIB" \
    --language swift \
    --out-dir "$GEN_DIR"

mkdir -p apple/Generated
cp "${GEN_DIR}/pier_ffi.swift" apple/Generated/pier_ffi.swift
echo "생성: apple/Generated/pier_ffi.swift"

echo "== [3/4] FFI 헤더 디렉터리 구성 =="
HEADERS_DIR="${GEN_DIR}/headers"
mkdir -p "$HEADERS_DIR"
cp "${GEN_DIR}/pier_ffiFFI.h" "${HEADERS_DIR}/pier_ffiFFI.h"
cp "${GEN_DIR}/pier_ffiFFI.modulemap" "${HEADERS_DIR}/module.modulemap"

echo "== [4/4] XCFramework 생성 =="
# Command Line Tools만 선택된 상태에서는 xcodebuild가 실패한다.
# 바인딩 생성까지는 Xcode 없이도 끝났으므로 여기서 안내 후 종료한다.
if ! xcodebuild -version >/dev/null 2>&1; then
    cat >&2 <<'EOF'
오류: xcodebuild를 사용할 수 없습니다 (Command Line Tools만 선택된 상태로 보입니다).
Xcode를 활성화한 뒤 다시 실행하세요:
    sudo xcode-select -s /Applications/Xcode.app
EOF
    exit 1
fi

rm -rf apple/Frameworks/PierCore.xcframework
xcodebuild -create-xcframework \
    -library "$STATICLIB" \
    -headers "$HEADERS_DIR" \
    -output apple/Frameworks/PierCore.xcframework

echo "완료: apple/Frameworks/PierCore.xcframework"
