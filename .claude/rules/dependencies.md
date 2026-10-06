---
paths:
  - "Cargo.toml"
  - "xtask/Cargo.toml"
  - "Cargo.lock"
  - ".clippy.toml"
  - ".cargo/config.toml"
  - "vendor/**"
  - "xtask/src/vendor.rs"
  - "xtask/data/vendor/**"
  - "xtask/tests/*_repro.rs"
  - "xtask/tests/downstream.rs"
  - "xtask/go/go.mod"
  - "xtask/go/extract/**"
  - "xtask/src/gen.rs"
  - "xtask/src/gen/**"
  - "xtask/src/dump.rs"
  - "xtask/src/dump/**"
  - "xtask/php/**"
---
# 의존성 규칙

## 들이기와 올리기
- 들이는 조건(`CLAUDE.md`)의 정답은 표준, 기준 서버, 시험 벡터다.
- 들이는 모든 것은 아래 외부 불신 절차를 거친다. 결함이 남으면 고친 판 고정, 가드, 사본 수정 가운데 하나로 없앤 뒤에만 쓴다.
- 판을 올릴 때마다 외부 불신 절차를 다시 거친다.
- Anchor는 라이브러리라 전역 할당기와 빌드 프로필을 정하지 않는다.
- 다음은 크레이트를 들이지 않고 직접 구현을 쓴다: RakNet, Snappy, 시그널링 HTTP, LevelDB 리더(가져오기), 오류 타입, CRC, base64, UUID, NBT, 결정적 수학(`sin`, `cos`, `atan2` 등).
- std 삼각함수는 쓰지 않는다.

## 외부 불신 절차
- `CLAUDE.md`의 의존성 검토(쓰는 부분 직접 읽기, 결함 제거 순서)를 크레이트뿐 아니라 Go 모듈, 데이터 출처, 문서 등 밖에서 가져오는 모든 것에 적용한다.
- 분석할 때 알려진 이슈와 권고를 찾고, 경계 조건(패닉, 넘침, 할당, 숨은 스레드, 결정성)을 시험한다.
- 찾은 결함과 조치를 `.claude/docs/dependencies.md`의 표나 사본 절에 적는다.

## 가드
- 가드의 기본 수단은 `.clippy.toml`의 `disallowed-methods`와 `disallowed-types`다.
- 들인 의존성의 시계, 난수, 상한 없는 큐, 블로킹 호출, 상한 없는 할당 진입점은 들이는 변경에서 금지 목록에 넣는다.
- 넣은 금지가 실제로 걸리는지 임시 코드로 확인한다.

## vendor 사본
- 상류가 아직 고치지 않은 결함은 `vendor/<크레이트>/`에 사본을 두고 고친다.
- 결함만 고친다. 기능을 더하거나 다듬지 않는다.
- 고친 곳과 이유를 `.claude/docs/dependencies.md`의 사본 절에 적고, 코드에도 `Anchor` 주석을 단다.
- 사본은 crates.io의 고정 판에서 시작한다. 고친 곳과 다른 사본을 가리키는 `path` 줄만 원본과 다르게 둔다.
- 상류 릴리스가 결함을 고치면 그 고침을 사본에서 뺀다. 남은 고침이 없으면 사본을 지우고 릴리스로 돌아간다.
- 새 상류 판이 나오면 외부 불신 절차로 검토해 올린다.
- 사본은 `[patch]`가 아니라 `path` 의존성으로 둔다.
- 사본은 작업 공간 멤버로 두지 않는다(`exclude = ["vendor"]`).
- str0m 통합 시험의 캡처 파일은 커밋하지 않는다. str0m 사본을 바꾸면 `cargo xtask vendor-test str0m`을 돌린다.
- dimpl 판을 올릴 때마다 풀 밖 버퍼가 반납되는 길이 없는지 다시 본다.
- str0m-proto, str0m-aws-lc-rs, is 사본은 str0m 릴리스로 돌아갈 때 함께 지운다.

## 빌드 설정
- 저장소의 빌드 설정에는 같은 결과에 시간만 줄이는 것만 둔다.
- 의존성 `opt-level = 1`과 sccache를 쓰지 않는다.

## 저작권
- 다른 서버와 라이브러리의 출력 형식, 주석, 코드 덩어리도 옮기지 않는다.
- 서버가 만드는 모든 문장(콘솔 로그, 명령 출력, 킥 사유, 오류 메시지, status 출력 등)은 Anchor 고유 문장으로 쓴다.
- 프로토콜이 요구하는 형식(핑 응답 필드 순서 등)은 형식이 아니라 규약이라 그대로 따른다.
- 생성 코드의 문서는 생성기가 IR의 정보(이름, 패킷 ID와 방향, 와이어 형식, 상한)로 쓴다.
- 추출기는 gophertunnel 주석을 읽지 않는다.
- 데이터에는 사실(값, 번호, 상한)만 쓴다.
- Mojang 프로토콜 문서는 숫자만 옮기고 파일을 복사하지 않는다.
- 덤프 데이터는 덤프 원본을 실행해 얻는다.

배경과 자세한 사실은 `.claude/docs/dependencies.md`.
