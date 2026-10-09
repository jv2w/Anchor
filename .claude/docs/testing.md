# 시험과 검증

작업 규칙은 `.claude/rules/testing.md`.

Anchor의 검사는 세 층이다. `cargo xtask check`는 바뀐 것만 보는 가벼운 검사, `cargo xtask verify`는 전체 검사, P6는 기준 서버(Mojang이 배포하는 공식 서버)와 패킷을 대조하는 별도 도구다. 클라이언트 화면처럼 셋 다 볼 수 없는 것은 실제 클라이언트로 확인한다.

## `cargo xtask check`

HEAD와 다른 파일(바뀐 것, 지운 것, 새 파일)만 보고 순서대로 돌며 처음 실패에서 멈춘다. 통과하면 마지막 줄이 `PASS`다.

1. `cargo fmt --check`
2. `cargo clippy --workspace --all-targets -- -D warnings`(빌드 포함)
3. 바뀐 것의 시험:
   - `src/`의 파일: 그 모듈의 단위 시험(`cargo test -p anchor --lib -- <모듈>::`). 하위 모듈이 있는 파일은 자기 모듈, 없는 파일은 그 폴더를 가진 모듈이다(`foo/bar.rs`, `foo/tests.rs` → `foo::`). `src/lib.rs`나 빌드 파일(`Cargo.toml`, `Cargo.lock`, `.clippy.toml`, `.rustfmt.toml`, `rust-toolchain.toml`, `.cargo/`)이 바뀌면 모든 단위 시험.
   - `tests/<이름>.rs`, `xtask/tests/<이름>.rs`: 그 시험 파일. 하위 폴더의 공용 파일이 바뀌면 그 패키지의 모든 통합 시험.
   - `xtask/src/`: xtask 단위 시험.

- 코드가 아닌 파일만 바뀌었으면 바로 통과한다. `examples/`는 clippy가 빌드하고, vendor 사본의 시험은 verify가 돈다.
- 빌드는 `target/check/`에서 한다. 도는 verify를 기다리지 않고 실행 중인 `xtask.exe`를 다시 빌드하지 않게 하려는 것이다.
- 통과하면 HEAD와 바뀐 파일 내용의 도장을 `target/check/pass`에 남긴다. 그대로인 트리는 바로 통과한다.
## `cargo xtask verify`

검사는 순서대로 돌고, 각 줄은 `ok  ` 또는 `FAIL`로 시작한다. 마지막 줄은 전체 결과 `PASS` 또는 `FAIL`이다. 하나라도 실패하거나 경고를 내면 전체가 `FAIL`이다. 이 PC에서 10~15분 걸린다.

| 순서 | 검사 | 보는 것 |
|---|---|---|
| 1 | `cargo xtask gen --check` | 생성된 프로토콜·데이터 코드가 생성기 출력과 같다 |
| 2 | `cargo xtask table --check` | API 대응표(`xtask/data/pmmp/table.md`)가 최신이고 빈 행이 없다 |
| 3 | lint reason tags | 모든 `#[expect]` 이유가 `CLAUDE.md` 6절의 태그로 시작한다 |
| 4 | `cargo fmt --check` | 포맷 |
| 5 | `cargo clippy --workspace --all-targets -- -D warnings` | lint(`[workspace.lints]`, `.clippy.toml` 금지 목록) |
| 6 | `cargo test --workspace` | 단위·통합 시험, 프로토콜 대조, 봇 시험 |
| 7 | vendor 사본 시험 | `vendor/sctp-proto`, `vendor/redb`, `vendor/str0m`, `vendor/dimpl` 자체 시험과 고친 결함의 재현 시험 |
| 8 | `cargo doc --workspace --no-deps` | 공개 문서와 링크(`RUSTDOCFLAGS=-D warnings`) |
| 9 | `cargo doc ... --document-private-items` | 비공개 문서의 링크(따로 `target/verify-private-docs`) |

- 각 cargo 검사는 `target/verify/`에서 빌드한다. 실행 중인 `xtask.exe`를 다시 빌드하지 않게 하려는 것이다.
- 필요한 도구: Go(`PATH`에 있어야 함; 봇, 오라클, Go 시험), dimpl 시험의 OpenSSL 빌드용 Perl(Windows는 `.cache/tools/`의 Strawberry Perl, Linux는 시스템 `perl`과 `make`). Linux의 dimpl 시험은 개발 의존성 wolfSSL도 소스로 빌드해서 `autoconf`, `automake`, `libtool`이 필요하다. 없으면 해당 검사가 실패한다.
- `dump --check`와 P6는 verify에 들지 않는다. `.cache/` 사본이나 공식 서버가 필요하기 때문이다(`data.md`).

## 시험 배치

- 공개 API 시험(`tests/`):
  - `block.rs`, `item.rs`: 상태·아이템 조회, 기준 목록과 데이터 덤프 대조.
  - `config.rs`: 설정 파일 기본값, 형, 범위, 모르는 키.
  - `server.rs`, `world.rs`: 빌드, 스레드 예산, 종료, 월드 API.
  - `shutdown.rs`: 콘솔 `stop`으로 정상 종료하면 마지막 틱이 커밋된다.
  - `crash.rs`: 강제 종료를 반복해도 데이터 파일이 손상되지 않고 블록이 정확히 하나다.
  - `replay.rs`: 크래시 로그 재생이 같은 패닉에 이른다.
  - `import.rs`: 공식 서버가 만든 월드 가져오기.
  - `log.rs`: 콘솔 로그와 끄기.
- xtask 통합 시험(`xtask/tests/`, `cargo test --workspace`로 돈다):
  - `protocol.rs`, `compression.rs`: gophertunnel 대조 P1~P4(`protocol.md`).
  - `spawn.rs`: P5. gophertunnel 봇이 같은 프로세스의 Anchor에 RakNet·NetherNet, zlib·Snappy, 오프라인·온라인(시험용 발급자)으로 스폰한다.
  - `examples.rs`: 예제 바이너리를 새 폴더에서 띄우고 봇이 `xtask/data/examples/<이름>.txt` 스크립트를 돈다.
  - `go.rs`: `xtask/go`의 Go 시험. `import_reader.rs`: goleveldb와 월드 리더 대조.
  - `downstream.rs`, `dimpl_repro.rs`, `redb_repro.rs`: vendor 사본이 사용자 프로젝트에 들어가는지와 고친 결함의 재현(`dependencies.md`).
- 측정용 시험 실행 예: `cargo test --release --lib tick::tests::bench -- --ignored --nocapture`(틱 비용).

## 멱등, 결정성, 보존 시험

- 멱등 시험 예: `the_same_request_twice_leaves_the_state_of_once`(인벤토리), `a_repeated_break_changes_nothing_twice`, `the_same_answer_twice_changes_nothing_twice`(폼), `a_duplicate_join_changes_nothing`.
- 결정성: 스크립트 클라이언트(`src/tick/tests/client.rs`)가 고정 시드로 게임 입력을 보내고, CPU 풀 1개와 max(논리 CPU, 4)개에서 틱별 상태 해시와 출력이 같아야 한다. 입력 로그를 재생해도 같다(`one_and_many_pool_threads_give_the_same_ticks_and_the_log_replays_them`, 리스너가 있는 `busy.rs`). 상태 해시가 구조체를 `..` 없이 분해하므로 새 필드를 해시에 넣지 않으면 컴파일이 실패한다.
- 보존: 고정 시드 무작위 인벤토리 요청에서 아이템 총량이 설명되는 만큼만 바뀐다. 강제 종료 시험(`tests/crash.rs`, `src/tick/tests/crash.rs`)은 틱 수로 정한 순간(벽시계 아님)에 서버를 죽여 복제·손실이 없는지 본다.
- 누수: 접속·종료, 창, 엔티티, 예약 갱신을 반복한 뒤 모든 표와 큐가 시작 값이다(`src/tick/tests/leak.rs`).
- 자동 시나리오: 패킷만으로 원목에서 거래까지 가는 생존 흐름(`src/tick/tests/scenario.rs`).

## P6: 공식 서버 대조

P6는 같은 봇 시나리오를 공식 서버와 Anchor에 돌려 받은 패킷을 비교한다.

- 봇: `xtask/go/bot`(gophertunnel 기반 Go 클라이언트). 공식 클라이언트처럼 서버 권위 인벤토리, PlayerAuthInput, 블록 액션을 쓰고 받은 패킷을 모두 JSON 줄로 기록한다.
- 공식 서버: `data.md` 출처 표의 판이고 사본은 `.cache/bds/<판>/`이다. 하네스가 사본의 `server.properties`를 쓰고 `worlds`를 지운 뒤 띄우고 `stop`으로 끝낸다.
- 시험 설정: 두 서버를 Xbox 로그인 없이(`online-mode=false`, Anchor `xbox-auth = false`), 평지 월드 `p6`, 시드 1로 맞춘다. 나머지(게임 모드, 난이도, 최대 인원, 시야, 압축 문턱)는 Anchor 기본값이라 설정에 따른 필드도 같게 비교된다.
- 캡처: 모두 `target/p6/` 아래다. `bds-<압축>/`, `bds-nethernet-<압축>/`, `bds-game/`, `bds-api/`와 Anchor 쪽 `anchor-*`. 비교 결과는 `anchor-api/compare.txt` 등에도 쓴다.

### Linux에서 돌리기

- 공식 서버는 Linux판(`bedrock-server-<판>-linux.zip`)을 Linux 체크아웃의 `.cache/bds/<판>/`에 푼다. 폴더 이름은 Windows와 같고, OS마다 자기 `.cache`를 둔다. `unzip`으로 풀면 `bedrock_server`의 실행 권한이 남는다.
- 하네스는 OS에 맞는 실행 파일(Windows `bedrock_server.exe`, Linux `bedrock_server`)을 찾고, 압축 안의 `bedrock_server_how_to.html`처럼 `LD_LIBRARY_PATH`를 서버 폴더로 두고 띄운다.
- 필요한 것: Go(`PATH`), `unzip`. 공식 서버 안내는 Ubuntu 18 이상만 지원한다고 적는다.
- Windows PC에서는 WSL2 Ubuntu의 Linux 파일 시스템(ext4)에 작업 트리를 복사해(`target/`, `.cache/` 제외) 빌드하고 시험한다. `/mnt/c`에서는 돌리지 않는다: 파일 시스템 동작과 fsync가 진짜 Linux와 달라 검증이 되지 않는다. 고치는 곳은 Windows 저장소 하나이고 복사본은 시험에만 쓴다.
- aarch64(`aarch64-unknown-linux-gnu`)는 교차 빌드만 확인한다(`gcc-aarch64-linux-gnu`, `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc`).

### 명령

| 명령 | 하는 일 |
|---|---|
| `cargo xtask p6 bds [zlib\|snappy]...` | 압축마다 공식 서버를 RakNet·NetherNet으로 띄워 spawn·big-skin·blocks 시나리오, 폴백, 이름 중복 로그인, 제안 identity 불일치를 기록한다. 끝에 게임 세션과 API 세션을 한 번씩 돈다 |
| `cargo xtask p6 anchor [zlib\|snappy]...` | 같은 시나리오를 이 프로세스의 Anchor에 돌리고 각 캡처를 공식 서버 캡처와 비교한다. 게임과 API 세션도 비교한다 |
| `cargo xtask p6 game bds\|anchor [시나리오]...` | 게임 세션만 |
| `cargo xtask p6 api bds\|anchor [시나리오]...` | API 세션만(공식 서버는 `allow-cheats=true`, 폼 스크립트 팩) |
| `cargo xtask p6 import` | 공식 서버가 만든 월드를 Anchor로 가져와 두 서버가 보내는 블록을 하나하나 비교한다 |
| `cargo xtask p6 run --addr HOST:PORT --out DIR [--transport T] [--token-key PEM --kid KID --iss ISS]` | 이미 떠 있는 서버에 시나리오를 돈다. 토큰 옵션은 온라인 시나리오를 더한다 |
| `cargo xtask p6 compare A B`, `p6 game compare A B`, `p6 api compare A B` | 캡처 둘을 비교한다 |

`p6 anchor`에 한 압축만 주면 NetworkSettings를 다른 압축의 공식 서버 캡처와 비교하는 문제가 있다.

### 시나리오

- 스폰(`p6 bds`, `p6 anchor`):
  - `spawn`: 로그인부터 PlayStatus(PlayerSpawn)까지의 스폰 창. 패킷 순서와 종류, `fields.txt`의 고정 필드, 아이템 패킷 전체, 청크와 서브청크 응답.
  - `big-skin`: 큰 Login 요청. NetherNet에서는 65,535바이트 조각으로도 보낸다.
  - `blocks`: 콘솔로 울타리, 판유리, 쇠창살, 벽, 문, 계단, 철사덫을 붙여 놓고 지우며 단계마다 블록 갱신 상태를 비교한다.
  - `auto` 폴백, `bind-mismatch`, 이름 중복 로그인. NetherNet 실행은 응답 SDP 구조와 조각 정보도 비교한다. 바이옴 정의와 지그소 데이터는 `bot cloudburst`가 기준 목록 출처의 데이터와 대조한다.
- 게임(`p6 game`): `craft`, `furnace`, `enchant`, `crops`, `trade`. 봇의 단계마다 명령, 요청 상태, ItemStackResponse(스택 ID는 처음 나온 순서), 단계 끝 아이템 상태, 위치별 마지막 블록 상태, 창 열기·닫기, 블록 엔티티 키, LevelEvent 종류, 엔티티 종류, 거래 창 모양과 오퍼 키를 비교한다. 소리, 채팅, 경험치 구슬, 오퍼·인챈트 내용 같은 무작위 값은 보이기만 한다. `setup` 단계는 명령만, `random` 단계는 이름·개수를 지운 모양만 비교한다. 시나리오 구역에서 4칸 밖의 변화는 보이기만 한다.
- API(`p6 api`):
  - `commands`(플레이어 명령과 출력, op), `particles`, `kick`, `forms`, `script`(봇 `-script` 동작 자체 시험, `script.txt`).
  - `weather`, `joinrain`, `joinstorm`(날씨 변화, 비·천둥 중 접속), `boltpeaceful`, `bolteasy`, `bolthard`(난이도별 번개).
  - `enchants`(인챈트 선택지 `Slot`), `stations`(모루, 대장장이, 신호기 요청 모양), `potions`(물약 색과 구름), `entities`(그림, 폭죽, 크리스털, 삼지창), `syntax`(문법 오류와 번역 이름), `tradeup`(거래 등급 상승), `others`와 동반 봇 `mover`(다른 플레이어의 접속, 이동, 공격, 죽음, 리스폰), `itemuse`(활, 삼지창, 물약 사용).
  - `villagers`(직업 번호), `numbers`(공식 서버 전용 번호와 원격 측정), `rules`, `rulesjoin`(게임 규칙), `line<바이트>`, `linekorean<바이트>`(명령 줄 길이; 공식 서버는 508바이트부터 연결을 끊음), `limits`, `limitchat`(문자열 상한).
  - 단계마다 `fields.txt`에 있는 패킷의 개수와 고정 필드, 봇의 출력·확인·폼 줄을 비교한다. 명령 목록은 둘 다 가진 명령만 메모로 비교한다. 사실 기록만 하는 시나리오는 하네스의 `FACTS_ONLY`, 고정 필드 없이 확인 줄만 비교하는 시나리오는 `CHECKS_ONLY`(`xtask/src/p6/api.rs`)에 있다.
- 가져오기(`p6 import`): 공식 서버 지형 월드의 모든 서브청크, 모든 층의 네트워크 ID가 같아야 한다. Anchor가 모르는 블록으로 두는 덤프에 없는 블록은 `intended.txt`의 `import | block` 줄로 넘긴다.

### 데이터 파일 (`xtask/data/p6/`)

- `facts.md`: 공식 서버가 보낸 값과 Anchor가 각 값에 대해 하는 일. 코드 주석이 이 파일을 가리킨다.
- `fields.txt`: 두 서버가 같아야 하는 고정 필드(`<패킷> <필드 경로>`).
- `deferred.txt`: 공식 서버가 스폰 창에서 보내지만 Anchor는 보내지 않는 패킷(`<패킷> - <이유>`). 비교에서 양쪽 모두 뺀다.
- `intended.txt`: Anchor가 일부러 다른 차이(`<시나리오> | <차이 원문> | <이유>`). 차이 문장이 정확히 같을 때만 통과하고, 더 나오지 않는 줄은 메모로 알린다.
- `script.txt`: `script` 시나리오의 동작. `forms-pack/`: 공식 서버용 폼 스크립트 팩.

## 실제 클라이언트로만 확인할 것

봇과 공식 서버로는 클라이언트 화면과 공식 클라이언트가 보내는 값을 볼 수 없다. 아래는 사용자가 공식 클라이언트로 접속해 확인할 목록이다.

- 접속: 공식 클라이언트가 RakNet으로 들어오는지(공식 서버는 "NetherNet 없이 접속 불가"로 기록), Join 전 대기 32개, 엄격한 ClientData JSON, 핑 응답의 게임 모드 뒤 숫자, 비신뢰 데이터 채널 내용 형식, 리소스팩 조각 요청 방식.
- Snappy: "AVX2 없는 기기에서 크래시" 보고.
- 스폰: PlayStatus(PlayerSpawn)를 보내는 시점 차이(Anchor는 `spawn-radius` 원의 청크 뒤), Publisher를 틱마다가 아니라 플레이어 블록이 바뀔 때만 보냄(같은 값 반복은 뺌), BiomeDefinitionList 순서.
- 레시피 책: 공식 클라이언트의 CraftRecipeAuto 재료 목록, `recipe-book = false`에서 책이 비는지, 교육판 아이템의 크리에이티브 표시, 레시피 ID·`deprecated` 블록 이름의 영향.
- 창과 월드: 창 열기 지연 2틱, 겹상자 짝 표시, 같은 차원 다른 월드 텔레포트 때 옛 청크와 난이도 반영, 가져온 월드의 모르는 화분 식물과 배너 무늬 표시.
- 커스텀 콘텐츠: 커스텀 블록 렌더링, 마찰, `"none"` 카테고리, 속성 상한, 커스텀 엔티티 `rid`·`hasspawnegg`.
- 화면: 보스바 색 갱신, 명령 별칭과 자동 완성, CommandRequest Version 값.
- 날씨: 비·눈·천둥 하늘, 번개 모습과 소리.
- 엔티티: 번호 입자 형식(LevelEvent 2009 형식과 `0x4000 | id` 형식 둘 다), `EntityDataFlagCount` 131, 멀리 있는 플레이어의 위치 표시줄.

자세한 값은 `xtask/data/p6/facts.md`의 "실제 클라이언트로만 확인"에 있다.

## 열린 확인

- `facts.md`의 "열린 질문" 목록(창 없는 인벤토리 요청, 높은 번호 입자, 플레이어 대상 tp 메시지 등).

## 이 PC에서 알려진 흔들림

- 루프백 TCP: 이 PC는 루프백 TCP 연결이 가끔 1초 뒤 시간 초과된다(Anchor 없이 Go 연결 300번 중 23번). NetherNet 시그널링(HTTP)을 쓰는 시험과 `p6 anchor`의 NetherNet 실행이 이 때문에 가끔 실패한다.
- 텔레포트 직후의 MovePlayer mode 2와 땅 플래그는 입력 도착 시각에 따라 두 서버 모두 흔들려 P6에서 비교하지 않는다.

## 무거운 시험 돌리기

verify 전체, P6, 결정성·강제 종료 시험은 CPU와 메모리를 많이 쓴다. 부하가 크면 편집기나 다른 프로세스가 멈출 수 있다.

- 낮은 우선순위로 띄우는 Windows 예: `cmd /c start /low /b /wait cargo xtask verify`. 자식 프로세스(rustc, 시험 바이너리)도 우선순위를 물려받는다.
- 범위를 정하는 수단: 빌드 병렬은 `CARGO_BUILD_JOBS`, 시험 병렬은 `-- --test-threads=N`, 이름 거르기는 `cargo test --lib tick::tests::crash`처럼 한다.
- 하네스는 봇마다 시간 한도(게임 시나리오 150초)가 있고 여유 시간 뒤 강제 종료한다.
- 측정 시험이 다른 빌드와 겹치면 값이 흔들린다.
