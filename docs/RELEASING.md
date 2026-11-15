# 릴리스

Pier는 GitHub Actions가 태그 푸시로 자동 릴리스한다. 앱은 Sparkle로 자동 업데이트를
확인하므로, 각 릴리스에는 `Pier-<버전>.dmg`(수동 설치용), `Pier-<버전>.zip`(Sparkle
업데이트용), `appcast.xml`(업데이트 피드) 세 자산이 올라간다.

## 새 버전 배포

```bash
git tag v0.1.1
git push origin v0.1.1
```

태그를 푸시하면 `.github/workflows/release.yml`이 다음을 수행한다.

1. `PIER_VERSION=<태그에서 v를 뗀 값>`으로 `./scripts/package.sh`를 실행해
   Rust 코어 → 앱 빌드 → ad-hoc 서명 → `dist/Pier-<버전>.dmg`와 `dist/Pier-<버전>.zip` 생성.
   이때 앱의 `CFBundleShortVersionString`/`CFBundleVersion`도 태그 버전으로 교체된다.
2. Sparkle 2.10.1 도구 tarball을 내려받아 SHA-256을 검증한 뒤 압축 해제한다.
3. GitHub Secret `SPARKLE_ED_PRIVATE_KEY`로 `sign_update` 서명을 만들어
   `dist/appcast.xml`을 생성하고, 서명과 공개키(Info.plist의 `SUPublicEDKey`) 일치를 검증한다.
4. `gh release create v<버전>`으로 세 자산을 올리고 릴리스 노트를 자동 생성한다.

릴리스가 올라가면 `…/releases/latest/download/appcast.xml`이 최신 피드를
가리키게 되고, 설치된 앱들이 하루 간격 확인 또는 메뉴 "업데이트 확인…"으로
새 버전을 발견한다.

## 시험 실행

실제 릴리스 없이 파이프라인만 검증하려면 Actions 탭의 Release 워크플로를
`workflow_dispatch`로 실행한다. `version`에 `0.1.1-test` 같은 값을 넣고
`dry_run`을 켜두면(기본값) 릴리스 대신 산출물이 workflow artifact로 올라간다.
`dry_run`을 끄면 실행한 커밋에 `v<version>` 태그를 만들어 실제 릴리스를 생성한다.

## 업데이트 서명 키

- **공개키**: 앱 `Info.plist`의 `SUPublicEDKey`에 들어 있다. 유출돼도 문제없다.
- **비밀키**: 두 곳에만 둔다.
  - GitHub Secret `SPARKLE_ED_PRIVATE_KEY` (CI 서명용, `generate_keys -x` 출력 형식)
  - 관리자 로그인 키체인 (Sparkle `generate_keys`가 만든 항목, account `com.gajago.pier`)

**비밀키를 분실하면 기존 설치본이 새 업데이트의 서명을 검증할 수 없어 자동
업데이트가 끊긴다.** 안전한 곳에 백업해 둘 것:

```bash
# 키체인의 비밀키를 파일로 내보내기 (파일에는 비밀키가 평문으로 들어간다)
<Sparkle>/bin/generate_keys --account com.gajago.pier -x <내보낼 파일>
```

내보낸 파일은 암호화된 저장소(예: 1Password 문서, 암호화 볼륨)에 보관하고
로컬에는 남기지 않는다.

## 알려진 제약

- 배포본은 ad-hoc 서명이므로 업데이트로 교체된 앱을 처음 실행할 때 키체인
  접근 허용 대화상자가 다시 뜰 수 있다.
- GitHub Releases의 `latest` 리다이렉트는 **최신으로 생성된 릴리스**를 가리킨다.
  예전 버전 태그를 나중에 푸시하면 `latest`가 그 릴리스를 가리켜 자동 업데이트가
  새 버전을 못 찾을 수 있으니, 태그는 항상 최신 버전부터 순서대로 푸시한다.
