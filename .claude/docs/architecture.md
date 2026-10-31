# 아키텍처

작업 규칙은 `.claude/rules/architecture.md`.

이 문서는 코드가 어디에 있고, 어떤 스레드가 무엇을 하며, 틱이 어떻게 돌고, 사용자가 어디로 확장하는지를 적는다.

```rust
use anchor::{command::Command, event::Priority};

fn main() -> anchor::Result<()> {
    anchor::Server::builder()
        .config(anchor::Config::default().load("anchor.toml")?)
        .on(Priority::Normal, on_player_join)
        .command(Command::new("spawn", "..").overload(&[], spawn))
        .build()?
        .run()
}
```

- `panic = "unwind"`는 기본값이고, `src/lib.rs`가 `compile_error!`로 강제한다.
- 관련 문서: `network.md`(전송·로그인), `protocol.md`(생성 코드), `storage.md`(저장·재생 DB), `data.md`(블록·아이템 데이터), `config.md`(설정 키), `testing.md`(검증), `dependencies.md`(외부 의존성).

## 저장소 구조

```
CLAUDE.md              규칙
Cargo.toml, Cargo.lock anchor 패키지 + 작업공간 + lint 정본
rust-toolchain.toml    툴체인 고정
.rustfmt.toml, .clippy.toml, .cargo/config.toml, .gitattributes, .gitignore
src/                   라이브러리
tests/                 공개 API 시험(<주제>.rs)
examples/              lobby, pvp, custom, minigame (봇 시나리오로 시험)
xtask/                 개발 도구(verify, gen, dump, table, p6). go/: 추출기·봇·대조 도구, data/: IR·덤프·기준 목록·P6 표
vendor/                결함을 고친 의존성 사본(dependencies.md)
.claude/docs/                  이 문서들
```

- 크레이트는 `anchor`와 `xtask` 둘이다.
- 서버 프로그램은 코드 없이 실행 인자 명령 `update-config`(`config.md`), `import <월드 폴더> [이름]`, `export <월드 폴더> [이름]`(`storage.md`)을 받는다(`args` 모듈). `Config::load`와 `Server::run`이 인자를 보고 서버를 켜지 않은 채 일만 하고 끝낸다. 그 밖의 인자는 프로그램의 것이다.
- 도구 설정을 점 이름으로 두면 상위 폴더와 홈의 설정이 섞이지 않는다. `rust-toolchain.toml`과 `Cargo.lock`은 점 이름을 쓸 수 없다.
- `.cache/`는 git에서 제외한다.

## 모듈 배치

| 영역 | 모듈 | 하는 일 |
|---|---|---|
| 전송 | `raknet`, `nethernet`(`signaling`, `peer`, `sdp`, `identity`) | UDP RakNet, NetherNet(WebRTC) 과 HTTP 시그널링 |
| 세션 | `session`(`connection`, `transport`, `packs`), `login`, `auth`, `crypto` | 배치 해독, 압축, 암호화, 로그인 순서, 리소스팩 전송, 틱 큐 중계 |
| 프로토콜 | `protocol`(`packet`, `types`, `consts` 생성, `io`, `batch`, `compression`) | 패킷 코덱, 배치, 압축 |
| 틱 규칙 | `tick` 아래 `survival`, `interact`, `inventory`, `update`, `entity`, `trade`, `block_entity`, `view`, `spawn`, `command`, `form`, `chat`, `weather`, `world_api`, `dispatch`, `hook`, `task`, `log`, `checkpoint`, `queue` | 게임 상태 `State`와 그것을 바꾸는 유일한 코드 |
| 월드 | `world`(`chunk`, `sub_chunk`, `paletted`, `light`, `generator`, `connect`, `tree`, `biome`) | 청크, 팔레트, 조명, 생성기, 이웃 연결 속성 |
| 저장 | `save`(저장 스레드), `storage`(redb 파일 하나), `import`(LevelDB 월드 가져오기), `export`(LevelDB 월드로 내보내기), `replay` | 스냅샷 커밋, 로드, 백업, 재생 |
| 블록·아이템 데이터 | `block`, `item`, `crafting`, `effect`, `nbt` | 데이터 덤프에서 생성한 표와 값 타입 |
| 공개 API | `player`, `entity`, `world`, `event`, `action`, `command`, `permission`, `form`, `display`, `menu`, `inventory`, `tile`, `custom`, `task`, `data`, `pack`, `config`, `math`, `replay`, `import`, `export` | 사용자가 보는 타입. `anchor::Context`, `Server`, `ServerBuilder`, `ShutdownHandle`, `Config`, `Error`는 루트에서 다시 내보낸다 |
| 기반 | `runtime`, `pool`, `clock`, `random`, `bucket`, `console`, `fs`, `error` | 스레드 생성, CPU 풀, 시계, 난수, 토큰 버킷(핑·입력 예산), 콘솔 로그, 상한 있는 파일 읽기 |

- 생성 파일의 목록은 `protocol.md`(코덱)와 `data.md`(블록·아이템 데이터)에 있다.
- `runtime`, `clock`, `pool` 밖의 스레드 생성, 시스템 시계 읽기, rayon 병렬 API 호출은 `.clippy.toml` 금지 목록이 막는다.

## 스레드와 큐

| 스레드 | 수 | 하는 일 |
|---|---|---|
| 틱 | 1 | 게임 상태를 바꾸는 유일한 스레드 |
| 저장 | 1 | 청크·플레이어 로드, 스냅샷 커밋, 백업 |
| 네트워크 워커(tokio) | `network-threads` | 전송, 세션, 코덱, 압축, 암호화, 시그널링 |
| CPU 풀(rayon, `anchor-cpu`) | `cpu-threads` | 틱의 병렬 읽기 단계, 청크 생성·직렬화, 처음 조명 계산, 청크 추가 데이터 읽기 |
| 블로킹 풀(tokio) | 최대 8 | 엔진 몫 4(인증 갱신, 팩·키 읽기), 사용자 `spawn_blocking` 몫 4 |
| 콘솔 | 1 | 표준 입력 줄을 상한 큐로 넣음(`console-input`) |
| 콘솔 로그 | 1 | Anchor 기본 콘솔 출력기가 표준 오류에 씀(`log-level`이 `off`가 아니고 `without_console_log`를 부르지 않았을 때) |

- `"auto"`의 스레드 수와, 계산 스레드 합계 상한을 넘는 설정의 거부는 `config.md`(`network-threads`, `cpu-threads`)에 있다.
- 블로킹 스레드는 I/O를 기다리므로 예산에 세지 않는다.
- 4 MiB 스택은 OS 기본값이나 `RUST_MIN_STACK`과 무관하게 NBT 512단 중첩을 버틴다.
- 주요 큐 상한:

| 큐 | 상한 | 가득 차면 |
|---|---|---|
| 세션 → 틱 입력 | 세션마다 1,024, 합계는 그 몫이 모두 들어가게 시작 때(`network.md`)(틱당 적용 8,192) | 몫을 다 쓴 그 세션만 연결을 끊는다 |
| 틱 → 세션 송신 | 1,024 | 틱이 기다리지 않고 그 세션을 버린다 |
| 틱 → 저장 요청 / 응답 | 64 / 2,048 | 응답은 틱당 정해진 수만 적용, 나머지는 다음 틱 |
| 플레이어 outbox(직렬 단계가 쌓는 패킷) | 틱당 4,096 | 그 플레이어를 끊는다(폭주한 규칙·콜백) |
| 거부된 변화가 다시 보낼 블록 위치 | 플레이어마다 틱당 4,096 | 그 플레이어를 끊는다(폭주한 클라이언트) |
| `defer` 대기 | 4,096(틱당 실행 16,384) | `Err` |
| 예약 작업 / 비동기 작업 / 결과 | 65,536 / 1,024 / 1 MiB | `Err` |

- rayon은 drop에서 스레드를 기다리지 않으므로, 정상 종료는 종료 핸들러로 CPU 풀 스레드를 센다.
- 저장 스레드는 CPU 풀에 스레드 수만큼씩 차례로 일을 넣되, 차례마다 첫 항목은 자기가 직접 돈다(`Pool::map_in_turns`). 그래서 풀 스레드는 많아야 스레드 수 - 1개만 저장 일을 잡고, 하나는 틱의 병렬 단계에 남는다. rayon 워커는 풀 밖에서 들어온 일을 훔칠 일이 없을 때만 집으므로, 모든 워커가 저장 일을 잡으면 틱의 병렬 단계가 그 차례 전체 뒤에서 기다린다. 다만 틱의 병렬 단계 중에 새 차례가 시작되면, join에서 기다리는 워커가 아직 안 잡힌 저장 항목을 집어 틱이 항목 하나만큼 기다릴 수 있다. 항목 비용이 고르지 않으면 저장 처리량이 준다. 이 개선은 틱의 대기 시간만 줄이고 틱의 일 자체는 줄이지 않으며, `cpu-threads`가 1이면(자동 설정에서 논리 CPU 4개 이하) 달라지는 것이 없다.

## 틱

- 고정 틱 속도(`Context::TICKS_PER_SECOND`, 한 틱 길이는 거기서 나온 `tick::TICK`), 엔진 시계다. 늦은 틱은 곧바로 다음 틱을 시작해 따라잡고, 한 주기 이상 밀리면 놓친 틱을 버리고 경고한다.
- Rust 1.98의 Windows `sleep`은 고해상도 대기 타이머(Windows 10 1803 이상)를 써서 실제 틱 간격 p99가 약 50.5 ms다. 기본 타이머(15.625 ms) 단위로 기다리면 46.9/62.5 ms로 흔들려 다른 플레이어 이동이 끊긴다. tokio `sleep_until`은 더 거칠고, 회전 대기는 CPU를 태운다.
- 틱 함수는 `step(state, inputs, responses, pool, output, log)` 하나다.

단계:
1. **입력(직렬)**: 저장 스레드 응답(로드된 청크·플레이어, 커밋 결과)을 도착 순서로, 그다음 세션 입력을 (세션 ID, 도착 순번)으로 정렬해 적용한다. 수신 훅이 여기서 돈다. 이어서 예약 작업.
2. **규칙**: 생존 → 상호작용(부수기·놓기·쓰기) → 블록 갱신(랜덤·예약·이웃) → 엔티티 → 거래 → 블록 엔티티 → 조명(월드당 상한).
3. **정리(직렬)**: `defer` 실행, 명령 동기화, `player::sync`(직접 API가 표시해 둔 변화 전송), 넘친 outbox 정리, 뷰(청크 선택·로드 요청·내리기, 월드 로드·언로드), 이번 틱에 바뀐 블록 엔티티 전송(위치마다 한 번, 마지막 값).
4. **출력(병렬 읽기 전용)**: 표시된 플레이어마다 패킷 목록을 CPU 풀에서 플레이어 순서로 만든다. 송신 훅이 여기서 돈다. 결과는 스레드 수와 무관하다.
5. **경계**: 전달 뒤 저장 차례면 스냅샷 하나를 저장 스레드로 보내고, 네트워크 제어(서버 이름, IP 차단)를 넘긴다.

- 틱이 플레이어를 내보내는 단계(킥, 다른 서버로 보내기, 중복 로그인, 손상 데이터, 정상 종료)는 그 플레이어의 outbox를 송신 훅에 거쳐 마지막 패킷(Disconnect, Transfer)보다 먼저 보낸다. 그래서 킥 직전이나 PlayerQuit 리스너가 보낸 것이 닿는다(PMMP도 대기 패킷을 끊기 패킷 앞에 비운다). 넘친 outbox만은 뒤가 잘렸으므로 보내지 않는다.
- PlayerAuthInput은 이동 → 블록 액션·아이템 상호작용 → 스택 요청 순서로, 나머지 패킷은 그것을 소유한 규칙 모듈로 간다.
- 거부된 클릭·부수기는 둘레 블록을, 거부된 표지판 편집은 그 블록을 그 자리에서 보내지 않고 위치만 표시한다. 출력 단계가 그 틱 끝의 블록(가짜 블록이 있으면 그것)을 위치마다 한 번 보내고, 같은 위치의 블록 변화는 따로 보내지 않는다. 그 자리에서 보내면 outbox가 블록 변화 뒤에 가므로, 같은 틱의 뒤 입력이나 규칙이 바꾼 블록을 클라이언트에서 옛 상태로 되돌린다.
- 한 틱의 청크 반경 요청은 응답과 시야 크기 조정이 마지막 값으로 하나다. 열린 시야는 뷰 단계에서 따라간다: 요청 수에는 상한이 없고, 요청마다 시야 원판 전체를 다시 만들면 클라이언트 하나가 틱을 붙잡는다.
- 청크 로드 상한(동시 1,024개, 틱당 256개)은 모든 시야가 함께 쓰고 먼저 묻는 시야가 빈 자리를 가져간다. 그래서 뷰 단계는 틱마다 한 사람씩 뒤의 플레이어부터(틱 % 플레이어 수 번째, 세션 순서로 돌아서) 시야를 돈다. 세션 순서로만 돌면 시야가 겹치지 않는 앞선 세션 64개가 상한을 채워, 저장 스레드가 느릴 때 뒤 세션은 스폰 청크를 못 받고 스폰 마감(`network.md`)에 걸린다.
- `slow-empty-worlds`가 켜지면 플레이어 없는 월드는 100틱에 1틱만 월드 규칙을 돌리고, 월드마다 다른 틱에 돌아 몰리지 않는다.

## 결정성

- **시간**: 틱이 쓰는 측정값은 로그되는 입력 `Measure`로 들어온다. TPS·MSPT, 연결 수, 프로세스 값은 전용 세션 ID에, 핑은 그 플레이어의 세션에 실린다. 접속 시각은 Join 신원에 든다. 밴 기간도 엔진 틱이다.
- **난수**: `random`은 카운터 방식이다. `Random::new(world_seed, tick, stream)`이 SplitMix64를 세 값의 혼합으로 시작하고 상태를 저장하지 않으므로 재시작, 재생, 병렬 단계에서 같은 값이 나온다. 명령 선택자 `@r`, 그림 고르기도 이 난수다.
- **ID**: 엔티티 `Id`는 색인 + 세대, 런타임 ID·스택 네트워크 ID·폼 ID는 엔진 카운터다. 커스텀 콘텐츠 번호는 등록 순서다.
- **실수**: `math`는 결정적 `min`·`max`·`clamp`(±0을 정한 쪽으로)와 다항식 `sin`·`cos`·`atan2`(fdlibm 계수)를 준다. 조명 시간 보정도 `cos` 대신 표다.
- **병렬 단계**: `Pool::map`은 결과를 입력 순서로 돌려준다.
- **반복 순서**: 입력 정렬 키가 입력마다 유일하므로 불안정 정렬도 결정적이다.
- **상태 해시**: 구조체를 `..` 없이 분해하므로 새 필드를 빼먹으면 컴파일이 실패한다.

## 입력 로그와 재생

- 로그에 드는 입력: 접속 신원, 디코드된 패킷과 크기, 떠남, 콘솔 줄, 작업 결과, 측정값, 저장 응답, 큐가 가득 차 버린 세션. 입력은 적용 전에 로그에 들어가므로 패닉한 틱의 로그에도 그 입력이 있다.
- 로그는 구간 목록이다. 프로세스 시작과 저장 스냅샷마다 새 구간이 시작되고, 구간 머리는 (시작 틱, 커밋 식별자, 그 상태의 체크포인트)다. 로그 머리에는 설정 지문과 등록 지문(명령, 커스텀 이름, 이벤트별 리스너 수, 작업 주기, SHA-256)이 있다.
- 상한은 64 MiB다. 패킷은 와이어 크기와 디코드에 든 할당 중 큰 쪽으로 센다(아이템 NBT는 바이트보다 훨씬 많이 할당한다). 그래서 로그를 다시 읽을 때도 모든 패킷이 64 MiB 할당 예산 하나를 함께 쓰고, 넘는 로그는 크기 오류로 거부한다. 커밋된 구간은 자리가 필요할 때만 오래된 것부터 버리고 가장 새 커밋 구간은 남긴다. 커밋 뒤 입력이 상한의 1/4에 닿으면 자동 저장이 꺼져 있어도 스냅샷을 요청해, 나머지 3/4가 커밋 시간을 덮는다. 상한을 넘으면 그 구간은 "잘림"으로 표시되고 재생은 거기서 멈춘다.
- 커밋 식별자: 틱이 적용한 모든 입력을 이어서 해시해 커밋과 함께 파일에 쓴다. 재생은 같은 틱에 다른 실행이 커밋한 파일을 잘못 고르지 않는다.
- 틱 스레드가 패닉하거나 저장이 실패하면 로그가 `data/crash/input-<unix 초>-<마지막 틱>.log`(같은 이름이 있으면 덮지 않음, 최대 1 GiB)로 남는다.
- 재생: `Server::replay(log) -> replay::Result<Replay>`. 로그를 쓴 실행과 같은 설정·월드·등록으로 만든 서버에서 부른다(다르면 `SettingsDiffer`·`RegistrationsDiffer`). 커밋된 파일과 체크포인트에서 같은 `step`을 돌리고 이후 구간의 체크포인트와 비교한다(다르면 `Diverged`). 끝은 `End::{Completed, Truncated, Panicked}`.
  - 클로저 안 상태는 처음부터 시작하고, 사용자 코드의 외부 효과(파일, 네트워크)는 다시 일어난다.
  - 어떤 DB 파일과 백업을 고르는지는 `storage.md`.

## 패닉 정책

| 어디서 | 결과 |
|---|---|
| 연결 작업(세션, 코덱) | 그 연결만 끊는다. NetherNet 구동부는 피어 하나를 감싸는 str0m 호출을 `catch_unwind`로 막아 다른 피어를 지킨다 |
| 틱 스레드 | `catch_unwind` 없이 끝난다. drop 가드가 `thread::panicking()`일 때 입력 로그를 메인 스레드로 넘기고, `run`이 크래시 로그를 쓰고 `Error::TickPanicked`로 끝난다. DB 커밋은 없다 |
| 저장 스레드 | `Error::Storage`로 끝난다. 틱이 죽어 `Finish` 없이 큐가 닫히면 남은 스냅샷을 커밋하지 않는다 |
| 사용자 `spawn_blocking` 본문 | `task::Error::Panicked`로 끝 처리기에 간다 |
| CPU 풀 작업 | 호출한 틱으로 전파된다 |

- 커밋이 끝내 실패할 때의 재시도와 종료는 `storage.md`에 있다.

## 공개 API 구조

- 같은 모양의 접근자: `entity`/`entity_mut`, `world`/`world_mut`, `worlds()`, `players()`(Id 순).
- `Player`와 `Entity`는 틱 상태 구조체 그 자체다.
- 메시지 계열(채팅, 팁, 팝업, 타이틀, 액션바, 토스트 등)은 `Player::send(Message)` 하나로 합쳤다. 원시 패킷은 `Player::send_packet`.
- 폼, 스코어보드, 보스바, 가짜 블록, 메뉴 창은 `Context` 메서드다(처리기가 `&mut Context`를 받고 표가 `State`에 있다).

## 이벤트

- 등록: `ServerBuilder::on::<E>(Priority, |e: &mut E, ctx: &mut Context| ..)`, `ServerBuilder::monitor::<E>(|e: &E, ctx: &Context| ..)`. 우선순위는 `Lowest`~`Highest` 다섯 단계다. 보기만 하는 리스너는 타입으로 읽기 전용인 `monitor`로 따로 등록하고 적용 뒤 상태를 본다.
- 다시 검사가 있는 까닭: 리스너가 바꾼 상태를 다시 보지 않고 적용하면 복제·소실이 생긴다.
- 모든 리스너는 취소된 이벤트도 보고 `is_cancelled`로 거른다.
- **재진입 없음**: 리스너가 도는 동안 그 이벤트의 리스너 목록은 상태 밖으로 빠져 있어도 된다.
  - `ctx.defer(f)`: 지금 콜백이 끝난 뒤 같은 틱에 큐 순서로 돈다.
  - `ctx.perform(action)`: 게임 규칙을 거치는 행동(이벤트 포함)을 defer로 넣는다. 행동은 봉인된 `action::Action` 구현이다(`BreakBlock`, `UseItemOn`, `UseItem`, `AttackBlock`, `InteractEntity`, `Damage`, `SetGameMode`, `Chat`, `Kick`, `Transfer`, `Attack`, `Explode` 등).
- EntitySpawn·ItemSpawn을 엔티티 틱이 알리는 까닭: 다른 규칙 중간이나 직접 API에서 리스너가 돌지 않게 한다.
- 목록은 `event/{block,entity,inventory,player,server,world}.rs`의 103개이고 `anchor::event::*`로 평평하게 공개한다. 대응표 이벤트 중 범위 밖(플러그인, PHP·운영 내부, 레드스톤)은 빼고, 원시 패킷 이벤트(DataPacketReceive·Send·Decode)는 패킷 훅으로, PlayerPreLogin은 PlayerLogin으로 합친다. 대응표에 없는 `TradeOpen`, `Trade`를 더한다.
- PlayerDataSave 취소를 무시하는 까닭: 원자 저장을 지킨다.

## 확장 지점

| 필요 | 쓰는 것 |
|---|---|
| 게임 반응 바꾸기 | 이벤트(`on`, `monitor`), `defer`, `perform` |
| 패킷 보기·바꾸기·버리기 | `ServerBuilder::on_receive`(입력 단계 직렬, `Context` 있음, 로그는 도착한 패킷이라 재생이 훅을 다시 돌림), `on_send`(출력 단계 병렬, `SentPacket`: 여러 플레이어가 나누는 패킷은 훅이 `get_mut`으로 바꿀 때만 그 플레이어 몫을 복사). 미리 인코드한 공용 시작 데이터와 세션이 닫는 마지막 패킷, 로그인·팩 단계는 훅을 거치지 않는다. `hook-movement-packets`·`hook-player-input`으로 잦은 패킷을 뺄 수 있다 |
| StartGame 고치기 | `ServerBuilder::start_game`(엔진 필드는 훅 뒤 되돌림) |
| 로그인 정책 | PlayerLogin 이벤트(거절 사유가 플래그 목록), 권한, op·밴·화이트리스트 |
| 명령 | `ServerBuilder::command(Command)`(타입 있는 인자, AvailableCommands 자동완성), `without_command`, `permission` |
| UI | 폼, 메뉴 창(가짜 컨테이너), 스코어보드·보스바·타이틀, 가짜 블록 |
| 예약·비동기 | `ctx.schedule`, `schedule_repeating`, `ServerBuilder::every`, 취소 핸들, `ctx.spawn_blocking(일, 끝 처리기)`(결과는 serde 직렬화되어 로그에 든다) |
| 사용자 데이터 | 플레이어별 비영속 Extensions(타입 키, 64종), `ServerBuilder::player_data`·`server_data`(serde 타입, 같은 DB 커밋), 접속하지 않은 플레이어의 인벤토리와 데이터 `ctx.hold_offline_player(xuid, 이름, 끝 처리기)`(티켓 64개, `storage.md`) |
| 커스텀 콘텐츠 | `custom_item`, `custom_block`(동작 trait `custom::Behaviour`), `custom_entity`(`entity::Behaviour`), `recipe`, 크리에이티브 인벤토리. 엔진이 StartGame 속성, ItemRegistry, 엔티티 식별자, 블록 해시 ID를 채운다. 텍스처는 리소스팩 |
| 리소스팩 | `ServerBuilder::resource_pack`, 설정 `resource-packs` |
| 월드 생성 | `Generator` trait, `ServerBuilder::generator`(이름으로 등록, flat·void 기본) |
| 설정 | `anchor.toml`과 코드의 `Config`(`config.md`) |
| 주민 거래 | 타입 있는 `custom::Offer`와 거래 API(아래) |

- `block::State`·`item::Type`은 16바이트 enum이라 바닐라 경로는 분기 하나로 표를 읽는다. 해시 충돌이면 빌드를 거부한다.
- 클라이언트는 새 상태 효과·인챈트를 표시할 수 없다.
- 사용자 저장 공급자 trait은 없다.

## API 대응표

- 대상은 `data.md` 출처 표에 고정한 덤프 원본 커밋의 public, 비`@internal` API에서 범위 밖을 뺀 것이다. `xtask/php/api.php`가 PHP 리플렉션으로 `xtask/data/pmmp/api.json`(class-like와 선언된 public 메서드)을 만든다.
- `xtask/data/pmmp/status.txt`: 줄마다 `<패턴> <상태> <대상|이유>`. 상태는 `differs`(이유), `merged`(대상 alias), `skipped`(이유), `open`(아직 없고 거부하지도 않음). 패턴은 FQN, `*`(`\`·`:`를 넘지 않음), `**`(모두)다. 위에서 처음 맞는 줄이 이기고, alias가 있으면 구현함이 먼저다.
- `cargo xtask table [--check]`가 `xtask/data/pmmp/table.md`(행마다 FQN, 상태, 위치 `파일:줄`, 시험, 이유)를 쓴다. verify가 최신 여부와 빈칸 0을 검사한다. 상태별 행 수는 `table.md` 머리에 있다.
- 방송 채널은 `broadcast`·`send`로 합쳤다.
- 참고한 구현에서 고쳐서 가져온 결함(대표): 전역 싱글턴, 가변 공유 `Vector3`, 복제하는 게터와 NBT 재직렬화, 문자열 키, 이벤트마다 객체 할당, AsyncTask 직렬화, 원시 패킷 이벤트, 숨은 O(n) 조회, 재진입 디스패치, 주석뿐인 MONITOR, 리스너가 바꾼 상태를 다시 검사하지 않음, 게임 로직의 벽시계와 전역 난수, 비원자 저장, 로드 순서로 바뀌는 ID, 조명 갱신(상한·경계 전파 없음), 예약 갱신을 저장하지 않음, 기본 월드 없이 남는 언로드, 덮어써 잃는 화로 결과, 모양을 검사하지 않는 shaped 레시피.

## 게임 범위

범위 = 데이터 덤프와 대응표의 동작 ∪ 다른 구현을 참고한 야생 시스템(모루, 대장장이 작업대, 베틀, 신호기 결제, 구리 산화, 화로 경험치) − 범위 밖 기능(`CLAUDE.md`).

구현한다:
- 생존: 체력, 배고픔, 공기·익사, 경험치, 상태 효과, 피해·전투·넉백, 죽음·리스폰, 게임 모드, 이동 검사, 서버 권위 부수기 시간.
- 블록: 액체와 양동이, 낙하 블록, 폭발, 불, 농사(작물, 줄기, 사탕수수, 선인장, 코코아, 네더 사마귀, 대나무, 덩굴, 열매, 경작지, 묘목에서 나무), 랜덤 틱 계열(잎 부식, 불·잔디 번짐, 얼음·눈 녹음, 구리 산화), 이웃 연결 속성.
- 작업대: 제작(smithing 포함), 화로 계열과 모닥불, 양조기, 인챈트대, 모루, 대장장이 작업대, 석재 절단기, 베틀, 신호기, 뼛가루.
- 엔티티: 떨어진 아이템, 경험치 구슬, 낙하 블록, TNT, 발사체, 물약 구름, 그림, 엔드 크리스털, 폭죽, 삼지창, AI 없는 NPC(주민형, 사람형, 좀비), 커스텀 엔티티.
- 침대와 수면, 서버 조명(클라이언트로 보내지 않음, 설정으로 끔), 주민 거래 조작.
- 날씨는 월드별 상태(맑음, 비·눈, 천둥)와 번개를 API(`Context::set_weather`, `strike_lightning`)로만 바꾼다. 순환이 필요하면 반복 예약 작업과 `server_data`로 만든다.
- 기본 명령 `status`는 가동 시간, TPS·MSPT, 메모리, 스레드, 접속자, 월드별 청크·엔티티 수를 Anchor 고유 형식으로 보여 준다.

- 범위 밖인 바닐라 주민 경제는 기본 거래 템플릿을 말한다.
- 덤프에 없는 블록의 예: 퇴비통, 다시마, 숫돌. 가져온 월드의 그런 블록은 비활성 블록으로 원래 모습만 보인다. 동작, 아이템, 놓기, 생성은 없다(`data.md`).

## 주민 거래 조작

대응표에 없는 기능이다. AI 없는 주민형 NPC의 거래 목록을 서버가 정한다.

- 소환: `Context::spawn_villager(world, pos, profession, &[Offer]) -> Result`. 생성 알·명령·API로 소환한 주민은 빈 목록이다.
- 목록 API: `trade_offers`, `set_trade_offers`, `add_trade_offer`, `replace_trade_offer`, `remove_trade_offer`, `trade_tier`, `set_trade_tier`, `set_trade_experience`, `set_trade_tier_experience`, `reset_trade_uses`. 목록을 미리 검사하는 `custom::check_offers`는 소환과 같은 규칙이다(오퍼 수 상한 `custom::MAX_OFFERS`). 오퍼는 NBT 대신 타입 있는 `custom::Offer`(`new(buy, sell, max_uses)`, `with_second_payment`, `with_any_damage`, `with_tier`, `with_price_multipliers`, `with_demand` 등)다.
- 이벤트: `TradeOpen`(창 열기), `Trade`(거래 1회).
- 상한: 저장 형태 합계 2 MiB.
- 프로토콜 값(공식 서버 대조): 창 종류 15, 오퍼 NBT 키는 공식 서버 키(`Recipes`, `netId` 등), Trade2 입력 칸 4·5, 출력 50(ItemStackRequest에서는 지불 컨테이너 47·48, 결과는 created output 60:50). `netId`는 CraftingData 다음부터 서버 카운터로 처음 열 때 매기고 다시 열어도 유지한다. 거래 뒤에는 등급이 올라도 UpdateTrade를 다시 보내지 않고(공식 서버와 같다), API로 목록을 바꿀 때마다 다시 보낸다. 보상 구슬은 거래마다 3~6, Damage 32767은 아무 데미지다.
- 거래 데이터는 엔티티와 같은 DB 커밋에 든다.

## 여러 월드

차원은 오버월드 하나(y −64~319, 서브청크 24개)다.

- 월드는 영구(파일 `data/worlds/<이름>.redb`)와 임시(저장 안 함) 두 종류다. 빌드 때 `ServerBuilder::world`·`world_settings`·설정 `[worlds]`로 선언한다.
- 실행 중 수명주기(`Context`): `create_world(name, &Options)`, `load_world(name, &Options)`(시작 때 파일이 있던 영구 월드), `unload_world`(WorldUnload 리스너가 취소 가능), `force_unload_world`, `delete_world`(언로드된 영구 월드와 그 파일만, 백업은 남김), `set_default_world`, `is_world_generated`, `is_world_loaded`. 이벤트 `WorldInit`, `WorldLoad`, `WorldUnload`는 뷰 단계에서 난다.
- 언로드: 그 월드의 플레이어는 기본 월드 안전 스폰으로 가고, 청크는 보통 경로로 내려가며, 다음 커밋이 월드와 플레이어를 한 스냅샷으로 함께 쓴다. 언로드된 월드도 `world::Id`와 색인을 유지해 같은 자리로 다시 로드된다. 월드는 실행당 최대 1,024개다.
- 월드 파일은 청크, 시간, 스폰만 갖는다. 생성기·시드·난이도·시야 값은 다시 켤 때 설정이나 `world::Options`에서 온다. 설정과 `Options`의 스폰은 새 월드에만 쓴다. 시드가 없으면 월드 이름에서 만든다(SHA-256 앞 8바이트).
- 월드별 값: 난이도(`set_world_difficulty`), 시간 정지(`stop_time`·`start_time`, 저장 안 함), 청크 붙들기(`hold_chunks`, 상한 `world::MAX_TICKETS`·`MAX_TICKET_CHUNKS`·`MAX_TICKET_RADIUS`), 시야·전송 값(`[worlds.<이름>]`), 날씨.

## 알려진 한계와 위험

- 크래시 때 외부 시스템에 쓴 사용자 효과는 돌아가지 않는다. 재생은 그 효과를 다시 일으킨다.
- 클로저나 Extensions가 살아 있는 구간은 재생할 수 없다. 예약 작업은 재시작하면 사라진다.
- 실행 중 바꾼 설정 값은 저장하지 않아 재시작하면 돌아간다(`storage.md`).
- NetherNet 구동 작업은 하나다. 측정으로 약 500명까지 충분하고, 넘으면 피어를 여러 작업에 나눠야 한다.
- 같은 차원의 다른 월드로 텔레포트할 때 ChangeDimension을 보내지 않는다. 공식 클라이언트에 옛 청크가 남는지는 실제 접속으로만 확인할 수 있다(`testing.md`).
- 블록 상태 16바이트 enum은 1명 p99에 조금 손해다. 4바이트 + 서버 표는 커스텀 상태에 전역 레지스트리나 API 변경이 필요하다.
- 결정성 시험은 디버그에서 길다(1~2분대).
- 블록 물리 임시 규칙(CloudburstMC 물리 갱신 전 임시, 빌린 값): 공식 서버 물리 덤프에 없는 1.26.60 상태가 있다(`data.md`). 연결 속성을 가진 불·레드스톤 가루·후렴 식물 상태 1,872개는 그 속성을 뺀 옛 상태의 물리를 써서, 연결에 따라 물리가 다르면 틀린다. 새 블록 `icicle`, `ice_crystal`, `disc_press` 상태 30개는 비슷한 블록에서 빌린 굳기·빛·충돌 상자라 실제 값과 다를 수 있다.
- 1.26.60 연결 속성(불, 레드스톤 가루, 후렴 식물)은 서버가 계산하지 않고 연결 없음으로 보낸다. 공식 서버가 보내는 값과 다를 수 있다.
