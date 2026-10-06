---
paths:
  - "src/protocol.rs"
  - "src/protocol/**"
  - "src/nbt.rs"
  - "xtask/src/gen.rs"
  - "xtask/src/gen/body.rs"
  - "xtask/src/gen/consts.rs"
  - "xtask/src/gen/emit.rs"
  - "xtask/src/gen/ir.rs"
  - "xtask/src/gen/model.rs"
  - "xtask/src/gen/names.rs"
  - "xtask/src/dump.rs"
  - "xtask/src/dump/limits.rs"
  - "xtask/data/protocol.json"
  - "xtask/data/reference/protocol_limits.json"
  - "xtask/go/go.mod"
  - "xtask/go/go.sum"
  - "xtask/go/extract/**"
  - "xtask/go/oracle/**"
  - "xtask/go/bot/issuer.go"
  - "xtask/go/bot/login.go"
  - "xtask/tests/protocol.rs"
  - "xtask/tests/compression.rs"
  - "xtask/tests/spawn.rs"
  - "xtask/tests/go.rs"
---
# 프로토콜 규칙

## 정답 출처
- 프로토콜 사실의 근거는 gophertunnel 코드, Mojang 문서, 공식 서버를 실행해 봇으로 관찰한 값뿐이다.
- `/versionup`은 쓰지 않는다.
- gophertunnel이 공식 서버 대조(P6)와 다르면 외부 불신 절차로 처리한다(`.claude/docs/dependencies.md`).

## 생성 코드
- 패킷 코덱은 고정한 gophertunnel에서 뽑은 IR로 생성한다.
- 추출기(`xtask/go/extract`)는 모르는 구문이 하나라도 있으면 실패한다. 건너뛰지 않는다.
- 추출기는 주석을 읽지 않는다. 생성 코드의 문서는 생성기가 IR의 자기 정보로 쓰고, gophertunnel 주석을 옮기지 않는다.
- IR(`xtask/data/protocol.json`)은 커밋한다.
- 틀린 생성 출력은 생성기(`xtask/src/gen*`)나 IR 입력을 고친다.
- 쓰지 않는 IR 상수는 이유 표(`xtask/src/gen/consts.rs`)에 적는다.
- 이름 규칙은 `xtask/src/gen/names.rs` 한 곳에 둔다. 규칙에 맞지 않는 이름은 같은 파일의 예외 표에 적는다.

## Rust 표현
- Go 구조체는 같은 모양의 평평한 struct로 생성한다. 조건부 필드도 늘 두고, 없으면 `Default`다. 모두 `Debug, Clone, PartialEq, Default`를 단다.
- 더 나은 공개 모양은 상위 API가 감싼다.
- 문자열은 UTF-8이 아니면 디코드 오류를 낸다. bool은 0과 1만 받는다.
- 쓰기는 길이를 조용히 자르지 않고 오류를 낸다(인코딩도 `Result`).
- NBT 목록(`nbt::List`)은 빈 목록도 원소 타입을 늘 지닌다.
- NBT 중첩은 스레드 스택이 아니라 힙 스택에 둔다.
- `anchor::protocol`은 공개 코덱 층이다: 패킷과 타입, `Packet` 디코드·인코드, 배치 인코드·디코드, `Compression`. `Reader`, `Writer`, `Codec`은 `pub(crate)`로 둔다. NBT는 `anchor::nbt`에 둔다.
- union의 변형 구조체와 ID 표는 생성하고, 태그를 읽고 쓰는 틀만 손으로 쓴다.

## 디코드 상한
- 모든 디코드 상한은 `Reader::limit` 하나를 지나고, 넘으면 크기 오류(`Error::Size`)다.
- 모든 `Vec`, `String`, NBT 할당은 할당 전에 `개수 × size_of`를 Reader 예산에서 먼저 뺀다. 중첩 Reader는 같은 예산을 나눠 쓴다.
- 필드별 상한은 Mojang bedrock-protocol-docs JSON 스키마의 `maxLength`, `maxItems`, `maxProperties`를 쓴다.
- gen은 필드 상한을 디코더와 인코더 모두에 넣는다.
- 상한마다(길이, 개수, 예산, NBT 깊이·크기, 해제 크기, 배치 패킷 수, 필드 상한) 넘는 선언이 크기 오류로 끝나는 시험(P4)을 둔다.

## gophertunnel 결함 조치
- 고정한 gophertunnel의 결함은 Anchor에서 고치고, 같은 형식을 유지해야 하는 곳은 시험 표본을 맞춘다.
- MaterialReducer는 `mix >> 16`, `mix & 0xffff`로 읽고, 범위를 넘는 쓰기는 오류다.
- ordinal 없는 Event 타입은 쓰지 않고 오류로 낸다.
- gophertunnel이 쓸 때 덮어쓰거나 읽고 버리는 값은 생성 코드에서 gophertunnel과 같게 두고, 클라이언트 패킷을 처리하는 틱 코드가 검사한다.
- EntityMetadata 개수, NBT 깊이 계수, 부분 읽기, ItemInstance·Login 길이는 상한과 오류로 막는다.
- Snappy 해제는 표준 Snappy만 받는다.

## 시험
- Go 도구는 개발 때만 쓴다. 모든 시험은 `cargo test`로 돌고 `cargo xtask verify`에 들어가게 한다.
- 오라클 코퍼스는 커밋하지 않는다.
- 봇의 온라인 로그인은 시험용 발급자로 검증하고, 실제 Xbox 토큰은 쓰지 않는다.

## 버전업 절차
새 프로토콜 버전으로 옮길 때 차례대로 한다. 단계마다 앞 단계가 통과해야 넘어간다.
1. 출처 고르기: gophertunnel의 최신 정식 태그를 고른다(프리뷰 브랜치는 쓰지 않는다). 태그 뒤 master의 커밋을 읽고 같은 프로토콜의 형식 수정이 있으면 그 커밋으로 고정한다. go-raknet, go-nethernet은 그 go.mod를 따라간다.
2. 외부 불신 검토: 바뀐 Go 코드(`protocol`, `packet`, `login`, 압축, go-raknet, go-nethernet)를 직접 읽고 결함을 적고 없앤다(`.claude/docs/dependencies.md`). gophertunnel 결함 목록(`.claude/docs/protocol.md`)이 아직 맞는지 다시 본다.
3. 고정: Go 쪽 버전은 `xtask/go/go.mod`(와 `go.sum`)에 의사 버전으로 고정한다.
4. 생성: `cargo xtask gen`. 추출이 모르는 구문으로 실패하면 추출기를 넓힌다. IR diff(`xtask/data/protocol.json`)를 읽어 새·바뀐·사라진 패킷, 필드, union, 상수를 확인하고, 손 코덱과 이를 쓰는 엔진 코드를 맞춘다.
5. 필드 상한: Mojang bedrock-protocol-docs의 같은 버전 릴리스 커밋으로 `xtask/src/dump.rs`의 고정 값을 바꾸고 `cargo xtask dump`를 돈다. dump나 gen이 상한으로 실패하면 `xtask/src/dump/limits.rs` 표를 고친다.
6. 시험: P1~P5를 포함한 `cargo xtask verify`가 PASS해야 한다.
7. 데이터: `.claude/docs/data.md` 출처 표의 덤프 원본과 기준 목록 출처를 같은 버전으로 바꾸고 상태 대조와 업그레이드 스키마를 맞춘다.
8. 공식 서버 대조: 새 버전 공식 서버의 복사본(`.cache/bds/<버전>/`)으로 P6 전체를 돌리고 다른 점은 공식 서버를 따른다(`.claude/docs/testing.md`).
9. 표시: 버전을 올리는 것은 공개 API의 깨지는 변경으로 다룬다(semver). 지원 프로토콜 번호와 고정 버전 표(`.claude/docs/protocol.md`)를 바꾼다.

배경과 자세한 사실은 `.claude/docs/protocol.md`.
