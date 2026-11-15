#!/usr/bin/env bash
# make-appcast.sh <zip> <version> <ed_key_file>
#
# Sparkle sign_update로 업데이트 아카이브에 EdDSA 서명하고
# 단일 item의 dist/appcast.xml을 생성한다.
#
#   zip          : dist/Pier-<version>.zip (package.sh 산출물)
#   version      : 태그에서 v를 뗀 버전 (예: 0.1.1)
#   ed_key_file  : `generate_keys -x` 형식의 EdDSA 비밀키(base64) 파일
#
# Sparkle 도구(sign_update)는 SPARKLE_BIN 환경변수로 지정한다.
# 비어 있으면 PATH에서 찾는다.
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$(pwd)"
DIST_DIR="${ROOT}/dist"

if [ $# -ne 3 ]; then
    echo "사용법: $0 <zip> <version> <ed_key_file>" >&2
    exit 64
fi

ZIP_PATH="$1"
VERSION="$2"
KEY_FILE="$3"

SIGN_UPDATE="${SPARKLE_BIN:+$SPARKLE_BIN/}sign_update"

for path in "$ZIP_PATH" "$KEY_FILE"; do
    if [ ! -f "$path" ]; then
        echo "오류: 파일이 없습니다: $path" >&2
        exit 1
    fi
done

# XML에 넣을 값 이스케이프. URL·버전·서명에 나올 수 있는 최소 집합만 처리한다.
xml_escape() {
    printf '%s' "$1" | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g' \
        -e 's/"/\&quot;/g' -e "s/'/\&apos;/g"
}

echo "== [1/3] 업데이트 아카이브 서명 =="
SIGN_OUTPUT="$("$SIGN_UPDATE" --ed-key-file "$KEY_FILE" "$ZIP_PATH")"
ED_SIGNATURE="$(printf '%s' "$SIGN_OUTPUT" | sed -n 's/.*sparkle:edSignature="\([^"]*\)".*/\1/p')"
LENGTH="$(printf '%s' "$SIGN_OUTPUT" | sed -n 's/.*length="\([^"]*\)".*/\1/p')"
if [ -z "$ED_SIGNATURE" ] || [ -z "$LENGTH" ]; then
    echo "오류: sign_update 출력에서 서명을 읽지 못했습니다: $SIGN_OUTPUT" >&2
    exit 1
fi

# 서명을 즉시 되검증한다 (키 파일·출력 파싱 오류를 여기서 잡는다).
"$SIGN_UPDATE" --verify --ed-key-file "$KEY_FILE" "$ZIP_PATH" "$ED_SIGNATURE" >/dev/null
echo "서명 검증 통과"

echo "== [2/3] appcast.xml 생성 =="
PUB_DATE="$(LC_ALL=C date -u '+%a, %d %b %Y %H:%M:%S +0000')"
TAG_URL="https://github.com/kwakchangkyoung/Pier/releases/tag/v$(xml_escape "$VERSION")"
ZIP_URL="https://github.com/kwakchangkyoung/Pier/releases/download/v$(xml_escape "$VERSION")/Pier-$(xml_escape "$VERSION").zip"
VERSION_ESC="$(xml_escape "$VERSION")"
ED_SIGNATURE_ESC="$(xml_escape "$ED_SIGNATURE")"

APPCAST_PATH="${DIST_DIR}/appcast.xml"
cat > "$APPCAST_PATH" <<EOF
<?xml version="1.0" encoding="utf-8"?>
<rss version="2.0" xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle" xmlns:dc="http://purl.org/dc/elements/1.1/">
    <channel>
        <title>Pier 업데이트</title>
        <link>https://github.com/kwakchangkyoung/Pier/releases/latest/download/appcast.xml</link>
        <description>Pier 최신 버전을 알리는 업데이트 피드입니다.</description>
        <language>ko</language>
        <item>
            <title>Pier ${VERSION_ESC}</title>
            <pubDate>${PUB_DATE}</pubDate>
            <sparkle:releaseNotesLink>${TAG_URL}</sparkle:releaseNotesLink>
            <sparkle:minimumSystemVersion>14.0</sparkle:minimumSystemVersion>
            <enclosure url="${ZIP_URL}"
                sparkle:version="${VERSION_ESC}"
                sparkle:shortVersionString="${VERSION_ESC}"
                sparkle:edSignature="${ED_SIGNATURE_ESC}"
                length="${LENGTH}"
                type="application/octet-stream"/>
        </item>
    </channel>
</rss>
EOF

echo "== [3/3] XML 검증 =="
xmllint --noout "$APPCAST_PATH"
echo "생성: $APPCAST_PATH"
