#!/usr/bin/env bash
# Pier 배포 패키지 생성 (서명 없이 배포 — ad-hoc 서명)
# 절차:
#   1) ./scripts/build-core.sh          Rust 코어 release 빌드 + XCFramework/바인딩
#   2) xcodegen generate                Xcode 프로젝트 재생성
#   3) xcodebuild Release               서명 없이 빌드 (dist/build 고정 경로)
#   4) 버전 덮어쓰기                    PIER_VERSION이 있으면 Info.plist 버전을 교체
#   5) codesign --sign -                ad-hoc 서명 + 검증
#   6) hdiutil create                   dist/Pier-<버전>.dmg (앱 + /Applications 링크, UDZO)
#   7) ditto                            dist/Pier-<버전>.zip (Sparkle 업데이트용)
#   8) 산출물 크기·sha256 출력
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$(pwd)"

APP_NAME="Pier"
DIST_DIR="${ROOT}/dist"
BUILD_DIR="${DIST_DIR}/build"
DMG_STAGE="${DIST_DIR}/dmg-root"

echo "== [1/8] Rust 코어 빌드 (release) =="
./scripts/build-core.sh

echo "== [2/8] xcodegen generate =="
(cd apple && xcodegen generate)

echo "== [3/8] xcodebuild Release (CODE_SIGNING_ALLOWED=NO) =="
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

echo "== [4/8] DMG 스테이징${PIER_VERSION:+ + 버전 ${PIER_VERSION} 적용} =="
rm -rf "$DMG_STAGE"
mkdir -p "$DMG_STAGE"
ditto "$APP_PATH" "${DMG_STAGE}/${APP_NAME}.app"
ln -s /Applications "${DMG_STAGE}/Applications"

# CI의 태그 릴리스처럼 PIER_VERSION이 주어지면 빌드 산출물의 버전을 덮어쓴다.
# CFBundleVersion과 CFBundleShortVersionString을 같은 값으로 둔다.
if [ -n "${PIER_VERSION:-}" ]; then
    /usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString ${PIER_VERSION}" \
        "${DMG_STAGE}/${APP_NAME}.app/Contents/Info.plist"
    /usr/libexec/PlistBuddy -c "Set :CFBundleVersion ${PIER_VERSION}" \
        "${DMG_STAGE}/${APP_NAME}.app/Contents/Info.plist"
fi

echo "== [5/8] ad-hoc 서명 =="
codesign --force --deep --sign - "${DMG_STAGE}/${APP_NAME}.app"
codesign --verify --deep --strict --verbose=2 "${DMG_STAGE}/${APP_NAME}.app"
echo "ad-hoc 서명 검증 통과"

VERSION="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' \
    "${DMG_STAGE}/${APP_NAME}.app/Contents/Info.plist")"

echo "== [6/8] DMG 생성 (UDZO) =="
DMG_PATH="${DIST_DIR}/${APP_NAME}-${VERSION}.dmg"
rm -f "$DMG_PATH"
hdiutil create -volname "$APP_NAME" -srcfolder "$DMG_STAGE" -format UDZO -ov "$DMG_PATH"

echo "== [7/8] ZIP 생성 (Sparkle 업데이트용) =="
ZIP_PATH="${DIST_DIR}/${APP_NAME}-${VERSION}.zip"
rm -f "$ZIP_PATH"
# --keepParent로 앱 번들이 zip 루트에 그대로 들어가게 한다.
(cd "$DMG_STAGE" && ditto -c -k --sequesterRsrc --keepParent "${APP_NAME}.app" "$ZIP_PATH")

echo "== [8/8] 결과 =="
ls -lh "$DMG_PATH" "$ZIP_PATH"
shasum -a 256 "$DMG_PATH" "$ZIP_PATH"
