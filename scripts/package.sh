#!/usr/bin/env bash
# Pier 배포 패키지 생성 (서명 없이 배포 — ad-hoc 서명)
# 절차:
#   1) ./scripts/build-core.sh          Rust 코어 release 빌드 + XCFramework/바인딩
#   2) xcodegen generate                Xcode 프로젝트 재생성
#   3) xcodebuild Release               서명 없이 빌드 (dist/build 고정 경로)
#   4) codesign --sign -                ad-hoc 서명 + 검증
#   5) hdiutil create                   dist/Pier-<버전>.dmg (앱 + /Applications 링크, UDZO)
#   6) DMG 크기·sha256 출력
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$(pwd)"

APP_NAME="Pier"
DIST_DIR="${ROOT}/dist"
BUILD_DIR="${DIST_DIR}/build"
DMG_STAGE="${DIST_DIR}/dmg-root"

echo "== [1/6] Rust 코어 빌드 (release) =="
./scripts/build-core.sh

echo "== [2/6] xcodegen generate =="
(cd apple && xcodegen generate)

echo "== [3/6] xcodebuild Release (CODE_SIGNING_ALLOWED=NO) =="
xcodebuild -project apple/Pier.xcodeproj \
    -scheme Pier \
    -configuration Release \
    -derivedDataPath "$BUILD_DIR" \
    CODE_SIGNING_ALLOWED=NO \
    build

APP_PATH="${BUILD_DIR}/Build/Products/Release/${APP_NAME}.app"
if [ ! -d "$APP_PATH" ]; then
    echo "오류: 빌드 산출물이 없습니다: ${APP_PATH}" >&2
    exit 1
fi

echo "== [4/6] DMG 스테이징 + ad-hoc 서명 =="
rm -rf "$DMG_STAGE"
mkdir -p "$DMG_STAGE"
ditto "$APP_PATH" "${DMG_STAGE}/${APP_NAME}.app"
ln -s /Applications "${DMG_STAGE}/Applications"

codesign --force --deep --sign - "${DMG_STAGE}/${APP_NAME}.app"
codesign --verify --deep --strict --verbose=2 "${DMG_STAGE}/${APP_NAME}.app"
echo "ad-hoc 서명 검증 통과"

echo "== [5/6] DMG 생성 (UDZO) =="
VERSION="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' \
    "${DMG_STAGE}/${APP_NAME}.app/Contents/Info.plist")"
DMG_PATH="${DIST_DIR}/${APP_NAME}-${VERSION}.dmg"
rm -f "$DMG_PATH"
hdiutil create -volname "$APP_NAME" -srcfolder "$DMG_STAGE" -format UDZO -ov "$DMG_PATH"

echo "== [6/6] 결과 =="
ls -lh "$DMG_PATH"
shasum -a 256 "$DMG_PATH"
