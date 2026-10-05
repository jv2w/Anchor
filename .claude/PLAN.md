# Anchor 계획서

설계 결정, 근거, 로드맵, 상태를 담는다. 규칙은 루트의 CLAUDE.md에 있다. 기준일은 2026-10-02다.
- 여기에는 사용자 결정과 조사로 확인한 사실만 고정한다. 세부 설계는 그 마일스톤을 시작할 때 정해 이 파일에 더한다(CLAUDE.md 8절).
- "검증 필요(Mx)"는 확정하지 못한 사실이다. 그 마일스톤에서 먼저 확인한다.

## 1. 저장소 구조
```
CLAUDE.md             규칙 (루트, 사용자 결정)
Cargo.toml            anchor 패키지 + 작업공간 + lint 정본 + 프로필
Cargo.lock
rust-toolchain.toml   툴체인 고정
.rustfmt.toml, .clippy.toml, .gitattributes, .gitignore, .cargo/config.toml
.claude/PLAN.md       이 파일
src/                  라이브러리
tests/, examples/     필요할 때 (examples는 M8)
xtask/                개발 도구(verify, gen, p6). go/: Go 추출기, 시험 도구, 봇. data/: IR, P6 표, 덤프, 기준 목록, 출처 커밋
vendor/               결함을 고친 의존성 사본과 그 사이의 바꾸지 않은 사본(사용자 결정, M3·M9). 9.1
```
- vendor 규칙(사용자 결정): 결함 부분만 고친 사본을 anchor가 path 의존성(`version = "=x"` 고정과 함께)으로 직접 쓴다. `[patch.crates-io]`는 가장 바깥 workspace에만 적용되어, Anchor를 path·git 의존성으로 쓰는 사용자 프로젝트가 결함 원본을 받았기 때문이다(사용자 결정, M9에서 `[patch]`를 버림). 고친 크레이트를 타입 수준에서 쓰는 중간 크레이트(str0m-proto, str0m-aws-lc-rs, is)는 바꾸지 않은 사본으로 두고 Cargo.toml에 `path`만 더한다. `xtask/tests/downstream.rs`가 저장소 밖 임시 프로젝트의 `cargo metadata`로 일곱 크레이트가 모두 사본으로 풀리는지 본다. 결함마다 재현 시험을 두어 고치기 전 실패, 고친 뒤 통과를 확인한다. 상류가 고친 릴리스를 내면 사본을 지우고 원래 크레이트로 돌아간다. 상류에 이슈나 PR은 올리지 않는다. 사본은 작업공간에서 제외해(`exclude`) 우리 lint를 걸지 않고, 자체 시험이 있는 사본은 verify가 따로 돌린다.
- 크레이트는 `anchor`와 `xtask` 둘이다(개발 도구 의존성이 라이브러리에 들어가지 않게). 모듈과 폴더는 필요할 때 만든다. 내려받은 외부 소스는 `.cache/`(M1에서 .gitignore에 더함).
- 도구 설정을 점 이름으로 두면 상위 폴더와 홈의 설정이 섞이지 않는다(조사 때 실측). rust-toolchain.toml과 Cargo.lock은 점 이름을 쓸 수 없다.
- 서버 실행 폴더는 `anchor.toml`(8.5절)과 `data/`(`world.redb`, `identity.key`, `auth.json`, `backups/`, `crash/`)뿐이다. ops, 화이트리스트, 밴은 `world.redb`의 표다. 콘솔 로그는 Anchor가 자기 스레드에만 기본 출력기를 걸고(설정 `log-level`), 사용자 바이너리는 `ServerBuilder::without_console_log`로 끄고 자기 구독자를 쓸 수 있다(CLAUDE.md 6절).

## 2. 네트워크
| 포트(기본, 설정 가능) | 용도 |
|---|---|
| UDP 19132 | RakNet(폴백), 서버 목록 핑 |
| TCP 19132 | NetherNet HTTP 시그널링 |
| UDP 19133 | NetherNet 게임 데이터(모든 피어가 소켓 하나) |
- IPv4와 IPv6를 같은 포트로 받는다. Windows UDP의 WSAECONNRESET은 무시하고 계속 읽는다(tokio-rs/tokio#2017).
- 시그널링(M3 확인): 주소 접속은 `GET /v1/join`을 `https://호스트:포트`, `http://호스트:포트` 순으로 보내 처음 답한 곳을 쓰고, 모두 실패하면 RakNet으로 간다(go-nethernet `endpoint/handler.go` 주석. 공식 클라이언트는 관찰할 수 없다). 서버 목록 정보는 이 GET의 JSON이다. UDP만으로 직접 접속하는 길은 LAN 발견(UDP 7551, go-nethernet `discovery`)뿐이라 TCP를 유지한다(LAN 발견은 구현하지 않는다). BDS는 시그널링과 게임 UDP에 같은 포트 번호를 쓰고 연결마다 UDP 소켓을 따로 연다. Anchor는 사용자 결정대로 UDP `nethernet-port` 하나를 모든 피어가 나눠 쓴다.

### 2.1 NetherNet
정답은 고정한 gophertunnel의 go.mod가 고정한 df-mc/go-nethernet이다(v1.62.0 기준 `endpoint/handler.go`). Mojang NetherNet 온보딩 가이드(https://mojang.github.io/bedrock-protocol-docs/guides/nether-net-onboarding-guide/)는 보조 근거다.
- 시그널링: 연결 시도마다 HTTP 요청 한 번(trickle ICE 없음). `GET /v1/join`은 2xx와 JSON `{name, protocol, version, level, players, maxPlayers, gameType}`, `POST /v1/join/{networkId}`는 SDP 오퍼를 받아 SDP 응답을 돌려준다. 최소 HTTP만 구현하고, 본문은 `Content-Length`와 `chunked`를 받는다(둘이 함께 오면 거부). 헤더, 본문, 시간, IP별 연결에 상한을 둔다.
- 오퍼의 `a=identity`: GameServerToken(RS256, JWKS)과 오퍼 지문에 대한 분리 JWS(ES384, `cpk`). 그 밖의 `alg`는 거부한다. 없으면 온라인 모드는 거부, 오프라인 모드는 받는다(BDS는 오프라인에서도 코드 37로 거부한다. 사용자 정의 클라이언트를 위해 받는 쪽을 유지한다).
- 응답의 `a=identity`: 모든 응답에 반드시 넣는다(없으면 클라이언트가 거부). 서버 신원 JWT(자체 서명, 장기 P-384 키)와 응답 지문에 대한 분리 JWS다. 위치와 모양은 BDS 1.26.52 응답을 따른다(M3 P6, 10.3).
- 신원 키: 처음 실행할 때 만들고 원자적으로 저장하며 재시작해도 유지한다(바뀌면 모든 플레이어에게 확인 창이 다시 뜬다). 깨졌으면 새로 만들지 않고 시작을 거부한다. JWT는 BDS처럼 응답마다 새로 발급한다(유효 60초).
- 신뢰: 기본은 평문 HTTP(처음 보는 키에 확인 창, TOFU). 인증서를 설정하면 HTTPS.
- WebRTC: 클라이언트는 host 후보만 쓴다. 서버는 설정한 공인 주소를 광고한다. 데이터 채널은 `ReliableDataChannel`과 `UnreliableDataChannel` 둘이고, 메시지 앞 1바이트는 남은 조각 수다. 재조립에 상한을 둔다. 다른 채널이 열리면 끊는다. 비신뢰 채널은 상한 안에서 읽고 버리며 기록한다(내용 형식은 공식 클라이언트만 알 수 있어 어느 경우든 받는다).
- 신원 결합: Login의 클라이언트 키가 오퍼에서 증명된 `cpk`와 같아야 한다. NetherNet은 Minecraft 암호화와 배치 헤더 0xfe가 없고, 압축 바이트는 그대로 있다.

### 2.2 RakNet
- 서버 쪽만, 기본은 직접 구현이다(조건을 만족하는 크레이트가 있으면 M2에서 비교). 범위는 핑, 연결 수립, 신뢰성·순서·조각, ACK/NAK, 혼잡 제어다. 단조 시계를 쓰고 ACK 범위, 조각, 재조립, 대기 데이터에 상한을 둔다.
- RakNet 프로토콜 11, MTU 400~1492(BDS와 go-raknet 확인, 10.2 "P6 결과"). 26.60은 RakNet을 "deprecated"로만 표시했다(제거 아님). BDS 1.26.52는 RakNet 전송에서 "플레이어는 NetherNet 없이 접속할 수 없다"고 기록한다(공식 클라이언트는 확인 불가). RakNet 코드는 어느 경우든 유지한다.

## 3. 세션
- 흐름: RequestNetworkSettings(버전 다르면 실패 상태 후 끊음) → NetworkSettings(압축) → Login → (RakNet만) 암호화 → 리소스팩 → StartGame → 청크 → 스폰.
- 로그인 전 상한: 로그인 마감 시간, 대기 패킷, 전체·IP별 연결 수에 상한을 둔다(넘치면 RakNet 0x14, 시그널링 503). 핑 응답은 IP별 속도 제한. 값은 M2·M3에서 정한다(gophertunnel은 로그인 마감 기본 꺼짐, 지연 패킷 큐 상한 없음).
- 로그인 검증(gophertunnel `minecraft/protocol/login`, `minecraft/service` 기준):
  - 온라인: OIDC 멀티플레이 토큰(RS256, JWKS)의 서명, 시간, `iss`(OpenID 설정의 issuer), `aud`(`api://auth-minecraft-services/multiplayer`). 클레임 `xid`, `xname`, `cpk`. ClientData(ES384, `cpk`)는 서명만 검증하고 내용은 신뢰하지 않는다.
  - 인증 환경은 서비스 디스커버리(`https://client.discovery.minecraft-services.net/api/v1.0/discovery/MinecraftPE/builds/<버전>`)에서 얻는다. 디스커버리, OpenID 설정, JWKS를 주기적으로 갱신하고 `data/auth.json`에 원자적으로 캐시한다. 실패하면 캐시로 돈다. 시작 때 받지도 못하고 캐시도 없으면 시작을 거부한다. 모르는 `kid`는 최소 5분 간격으로 한 번만 다시 받는다.
- RakNet 암호화(gophertunnel `minecraft/conn.go`, `packet/encryption.go`): 서버가 16바이트 salt를 ServerToClientHandshake의 ES384 JWT로 보낸다. 키 = SHA-256(salt ‖ ECDH-P384 공유 비밀). AES-256-CTR, IV = 키[0..12] ‖ 00000002. 범위는 0xfe 뒤 전부. 체크섬 = SHA-256(LE64(카운터) ‖ 평문 ‖ 키)의 앞 8바이트, 송수신 카운터 따로. 틀리면 끊는다(gophertunnel 주석의 CFB8은 틀렸고 코드는 CTR).
- 압축(사용자 결정: zlib과 Snappy 둘 다, 설정으로 고름, 기본 Snappy):
  - 설정(8.5절): `compression`, `compression-threshold`(기본 256), `compression-level`(zlib, 기본 6), `max-batch-bytes`(기본 16 MiB). 문턱 0은 받지 않는다(출처마다 뜻이 다름).
  - 문턱 256: PMMP, gophertunnel, Java판 기본값이다. 조사 때 측정에서 100바이트 안팎 배치는 압축하면 커졌다. 문턱은 NetworkSettings로 알리고 서버도 같은 값을 쓴다.
  - 형식: NetworkSettings ID 0 zlib, 1 Snappy. 배치 첫 바이트 0x00 zlib, 0x01 Snappy, 0xff 압축 안 함(문턱 미만). 협상한 ID와 0xff 말고는 끊는다. zlib은 헤더 없는 raw DEFLATE다. Snappy는 블록 형식이고 표준 원소만 받는다(오프셋 0, 쓴 길이 초과 오프셋, 선언 길이 초과, 남는 입력, 길이 불일치는 오류. copy-4는 받는다).
  - 해제 상한: `max-batch-bytes`를 세 방식 모두에, 로그인 전후 똑같이 건다. 16 MiB 근거: 정상 Login이 2 MiB를 넘는 일이 흔하고 8 MiB, 10 MiB 상한에서 실패 보고가 있다(pmmp/PocketMine-MP#3733). Snappy는 할당 전에 선언 길이가 상한이나 압축 크기의 22배를 넘는지 본다. zlib은 상한까지만 자라는 버퍼로 푼다.
  - 로그인 전: Login이 가장 큰 정상 배치라 상한을 줄이지 않는다. 대신 NetworkSettings 전 메시지 1 KiB 이하, 첫 배치의 첫 패킷은 Login(나머지는 검증 뒤 처리), 인증 전 세션 전체의 재조립·해제 바이트 합 8 × `max-batch-bytes` 이하.
  - 재조립 상한: 메시지 하나 `max-batch-bytes + max-batch-bytes/6 + 42`바이트(Snappy 최악). RakNet 조각 수 상한은 이것을 조각 크기로 나눈 값이다(512조각으로는 모자람). 조각 수만큼 미리 할당하지 않는다.
  - 출력: 배치는 256 KiB 전에 끊고 순서를 지킨다. 방송과 청크는 한 번만 압축해 나눠 쓴다.
  - Snappy 주의: gophertunnel 주석에 "AVX2 없는 기기에서 크래시"라는 확인되지 않은 보고가 있다(Mojira BDS-19188 정황). 공식 클라이언트 크래시는 AI가 관찰할 수 없어 검증하지 않고 `compression` rustdoc에 적는다. 서버 형식은 BDS 대조로 확인한다.
- 상한: 배치당 패킷 812개(gophertunnel 값)에서 시작. 목록 길이 상한은 필드마다 두고, 근거는 Mojang bedrock-protocol-docs `json/` 스키마의 `maxLength`, `maxItems`다. 입력은 연결마다 해제 바이트의 초당 예산(토큰 버킷), 출력은 대기 바이트 상한을 두고 넘치면 끊는다.

## 4. 프로토콜 파이프라인
- 출처는 sandertv/gophertunnel의 최신 정식 태그다(프리뷰 브랜치 금지). 2026-10-01 기준 v1.62.0 = 1.26.50(프로토콜 2193). 26.60(프리뷰 2223)은 정식 태그가 나오면 11절로 올린다. M1에서 태그 뒤 master의 형식 수정을 찾아 검토한다(같은 프로토콜이면 그 커밋으로 고정, 아니면 IR 보정 목록에).
- 추출기(Go, `xtask/go`): `golang.org/x/tools/go/packages`로 타입 정보까지 읽고, 각 패킷의 `Marshal(io protocol.IO)`를 걸어 IR(JSON)을 낸다. 모르는 구문이 하나라도 있으면 실패한다(조사 때 시제품이 Marshal 448개를 걸었다).
- 생성기(Rust, `cargo xtask gen`): IR로 코덱을 만든다. 생성물이 최신인지 verify가 검사한다. 이름은 CLAUDE.md 5.1을 따른다. 단어 나누기는 용어 표의 이름을 먼저 한 단어로 묶고, 나머지는 대소문자 경계로 나눈다(`HTTP|Server`). 숫자는 앞 단어에 붙인다. 모듈 이름과 겹치는 앞뒤는 뗀다(`packet::ViolationWarning`). 규칙에 안 맞는 이름은 예외 표에 적는다.
- 손으로 짜는 코덱: EntityMetadata, GameRule, ItemStack 계열, StackRequestAction, Bitset, UUID, Login 요청, 서브청크 페이로드 등 Marshal로 표현되지 않는 것.
- 없애는 gophertunnel 결함: 0과 1이 아닌 bool 거부, EntityMetadata 개수 상한, 쓰기에서 길이를 조용히 자르지 않음, 모든 길이를 할당 전 검사, NBT 깊이 512와 4 MiB, 로그인 마감과 큐 상한, 0xff 원문 상한.
- 시험(Go는 개발 때만, 모두 `cargo test`로 verify에서 돈다. Go가 없으면 실패):
  - P1 디코드 뒤 남는 바이트 0. P2 왕복 동일. P3 gophertunnel과 양방향 바이트 동일(배치 압축은 교차 해제).
  - P4 고정 시드 변형 입력: 외부 입력을 읽는 모든 디코더에서 패닉 0, 상한 초과 선언은 크기 오류.
  - P5 gophertunnel 봇이 RakNet과 NetherNet, zlib과 Snappy 각각으로 스폰. 큰 스킨 Login. 온라인 로그인은 시험용 발급자(자체 키로 만든 OpenID 설정, JWKS, 토큰)로 검증한다(실제 Xbox 토큰은 쓰지 않는다).
  - P6 기준 서버 대조(M2부터): 목표 프로토콜의 BDS 복사본(`.cache/bds/<버전>/`, 원본에서 실행 안 함)을 `online-mode=false`로 띄우고 콘솔 명령으로 상황을 만든다. 원본은 `C:\Users\User\Desktop\bedrock-server-1.26.52.3`(프로토콜 2193, M2에서 확인). 봇과 시나리오는 BDS에서 먼저 통과해야 믿는다. 같은 시나리오를 BDS와 Anchor에 돌려 받은 패킷의 순서, 종류, 정해진 필드를 비교하고(세계마다 다른 값은 뺀다), 다르면 BDS를 따른다. 시나리오는 마일스톤마다 늘린다.
- gophertunnel도 형식 버그를 자주 고친다. P3 통과는 gophertunnel과 같다는 뜻일 뿐이고, P6와 다르면 CLAUDE.md 7절을 따른다.

## 5. 틱과 스레드
- 20 TPS, 엔진 시계. 50ms를 넘으면 최대 1틱만 따라잡고 기록한다. 대기는 `std::thread::sleep`(Rust 1.98 Windows는 이미 고해상도 타이머).
  - 틱 간격(U8, 2026-10-05 실측): Windows 기본 타이머(15.625 ms)로 기다리면 50 ms 틱이 46.9/62.5 ms로 흔들려 다른 플레이어 이동이 끊긴다(Endstone SmoothMotion이 BDS에서 잰 원인). Rust 1.98 std 소스(`sys/thread/windows.rs`)의 `sleep`은 `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION` 타이머(Windows 10 1803 이상)로 기다리고, 만들지 못할 때만 `Sleep`으로 내려간다. 그래서 Anchor 틱 루프에는 이 문제가 없다. 실제 틱 루프 200틱(release, `tests/server.rs` `tick_intervals`, 무시된 측정 시험): 최소 49.0~49.3, p99 50.4~50.6, 최대 50.5~51.0 ms. 같은 시험에 15.625 ms 단위 대기를 일부러 넣으면 46.9/63.0 ms가 나와 시험이 실패한다. 비교(빈 루프 200회): tokio `sleep_until` 42.3~63.1 ms(p99 62.7, 타이머가 거칠다), 2 ms 전까지 자고 나머지 돌기 p99 50.1 ms(틱마다 CPU 2 ms를 태움). 클라이언트 보간 창(약 50 ms)에 0.5 ms 차이는 보이지 않으므로 std 대기를 그대로 둔다. `timeBeginPeriod`는 `unsafe`나 새 의존성이 필요하고 고해상도 타이머에는 필요 없다.
- 틱 함수는 `fn step(state: &mut State, input: &[Input]) -> Output`이고 입력은 (세션 ID, 도착 순번)으로 정렬한다.
- 단계: 1 입력(직렬: 입력, 수신 훅, 비동기 완료) → 2 계산(병렬 읽기 전용) → 3 적용(직렬: 결과, 이벤트, 예약 작업) → 4 출력(병렬 읽기 전용: 패킷 생성, 송신 훅).
- 직렬 단계 밖의 사용자 코드는 셋뿐이다: 송신 훅(읽기 전용), 월드 생성기(시드와 좌표만 받는 순수 함수), `spawn`/`spawn_blocking` 본문(게임 상태에 닿지 못함).
- 스레드 예산: 틱 1, 저장 1, 네트워크 워커, CPU 풀(설정 `network-threads`, `cpu-threads`). 블로킹 풀은 따로 상한(tokio 기본 512는 쓰지 않음).
- 결정성: 실수 `max`, `min`, `clamp`는 ±0을 비결정적으로 골라서 틱 코드는 엔진 도우미 하나를 쓴다. 입력 로그(입력, 비동기 결과, 시각)는 마지막 DB 커밋부터 기록하고, 패닉하면 `data/crash/`에 남긴다.

## 6. 월드와 저장
- 서브청크는 팔레트와 비트 배열이다. 월드는 영구와 임시(저장 안 함) 두 가지다. 생성기는 void와 flat, 사용자 생성기는 trait이다. 청크 전송은 시야, 틱당 수, 스폰 반경에 상한을 둔다(방식은 M6).
- 저장(redb, 파일 하나): 표는 `chunks`(키 = world_id, x, z, 종류를 고정 길이 빅엔디안, 부호 비트 뒤집음), `players`(키 = XUID, 오프라인은 `offline:`+이름), `world_meta`, 사용자 표, ops·화이트리스트·밴. 값은 압축 + 자체 체크섬이고, 디스크 값도 외부 입력으로 다룬다. 디스크에는 블록 이름과 상태만 쓴다.
- 커밋: 저장 주기(기본 10분)마다 틱 경계의 바뀐 부분을 COW 스냅샷으로 떼어 저장 스레드가 한 트랜잭션으로 커밋한다. 모든 쓰기 트랜잭션에 `set_quick_repair(true)`. 관리 명령(op, ban 등)은 다음 틱 경계에 커밋을 요청한다. 외부 시스템에 쓴 사용자 효과는 크래시 때 돌아가지 않는다(문서에 적는다).
- 실패와 손상: 커밋 실패는 핸들을 다시 열어 3번 재시도하고, 그래도 실패하면 플레이어를 내보내고 종료한다. redb 읽기는 페이지 체크섬을 확인하지 않으므로(cberner/redb#1334) 매 시작마다 닫힌 파일을 백업한 뒤 `check_integrity()`를 하고, 손상이면 시작을 거부하고 원본을 남긴다. 실행 중 손상 청크는 격리한다. 백업은 열기 전 닫힌 파일을 배타 잠금 아래 복사한다(M6에서 바꿈, 10.6). `compact()`는 시작과 종료 때만.
- M9 저장 형식: 4(항목 상한), 5(월드 기록에 시간 정지·난이도), 6(날씨), 7(meta에 커밋 식별자). 이전 형식은 모두 읽는다.
- M9 바꿈: 청크 묶음 읽기의 I/O 실패도 커밋처럼 핸들을 다시 열어 3번 재시도하고 그래도 실패하면 저장 없이 종료한다(손상만 격리, 입력 로그 `LoadFailed`). 앞 커밋이 실패한 뒤의 커밋은 거부한다(원자성). 틱이 죽으면(정상 종료의 `Finish` 없이 큐가 닫힘) 저장 스레드는 남은 요청을 커밋하지 않고 compact 없이 닫는다(4절 6번). 저장 형식 4: 지형은 서브청크마다 값 하나(종류 바이트 `0x20|번호`), 엑스트라 목록은 16 MiB 이하 쪽(쪽 0은 13바이트 키, 쪽 1부터 빅엔디안 u16을 붙임)으로 나눠 어떤 값도 레코드 상한을 넘지 않는다. 옛 형식(통째 지형 종류 0, 엑스트라 한 값)도 읽고, 지형을 처음 다시 쓰는 커밋이 24개를 모두 쓰고 옛 값을 같은 트랜잭션에서 지운다. 그 뒤에는 바뀐 서브청크만 쓴다. 형식 4 파일은 이전 Anchor가 열지 않는다(meta 버전 검사). 형식 5(M9 PMMP-A): 월드 기록에 시간 정지(bool)와 난이도(u8, 255는 서버 값), 4 이하는 흐름·없음으로 읽는다. 항목 하나 상한(사용자 결정, M9 R4-D1: PMMP에 없음): 스택 하나의 저장 형태 96 KiB(최대 손상 기준, 읽기 메모리 4배 이하, 이름 1,024바이트) 넘는 NBT는 `Stack::with_extra`가 `item::Error::TooLarge`로 거부, 셜커 상자 칸에는 셜커 상자와 3 KiB를 넘는 스택을 넣지 않음(요청·API는 거부, 놓기·읽기에서 안 맞는 스택은 옆에 떨어지거나 spill로 남아 잃지 않음; 꽉 찬 상자는 실측 82.1 KiB라 늘 스택 하나에 듦), 주민 거래 목록 저장 형태 합계 2 MiB, 플레이어 extra는 레코드 크기까지. 엑스트라 쪽은 바이트(16 MiB)와 읽기 메모리(64 MiB) 둘 다로 자른다(`nbt::Cost`).
- 서버 조명(사용자 결정): 게임 로직용으로 계산하고 클라이언트로는 보내지 않는다. 블록 값(밝기, 필터, 하늘빛 차단)은 PMMP 덤프에서 온다. 블록 변경은 증분 갱신하고 틱당 처리량에 상한을 둔다(PMMP는 상한 없음, dragonfly는 갱신 안 함). 시간대 보정은 PMMP 식을 표로 만든다(`cos` 금지). 계산 안 된 위치는 `None`. 설정 `lighting`(8.5): "dynamic"은 계산하고 블록 변경을 따름, "on-load"는 청크를 읽을 때만 계산, "fixed"는 계산하지 않고 모든 위치를 블록빛 0, 하늘빛 `fixed-light-level`(기본 15, 하늘에 열림)로 본다(0이면 작물이 안 자라고, 블록빛 15면 얼음과 눈이 녹아서). 그 결과(작물이 지하에서도 자람 등)를 rustdoc에 적는다.
- 랜덤 틱: 설정 `tick-radius`, `random-tick-speed`, `disable-block-ticking`(PMMP 관례). 대상은 반경 안이고 주변 3x3이 로드되고 조명 계산을 마친 청크다(`lighting = "fixed"`면 조명 조건은 만족). 청크 순서는 정렬 순서, 난수는 엔진 RNG(dragonfly의 비결정성을 고친다). 동작 기준은 PMMP.

## 7. 블록과 아이템 데이터
- 종류, 상태, 직렬화 규칙, 동작은 PMMP 5.44.3(Bedrock 1.26.30 데이터)에서 온다. 그 뒤 추가된 블록과 아이템은 없다(채우지 않는다).
- 추출(M4): PMMP 마지막 커밋 6a7cc02를 `.cache/`에 받아 한 번 실행해 덤프한다(소스 파싱 안 함). 블록 상태마다 Bedrock 직렬화, 조명 값, 랜덤 틱 여부를, 아이템마다 직렬화를 JSON으로 쓴다(조사 때 실측: 상태 11,420, 오류 0, 아이템 323). PMMP 상태와 Bedrock 상태는 1:1이 아니다(1,888개가 겹침. 그 값은 블록 엔티티나 런타임 데이터로 둔다).
- 버전별 번호는 기준 목록에서 온다(검증과 번호에만 쓰고 블록을 만들지 않는다): 블록 상태는 dragonfly `block_states.nbt`, 교차 확인 CloudburstMC `block_palette.nbt`(완전히 같을 때만 채택, 다르면 BDS를 따름). 아이템 런타임 ID는 dragonfly `vanilla_items.nbt`(버전마다 바뀜).
- 블록 ID는 해시 방식(FNV-1a 32, dragonfly `network_block_hash.go`)이다. 기준 목록 전체가 CloudburstMC `network_id`와 같은지 골든 시험한다. BDS도 해시 방식이다(M2 P6: `UseBlockNetworkIDHashes` true, 팔레트 값이 해시와 같음).
- 상태 대조: 구현한 모든 상태가 기준 목록에 있어야 한다. 26.50에서는 PMMP 블록 115개 이름에 속성이 생겼다: 울타리·판유리·철창·트립와이어 57개에 연결 속성(`minecraft:connection_*`), 계단 58개에 `minecraft:corner`(M4 실측). BDS는 이 속성을 서버에서 이웃으로 계산해 보내고, 이웃이 바뀌면 먼저 있던 블록을 다시 보낸다(M4 P6, 10.4). Anchor도 서버에서 계산한다(M6 블록 변경, M7 동작).
- 저장 업그레이드 규칙은 PMMP 업그레이드 스키마 형식이다(스키마 저장소에 결함이 있어 검증 후 들인다).
- 알 수 없는 블록은 클라이언트에 `minecraft:info_update`로 보내고 저장 원본은 보존한다. 클라이언트가 보낸 알 수 없는 블록이나 아이템은 그 요청만 거부한다.
- data-driven 블록 정의(M2 P6): BDS는 StartGame에 98개를 보낸다(색깔별 양털·콘크리트 계단·반 블록과 `shelf_mushroom`, `shrub`). 모두 PMMP에 없는 블록이라 Anchor는 보내지 않는다(1절). P6 고정 필드에서도 뺀다.

## 8. API
### 8.1 구조
```rust
// 설계 예시
fn main() -> Result<(), anchor::Error> {
    Server::builder()
        .config(Config::default().load("anchor.toml")?)
        .on(Priority::Normal, on_player_join)
        .command(Command::new("spawn").overload(spawn))
        .build()?
        .run()
}
```
- 게임 상태는 `Server`만 소유하고 콜백은 빌린다. 틱을 넘겨 보관할 때는 세대 번호 있는 `Copy` Id만 둔다. 사용자 상태는 타입 키 Extensions에 두고, 영속 타입은 빌드 때 등록해 같은 DB 커밋에 넣는다.
- 위치, 블록 상태, Id는 `Copy` 값이다. `ItemStack`은 불변 값이다. 조회는 O(1) 색인이고 반복 순서는 결정적이다.

### 8.2 이벤트
- PMMP 우선순위(`Lowest`~`Highest`)와 취소를 그대로 쓰고, MONITOR는 타입으로 읽기 전용인 `monitor`로 바꾼다. 리스너가 끝나면 엔진이 적용하고, 모니터는 적용 뒤 상태를 본다(조사 때 std 시제품으로 할당 0, 컴파일 강제 확인).
- 재진입 없음: 직접 API 호출은 이벤트를 쏘지 않는다. 게임 규칙을 거칠 일은 `perform`, 뒤이을 일은 `defer`(상한 있는 FIFO, 같은 틱).
- 이벤트는 스택 값이고 리스너가 없으면 만들지 않는다.
- 목록은 PMMP 이벤트를 합쳐 가져온다(M8에서 확정). 범위 밖 기능(레드스톤, 차원)의 이벤트는 뺀다. 제작·굽기·인챈트·수면·성장·랜덤 틱 이벤트는 가져온다. PMMP에 없는 거래 이벤트 둘(창 열기, 거래 1회)을 더한다.

### 8.3 확장 지점 (포크하던 이유 → 대신 쓰는 것)
커스텀 블록·아이템·엔티티(정의와 동작 trait, 엔진이 StartGame 등을 채움), 수신·송신 패킷 훅, 시작 게임 설정, 로그인 정책(PlayerLogin 이벤트), 권한, 명령(타입 있는 인자와 자동완성), 폼, 메뉴 창, 스코어보드·보스바·타이틀, 예약과 비동기 작업, 가짜 블록, 플레이어 데이터(Extensions), 생성기 trait, 리소스팩, 레시피 등록, 주민 거래(8.6), 설정(8.5).

### 8.4 PMMP 가져오기와 게임 범위
- 대상은 PMMP의 public, 비`@internal` API에서 범위 밖을 뺀 것이다. 대응표(M8)는 `#[doc(alias)]`로 만들고 상태는 구현함, 다름(이유), 병합함(대상), 안 함(이유)이다. 같은 일을 하는 메서드는 합친다(예: 메시지 전송 계열 → `send(Message)`).
- 고쳐서 가져오는 결함(대표): 전역 싱글턴, 가변 공유 벡터, 복제하는 게터와 NBT 재직렬화, 문자열 키, 이벤트마다 객체 할당, AsyncTask 직렬화, 원시 패킷 이벤트, 숨은 O(n) 조회, 재진입 디스패치, 주석뿐인 MONITOR, 게임 로직의 벽시계와 전역 난수, 비원자 저장, 로드 순서로 바뀌는 ID, 조명 갱신 결함, 모양을 검사하지 않는 shaped 레시피(dragonfly).
- 게임 범위(사용자 결정): PMMP 동작 ∪ dragonfly 야생 시스템 − CLAUDE.md 1절 제외. 블록은 PMMP에 있는 것만이라 퇴비통, 다시마, 숫돌은 없다. 숫돌은 안 함(사용자 결정 2026-10-05: 2026-10-04의 사용자 예외를 거둠. 블록, 아이템, 갈기 규칙, 생성기의 예외 파일을 지우고 PMMP에 없는 다른 블록과 같게 둠; PMMP에 숫돌 API가 없어 대응표 행은 없음).
- 범위 수정(사용자 결정, 2026-10-04): 서버 권위 부수기 시간 강제는 설정으로 끄고 켠다. 주민형 NPC는 생성 알 등으로 소환하면 거래 목록이 비어 있다(바닐라 기본 거래 템플릿 없음, 거래 API는 그대로). 기본 명령 status는 가동 시간·TPS·MSPT·메모리·스레드·접속자·월드별 청크·엔티티 수를 Anchor 고유 형식으로 보여 준다. Anchor는 기본 콘솔 로그를 낸다(시작·종료, 여는 주소와 포트, 월드 로드, 접속·퇴장, 경고·오류; 설정으로 끄고 수준을 고름; 사용자 바이너리가 자기 로그 방식으로 바꿀 수 있음 — CLAUDE.md 6절을 이 결정에 맞게 사용자 지시로 고침). 자연 날씨 순환은 없고(기본 맑음) 월드별 날씨 상태(맑음, 비·눈, 천둥)와 번개 소환(보이는 것과 소리, 피해·불은 선택)을 API로 준다(접속·월드 이동 때 상태를 보내고 패킷 훅을 거침). 사용자 정의 상태 효과와 인챈트를 등록하는 API는 만들지 않는다(사용자 결정 2026-10-05: 클라이언트가 새 효과·인챈트를 표시할 수 없어 뺌).
- 저작권(사용자 결정, 2026-10-04): 다른 서버(PMMP, BDS, dragonfly, gophertunnel 등)의 문장과 출력 형식을 그대로 쓰지 않는다. 서버가 직접 만드는 모든 문장(콘솔 로그, 명령 출력, 킥 사유, 오류 메시지, status 출력, 리소스팩 거절 사유 등)은 Anchor 고유 문장과 형식이다. 클라이언트 번역 키를 보내 클라이언트가 자기 문장을 보여 주는 것은 된다. 프로토콜이 요구하는 형식(핑 응답 필드 순서 등)은 형식이 아니라 규약이라 그대로 둔다. 생성 코드의 문서도 gophertunnel 주석을 옮기지 않는다.
  - 기본 동작: 배고픔(설정으로 끔), 액체와 양동이, 낙하 블록, 폭발, 불 피해, 발사체, 물약 구름, NPC, 떨어진 아이템과 경험치 구슬.
  - 제작(PMMP bedrock-data 레시피, smithing 포함, 사용자 레시피 API), 화로 계열과 모닥불, 양조기, 인챈트대, 모루, 대장장이 작업대, 석재 절단기, 베틀, 신호기, 뼛가루. PMMP에 동작이 없는 모루, 대장장이 작업대, 베틀, 신호기 결제는 dragonfly를 참고한다.
  - 농사(작물, 줄기, 사탕수수, 선인장, 코코아, 네더 사마귀, 대나무, 덩굴, 열매, 경작지, 묘목에서 나무), 랜덤 틱 계열(잎 부식, 불 번짐, 잔디 번짐, 얼음과 눈 녹음, 구리 산화), 침대와 수면, 경험치, 조명, 주민 거래 조작.

### 8.5 설정 파일 (`anchor.toml`, 사용자 결정)
- 한 파일에 `[general]`(누구나 정하는 값)과 `[developer]`(성능·프로토콜, 주의 필요) 두 표. PMMP 설정이 관례이고 키는 PMMP 이름 그대로(kebab-case)다. 더하는 키는 꼭 필요한 것만이다.
- 기본 파일(`Config::load`가 파일이 없을 때 만든다. 아래가 정본이다):
```toml
# Anchor server settings. Anchor reads this file at start and never writes it again.
# A wrong value or an unknown key stops the start with a message naming the key.
# A key left out keeps its default, so new Anchor versions start with an old file.

# Settings for anyone opening a server.
[general]
# Name shown in the server list.
motd = "Anchor Server"
# Addresses to listen on. "0.0.0.0" and "::" mean every interface.
server-ip = "0.0.0.0"
server-ipv6 = "::"
enable-ipv6 = true
# UDP: RakNet and server list ping. TCP: NetherNet signaling. Both use this port.
server-port = 19132
# UDP port for NetherNet game data, shared by every player.
nethernet-port = 19133
# Address players use to reach this server, if it differs from the local one (NAT).
# Empty: advertise the addresses of the local interfaces.
public-ip = ""
max-players = 20
# true: players must sign in to Xbox Live. false: anyone can join under any name.
xbox-auth = true
# true: only listed players can join. Manage the list with the whitelist command.
white-list = false
# Default for new players: "survival", "creative", "adventure" or "spectator".
gamemode = "survival"
# true: players get the default game mode on every join.
force-gamemode = false
# "peaceful", "easy", "normal" or "hard".
difficulty = "normal"
hardcore = false
pvp = true
# Each switch below turns a game system off completely when false: its rules stop running.
hunger = true
# Gaining experience points and levels. false also means no experience orbs.
experience = true
# Experience orbs and throwing bottles o' enchanting.
experience-orbs = true
fall-damage = true
# Air supply under water, drowning and suffocation damage.
drowning = true
# Armour protection and armour wear.
armor = true
# Tools, weapons and armour wear out with use.
durability = true
# Sleeping in beds and bed spawn points.
beds = true
# The time of day moving on.
daylight-cycle = true
# The recipe book: false hides it and refuses its quick crafting; the crafting grid still works.
recipe-book = true
# The vanilla recipes; false keeps only recipes the server program registers.
vanilla-recipes = true
# Education Edition items (elements, compounds and their tools). false: they are not offered in the creative inventory or commands.
education-items = true
# Ticks a dropped item stays on the ground (1 to 72000).
item-despawn-ticks = 6000
# true: a hit while sprinting can be critical.
sprint-critical-hits = false
# The server checks that a block took long enough to break.
break-time-check = true
# The server checks that a move is not too far (more than 15 blocks in one input).
movement-check = true
# Main world. Created on first start with level-type: "flat" or "void".
level-name = "world"
level-type = "flat"
# Whole number as text. Empty: random, chosen once when the world is created. Other text stops the start.
level-seed = ""
# Largest view distance a player may use, in chunks (2 to 64).
view-distance = 16
# false: player inventories and positions are not kept between joins.
save-player-data = true
# Copies of the world file in data/backups, at start and with the backup command.
backups = true
# Message players see when the server stops.
shutdown-message = "The server has stopped"
# Resource pack files (.mcpack or .zip), relative to this file. Earlier entries override later ones.
# An encrypted pack needs its key in "<file>.key" next to it.
resource-packs = []
# true: players must accept the resource packs to join.
force-resources = false
# Console log detail: "off", "error", "warn", "info", "debug" or "trace".
log-level = "info"

# Settings that tune performance and protocol behavior. Change them with care.
[developer]
# Network compression: "snappy" (less CPU) or "zlib" (less bandwidth).
compression = "snappy"
# Batches smaller than this many bytes are sent uncompressed (1 to 65535).
compression-threshold = 256
# zlib only: 1 (fastest) to 9 (smallest).
compression-level = 6
# Largest decompressed batch a client may send, in bytes (1 MiB to 64 MiB).
max-batch-bytes = 16777216
# false: RakNet game traffic is sent in plain form. Anyone between a player and the server
# can then read it and change it, login included. Use false only on a trusted network.
# NetherNet traffic is always encrypted by its own transport.
enable-encryption = true
# false: accept only NetherNet. The RakNet port still answers server list pings.
raknet = true
# Largest RakNet packet in bytes (576 to 1492). Lower it behind tunnels or VPNs.
max-mtu-size = 1492
# PEM certificate chain and private key for HTTPS NetherNet signaling.
# Empty: plain HTTP, and players confirm the server key on first join.
nethernet-tls-cert = ""
nethernet-tls-key = ""
# Threads for network work and for parallel game work: "auto" or a number.
network-threads = "auto"
cpu-threads = "auto"
# Chunks sent to each player per tick.
chunks-per-tick = 4
# Chunk radius sent before a player spawns (at most view-distance).
spawn-radius = 4
# Chunk radius around players where blocks and entities are ticked (at most view-distance).
tick-radius = 4
# Random block ticks per sub-chunk per tick (crop growth, leaf decay). 0 turns them off.
random-tick-speed = 3
# Blocks that never get random ticks, by name, for example "minecraft:grass_block".
disable-block-ticking = []
# true: a world without players runs one tick in every 100.
slow-empty-worlds = false
# "dynamic": light is calculated and follows block changes.
# "on-load": light is calculated when a chunk loads but not after block changes.
# "fixed": no light is calculated; every block has fixed-light-level as sky light.
lighting = "dynamic"
# Sky light everywhere when lighting is "fixed" (0 to 15). Block light is then 0.
fixed-light-level = 15
# Ticks between checks for items a player can pick up (1 to 20).
item-pickup-interval = 1
# false: movement packets the server sends skip the send hooks.
hook-movement-packets = true
# false: PlayerAuthInput packets skip the receive hooks.
hook-player-input = true
# Minutes between whole-state saves (1 to 1440).
save-interval-minutes = 10
# When the world file is checked at start: "always", "after-unclean-shutdown" or "never".
# The check command checks it while the server runs.
integrity-check = "always"
# false: Anchor does not read commands from standard input.
console-input = true

# Per-world values, by world name. They override view-distance, chunks-per-tick,
# spawn-radius and tick-radius for that world, for example:
# [worlds.lobby]
# view-distance = 8
# chunks-per-tick = 8
```
- PMMP에서 이름을 바꾼 것: `batch-threshold` → `compression-threshold`, `blocks-per-subchunk-per-tick` → `random-tick-speed`(Java판 이름과 기본값 3, Bedrock 기본 1과 다름), `chunk-sending.per-tick` → `chunks-per-tick`, `ticks-per.autosave` → `save-interval-minutes`, 중첩 경로는 평평하게. PMMP에 없는 키: NetherNet 포트와 TLS, `public-ip`, `max-batch-bytes`, `raknet`, 스레드 수, `lighting`, `hunger`(사용자가 확정한 토글). M9(사용자 결정, 2026-10-04): PMMP `network.enable-encryption` → `enable-encryption`, PMMP `debug.level` → `log-level`(끄기와 모든 수준). PMMP에 없는 M9 키: 기능 스위치(`experience`, `experience-bottles`, `fall-damage`, `drowning`, `armor`, `beds`, `daylight-cycle`, `recipe-book`, `vanilla-recipes`, `break-time-check`, `movement-check`, `backups`), `item-despawn-ticks`, `sprint-critical-hits`, `slow-empty-worlds`, `lighting`(값 셋)·`fixed-light-level`, `item-pickup-interval`, `hook-movement-packets`, `hook-player-input`, 월드별 `[worlds.<이름>]` 표(`view-distance`, `chunks-per-tick`, `spawn-radius`, `tick-radius`). 옛 `lighting = true/false`도 받는다(true → "dynamic", false → "fixed", 15).
- PMMP에서 뺀 것: PHP 전용(메모리, GC, async-workers, timings, 통계·보고·업데이트), 플러그인, 범위 밖(query, upnp, 월드 형식), 구조로 대체(XUID 검사, 생성기 문자열, 별칭, 월드 목록, 언어, IPv6 포트, auto-save). 암호화(항상 켬이었음)와 디버그 레벨은 M9에서 사용자 결정으로 설정이 됨.
- 기능 스위치(사용자 결정, 2026-10-04): false인 기능은 그 코드가 아예 돌지 않는다(틱 작업, 이벤트, 패킷, 메모리 0). 숨기기만 하지 않고, 끈 상태에서 그 코드가 실행되지 않음을 시험으로 확인한다. 설정에 없는 동작은 가장 성능적이고 안전하며 어떤 서버에서도 맞는 기본값이다. 새 키의 기본값은 지금 동작과 같다.
- 검증: 모르는 키와 표, 타입 오류, 범위 밖 값, hardcore인데 hard가 아님, `[worlds]`에 선언되지 않은 월드 이름은 시작을 거부한다(조용히 고치지 않는다). 빠진 키는 기본값이다. 모르는 표 검사는 `build()`에서 한다. `spawn-radius`와 `tick-radius`의 실제 값은 min(값, `view-distance`)이다.
- 우선순위: 코드가 최종 `Config` 하나를 만들고 나중에 쓴 값이 이긴다(`Config::default()` → 코드 기본값 → `load`가 파일 키를 덮음 → 강제할 값). `config()`를 안 부르면 파일을 읽지도 쓰지도 않는다. 엔진은 파일을 다시 쓰지 않고, 실행 중 변경은 종료까지만 유지된다.

### 8.6 주민 거래 조작 (사용자 결정, PMMP와 dragonfly에 없음)
- 범위: 주민형 엔티티(AI 없음)의 거래 목록을 설정하고 바꾸는 API, 서버 권위 거래 처리, 거래 이벤트. 바닐라 주민 경제는 하지 않는다.
- 확인된 프로토콜: UpdateTrade 필드 순서(gophertunnel `packet/update_trade.go`), 창 종류 15, 오퍼 NBT의 `Recipes`와 `netId` 키, Trade2 입력 슬롯 4·5와 출력 50(ItemStackRequest에서는 컨테이너 47·48·49).
- 검증 필요(M7), P6로 정한다(BDS에서 `summon villager_v2` 후 봇이 거래 1회): 나머지 오퍼 NBT 키, 거래 요청의 액션 순서와 `netId` 대응, 레거시 villager 액터로 Trade2 화면이 열리는지, `DemandBasedPrices` 값, 거래 뒤 갱신 방식.
- 원칙: 가격은 서버가 실제 개수로 보낸다(클라이언트 가격식에 기대지 않음). 요청 ID 한 번만, 최신 뷰 리비전만, 재료와 남은 횟수 검사, 반복 상한 256, 적용은 한 단위이고 실패하면 모두 되돌린다. 창을 닫거나 대상이 사라지면 입력 슬롯 아이템을 돌려준다. 거래 데이터는 엔티티와 같은 DB 커밋에 든다.

## 9. 외부 의존성과 데이터 출처
### 9.1 들인 것
| 이름 | 고정 | 용도 | 결함과 조치 |
|---|---|---|---|
| sandertv/gophertunnel(MIT) | 80c811b(v1.62.0 뒤, 1.26.50, 2193) | 패킷, 로그인, 암호화, 압축 형식, Go 봇 | 형식 버그 수정이 잦음, 주석과 코드 불일치, 4절 결함, NBT map 쓰기 순서가 무작위(시험 도구가 정렬해 씀). 로그인(M2): 정규식에 앵커가 없어 이름·버전 검사가 느슨함 → 모든 글자 검사, `ServerAddress`를 DNS로 조회(블로킹) → 모양만 검사, 오프라인 UUID가 무작위 → `OfflinePlayer:`+이름의 v3, base64가 느슨함 → 엄격. 봇은 기본 스킨 JSON 두 개를 복사해 씀 |
| sandertv/go-raknet, df-mc/go-nethernet | gophertunnel go.mod가 고정 | RakNet, NetherNet 형식, 봇 | go-nethernet은 비신뢰 채널을 읽지 않음. go-raknet(M2, Anchor는 고쳐 구현): 쿠키가 CRC32, 오류마다 IP를 10초 막음(위조로 남을 막음), OCR2 응답 손실에 멈춤, 신뢰 인덱스 중복 제거 없음(조각 묶음 누수), 24비트 번호 넘침 미처리, 조각 512개 상한, 재조립 O(n²), 모르는 ACK 종류 무시, 혼잡 제어 없음, 사실상 상한 없는 큐, MTU 400 미만 처리 불일치, 순서 채널 무시 |
| pmmp/PocketMine-MP(LGPL-3.0, 보관) | 6a7cc02(M3부터 `.cache/pmmp`, M4부터 의존성은 `composer.lock` 그대로 `vendor`) | 블록·아이템 종류와 동작, API, 이름 규칙과 중복 로그인(M3), 덤프(M4) | 1.26.30 데이터, 상태가 1:1 아님, 공개 때 라이선스 검토. 중복 판정에 UUID도 쓰지만 Anchor는 사용자 결정대로 XUID·이름만. M4 덤프에서: smithing 레시피를 읽지 않음 → 덤프가 PMMP 아이템 해석기로 읽음, 모르는 인챈트를 버려 크리에이티브에 빈 인챈트 책 61개가 남음 → 뺌, 불길한 배너를 검은색(meta 0)으로 둠 → BDS처럼 흰색(15), 블록 아이템 상태가 바닐라 아이템 상태와 다름 → 네트워크 형태만 바닐라 상태(10.4) | M7: 게임 규칙 전체의 동작 기준. 골든은 PMMP 코드를 PM5 PHP로 실행해 만든다(`xtask/php/place.php` → `place.json` 놓기 202,358개, `liquid.php` → `liquid.txt` 액체 5장면, 조명·부수기 시간 시험값). 고친 결함은 10.7 결과의 각 항목.
| pmmp/PHP-Binaries | php-build-524, PHP 8.4 PM5 Windows(zip sha256 `cf4023b8549c99eec47cfd403805ac7d78100fa366394dab4e45bb544dcc9be4`), `.cache/php-pm5` | PMMP 덤프 실행(개발 도구) | 일반 PHP에는 PMMP 확장이 없음 → PM5 빌드 |
| pocketmine/bedrock-data(CC0) | 6.7.0+bedrock-1.26.30 | 레시피 등 데이터 | 26.40 이후 없음 |
| df-mc/dragonfly(MIT) | v0.11.5 `4c7b5074`(1.26.50), `.cache/dragonfly` | 기준 목록(블록 상태, 아이템, 크리에이티브 아이템 상태), 야생 시스템 참고 | 해시 함수 패닉(Anchor는 코드를 쓰지 않고 시험에서 다시 구현), 비결정 코드, 조명 갱신 없음. 크리에이티브 목록은 빈 블록 속성을 생략함(빈 속성으로 읽음) | M7 참고(PMMP에 없음): 구리 산화, 모루 규칙(호환 안 되는 인챈트만이면 1레벨을 받고 아무것도 안 함 → 거부), 화로 경험치 값. | M8: 커스텀 블록 네트워크 해시 골든 3개를 dragonfly Go 코드(`go run`, 개발 때만)로 뽑음, 폼 JSON 모양 참고(v0.11.5).
| CloudburstMC/Data(Apache-2.0) | `659ce1e`(26.50), `.cache/cloudburst-data` | 블록 ID 골든, 교차 확인, 크리에이티브 아이템 상태, 바이옴 정의(M6) | 팔레트는 gzip big-endian NBT. dragonfly와 블록·아이템 목록이 같음을 도구가 확인한다. 바이옴(M6): 생성 데이터가 든 `biome_definitions.json`은 쓰지 않고 stripped 판만, 밀도 4개는 패킷 필드가 없어 0이 아니면 dump 실패, 같은 키 중복은 serde가 덮어씀(89개 수를 시험·P6이 확인) | M7: `entity_identifiers.dat`(AvailableActorIdentifiers, BDS와 바이트 같음), `voxel_shapes.json`(VoxelShapes, 개수·크기 BDS와 같음)을 `xtask/data/reference/`에 그대로 복사. | M9 긴급 수정(2026-10-04): `jigsaw_structure_data.nbt`, `entity_properties.nbt`를 dump(`xtask/go/reference/network.go`)가 네트워크 NBT(`jigsaw_structure_data.dat`, `entity_properties.dat`)로 바꿔 씀(BDS와 크기·값 같음). `entity_identifiers.dat`, `voxel_shapes.json`도 이제 dump가 복사하고 `--check`가 확인함.
| Mojang/bedrock-protocol-docs(Minecraft EULA, All rights reserved) | v1.26.50 `475bd72`(프로토콜 2193; 1.26.51·automated/1.26.52와 스키마 같음), `.cache/bedrock-protocol-docs` | 필드 길이 상한(M9 R1-K2), NetherNet 가이드 | 숫자만 옮김(스키마 파일 복사 안 함, 공개 때 라이선스 검토). 문자열 상한 단위가 문서에 없음 → 바이트(gophertunnel Text 검사가 같은 숫자를 바이트로 씀, 768=256×3), 검증 필요(M9 P6: BDS가 여러 바이트 글자를 상한 바이트 넘게 받는지). gophertunnel과 다른 곳: 1,024보다 큰 목록 상한 18개는 공통 상한 1,024가 더 좁음, 고정 배열·필드 11곳은 안 씀(dump 표 UNUSED), 맵 상한은 목록에, 맵 키는 필드에, 바이트 색인 목록은 바이트 조각에 적용. minLength 안 씀. gophertunnel과 다르면 CLAUDE.md 7절 |
| BDS(기준 서버) | 1.26.52.3(프로토콜 2193), `.cache/bds/` 복사본 | P6 대조 | 실행만, 분석하지 않음. RakNet 전송을 "플레이어 접속 불가"로 기록함 | M7: 게임 시나리오 craft·furnace·enchant·crops·trade(`p6 game bds`), 사실은 `xtask/data/p6/m7-facts.md`. 주민 거래 창 이름은 BDS behavior pack 데이터 표에서 옮김(실행 파일 분석 아님). 재료에 태그 하나를 개수 2로 넣은 CraftRecipeAuto에 BDS가 멈춤(Anchor는 클라이언트 재료를 믿지 않음) | M8: `p6 api`(commands·particles·kick·forms·script), 폼은 스크립트 팩 `xtask/data/p6/forms-pack`(@minecraft/server-ui 2.0.0, 실험 토글 없음, 복사본 `development_behavior_packs`에만), 사실은 `m8-facts.md`, 의도된 차이는 `intended.txt` | M9: 카메라 프리셋·조준 보조 프리셋은 BDS 캡처에서 옮김(`xtask/data/reference/bds_camera.json`, 실행 파일 분석 아님)
| libdeflater(MIT, libdeflate 1.26) | =1.26.1 | zlib 압축(level 6에서 miniz_oxide보다 2~4배 빠름, 실측) | `use_rust_alloc` 기능의 OOM null 역참조 → 기본 기능만. `Decompressor` 내부 상태 패닉 → `.clippy.toml` 금지. `Compressor::new`는 malloc 실패에만 패닉(생성 때만, 외부 입력 닿지 않음) |
| miniz_oxide(MIT/Apache-2.0/Zlib) | =0.9.1 | zlib 해제 | `decompress_to_vec*`는 상한 없음 → 금지, 상한까지만 자라는 자체 루프 |
| serde, serde_json, toml | =1.0.229, =1.0.151, =1.1.6 | 설정, 인증 캐시, JWT, IR 읽기 | toml은 깊이 80에서 멈춤(`unbounded` 기능이 켜지면 사라짐), toml 오류 `Display` 안의 `expect` → `message()`와 `span()`만 씀. `serde_json::from_reader` 금지 |
| tokio(MIT) | =1.53.1 | 런타임, 상한 큐, UDP, 시그널 | 상한 없는 채널, `blocking_*`(mpsc `blocking_send`·`blocking_recv`·`blocking_recv_many`, oneshot·broadcast `blocking_recv`), 상한 없는 io-util 읽기(`read_to_end`·`read_to_string`·`read_line`·`read_until`·`lines`·`split`), `tokio::sync::Mutex`·`RwLock` 타입, `block_on`, 런타임 생성, 시계 진입점을 금지 목록에 둠 |
| aws-lc-rs(ISC/Apache-2.0) | =1.18.1, 기본 기능 끔 | ES384, RS256, ECDH P-384, SHA-256, AES-256-CTR, 난수 | ECDSA 공개키가 SPKI 뒤 바이트와 SEC1 점도 받음 → DER SEQUENCE 하나만, 스트리밍 CTR이 무작위 IV → 금지하고 자체 카운터, 서명 키와 ECDH 키가 다른 타입 → PKCS#8로 잇고 공개키 같음을 확인, 난수와 키 생성 진입점(`rand::fill`·`generate`, 각 키 `generate*`, `kem` 캡슐화, RSA 암호화, `RandomizedNonceKey` 봉인)과 `SystemRandom` 타입 금지 |
| rustls(aws_lc_rs, ring 끔), webpki-roots | =0.23.45, 1.0.9 | 인증 문서 HTTPS | 공급자가 없으면 패닉 → 늘 aws-lc-rs를 줌. ring은 메타데이터에만 있고 빌드되지 않음(`allowed-duplicate-crates` windows-sys) |
| ureq(MIT/Apache-2.0) | =3.4.2 | 인증 문서 받기(블로킹 풀) | 시간 상한이 있으면 DNS마다 분리 스레드 → 인라인 resolver(DNS는 OS 상한), 본문 상한이 상한과 같은 크기를 거부 → 1 MiB 미만, 기본으로 프록시 환경 변수를 읽음(둠). 블로킹 호출(`run`, `RequestBuilder::call`·`send*`, `Agent::run`, `RequestExt::with_agent`·`with_default_agent`)과 상한 없는 본문 읽기 금지 |
| tracing(MIT) | =0.1.44, std만 | 로그 | `attributes` 기능을 꺼 syn 2를 뺌 |
| rayon(+rayon-core, crossbeam-deque·epoch·utils, either)(MIT/Apache-2.0) | =1.12.0(rayon-core 1.13.0, crossbeam-epoch 0.9.21) | CPU 풀(M5) | 전역 풀을 지연 생성하는 진입점(`join`, `scope`, `spawn`, 병렬 반복자 등) → `.clippy.toml` 금지, `pool.rs`만 씀(임시 코드로 걸림 확인), 풀 drop이 스레드를 기다리지 않음 → `exit_handler`로 세어 종료 때 기다림, 잡 패닉은 호출한 쪽(틱)으로 전파됨(확인), 내부 결함·패닉 처리기 없는 spawn 잡 패닉은 abort → spawn을 쓰지 않음. 열린 RustSec 권고 없음(crossbeam 권고는 고친 판보다 새 판) |
| socket2(MIT/Apache-2.0) | =0.6.5 | IPv6 전용 소켓, 소켓 버퍼 | 블로킹 입출력 진입점 금지 |
| str0m(+is, str0m-proto, str0m-aws-lc-rs)(MIT/Apache-2.0) | =0.24.1(0.11.1, 0.7.0, 0.6.0), 기본 기능 끔 + `aws-lc-rs`. str0m은 `vendor/str0m` 사본 | NetherNet WebRTC(SDP API는 원격 정보 입력에만, 응답 SDP는 자체) | 사본에서 고친 것: ①·②는 0.24.1(#1062)이 같은 내용으로 고쳐 사본에서 뺐다(M9; 0.24.1의 그 밖 변경인 DTLS 초기화 전 가드, SDP msid, pacer 버스트는 결함 없음). ① SCTP 송신 버퍼가 128 KiB 고정이라 그보다 큰 메시지(262,144바이트 조각)를 영원히 못 보냄 → `set_sctp_max_buffered_amount` 설정(Anchor 1 MiB), ② `poll_output`이 대기 SCTP 패킷마다 재귀해 큰 송신에서 스택 넘침(실측, `catch_unwind`로 못 막고 프로세스가 죽음) → 반복문, ③ SCTP RTO 초기값이 RFC 4960의 3초라 INIT를 잃으면 3초를 기다림(손실 10%에서 3.8%가 성립 마감 10초를 넘김) → RFC 9260의 1초(`sctp::RTO_INITIAL_MS`, 클라이언트·서버·SNAP 설정, M6. 상류 main에도 RTO 설정이 없음). 재현: `a_lost_sctp_init_is_sent_again_after_one_second`, 사본 안 `many_remote_streams_stay_bounded`(0.24.1 원본에서 실패). 그 밖: 기본 설정이 전역 공급자·fastrand ICE 자격·자체 인증서 → 모두 명시 설정과 금지 목록, 직접 API는 원격 `max-message-size`를 못 받음 → SDP API에 `setup:passive`로 적은 오퍼를 줌(DTLS client 역할 유지), ICE는 끊김만 알림·SCTP 무한 재전송 → 피어 마감과 무응답 10초, `accepts`는 선형 → ufrag·주소 색인, 남은 내부 패닉 → 피어마다 `catch_unwind`, ICE 비밀번호가 trace에 찍힘 → `pii` 끔, 설정 로그 금지 M9 사본 수정: ④ 클라이언트 SCTP가 받는 스트림 수를 65,535로 알려 원격이 스트림 id마다 DATA를 쓰면 항목이 id마다 생기고 poll마다 모두 훑음(구동 작업이 수십 초 멈춤) → 클라이언트 `TransportConfig`에 `with_max_num_inbound_streams(16)`(`sctp::MAX_INBOUND_STREAMS`, NetherNet 채널 id 1·3), ⑤ 모르는 스트림의 ChannelData 버림을 warn으로 찍어 원격이 로그를 채움 → debug. 재현 사본 안 `sctp::tests::many_remote_streams_stay_bounded`(원본 실패). M9 시험(사용자 결정): verify는 사본 lib 시험만 돈다. 통합 시험 1,078개는 공개 크레이트에 없는 캡처 7개(5.4 MB)가 필요해 커밋하지 않고, 사본을 바꿀 때만 xtask 명령이 상류 95e5038c에서 `.cache`로 받아 sha256을 확인한 뒤 돌린다(M9 0.24.1 실행: 통합 시험 1,086개 통과, 바이너리 69개, 캡처 sha256 같음). |
| str0m-proto, str0m-aws-lc-rs, is(MIT/Apache-2.0) | 0.7.0, 0.6.0, 0.11.1, `vendor/` 사본(바꾸지 않음, M9) | str0m과 dimpl 사이 | str0m 0.24.1도 같은 판을 요구해 그대로. crates.io .crate에 라이선스 파일이 없어 str0m 저장소의 LICENSE-MIT.txt만 더했다(M9 저작권, 저장소에 Apache 파일 없음). 그 밖은 .crate와 바이트 단위로 같고(sha256 = Cargo.lock checksum) Cargo.toml의 path 줄만 다르다. 사본이 없으면 레지스트리 dimpl이 따라와 dimpl이 둘이 된다(타입 불일치, 결함 원본). 코드는 전부터 빌드에 있던 것이라 str0m 행의 분석이 그대로다. str0m 릴리스로 돌아갈 때 함께 지운다 |
| dimpl(MIT/Apache-2.0) | 0.7.5, `vendor/dimpl` 사본 | DTLS 1.2 | 사본에서 고친 것: ① ChangeCipherSpec 전 같은 epoch-1 레코드 두 번에 assert(engine.rs:364) → replay처럼 버림, ② 피어 인증서가 poll 버퍼(str0m 2,000바이트)보다 크면 assert(dtls12·13 client.rs) → `BufferTooSmall`을 돌려 str0m이 오류로 끊음, ③ Auto 모드 supported_versions 잘림에 범위 밖 읽기(auto.rs:436) → 길이 검사, ④(버퍼 풀이 자람)는 0.7.5(#167)가 고쳐 사본에서 뺐다. 상류는 풀 밖 버퍼 반납에 release `assert!`를 두지만, 0.7.5의 반납 지점과 aws-lc-rs 구현을 모두 읽어 풀 밖 버퍼가 반납되는 길이 없음을 확인했다(M9, 판을 올릴 때마다 다시 봄). 재현: `xtask/tests/dimpl_repro.rs` 4개(0.7.5 원본에서 모두 실패). 레코드 순서: 0.7.5(#169)는 한 데이터그램 안 순서가 바뀌면 그 데이터그램을 통째로 버리고 재전송으로 회복한다(`records_reversed_inside_one_copy_of_a_datagram_are_recovered_by_retransmission`). 모든 사본이 바뀌면 그 피어만 10초에 닫힌다. 중복이 일으키는 재전송에 backoff가 생겼다(Duplicate·LoseFirst 피어 시험 통과). DTLS 1.2 고정, `log` 사용 M9: verify가 사본 시험 560개를 기본 기능으로 돈다. openssl vendored 개발 의존성 빌드에 Strawberry Perl portable 5.42.3.1을 `.cache/tools`에 두고 그 명령에만 `OPENSSL_SRC_PERL`로 넘긴다(처음 빌드 약 19분). wolfssl 시험은 Windows가 아닐 때만. 이 명령만 MSVC 링커를 쓴다(rust-lld는 OpenSSL 정적 라이브러리에 없는 디버그 정보에 LNK4099 경고를 내고 경고는 verify 실패). |
| sctp-proto(MIT/Apache-2.0) | 0.10.5, `vendor/sctp-proto` 사본 | SCTP | FORWARD-TSN `+1` 루프는 0.10.4에서 고침(확인). 사본에서 고친 것: ① 수신 창이 0이어도 빠진 청크를 무제한 받음 → 누적 TSN 바로 다음 청크만(RFC 4960 6.2), ② 받는 TSN 기록이 읽힌 데이터를 붙잡고 창에 안 셈 → 데이터 없이 TSN만 두고 기록마다 창에서 뺌, ③ 스트림 수 협상이 뒤집힘(INIT·INIT-ACK) → RFC 4960 5.1.1대로(업스트림 시험 기대값도 바로잡음), 협상 수 밖 스트림은 만들지 않음, 다시 연 스트림은 대기열에 한 번만, ④ 같은 원격의 새 INIT마다 연결이 생겨 쌓임 → 원격 하나에 연결 하나(RFC 4960 1.4), ⑤ 빈 DATA를 받아 둠 → 연결 종료(RFC 4960 6.2), ⑥ SACK 갭 오프셋 u16 넘침과 TSN `+1` 넘침(디버그 패닉) → 순환 산술. 재현: 사본 안 `association/anchor_test.rs`  8개와 `endpoint/anchor_test.rs` 1개(원본에서 모두 실패, verify가 사본 시험을 따로 돌림). M5에서 고친 것: ⑦ 보낼 때와 받을 때 청크마다 큐 전체를 다시 정렬(순서가 어긋난 폭에 비례해 청크당 비용이 커짐, 실측 이차) → 정렬된 자리에 넣음, ⑧ poll마다 전송 중인 청크 전체를 재전송 탐색 → T3 타임아웃 표시가 없으면 건너뜀. 재현: `pushing_a_tsn_does_not_sort_the_whole_queue`, `polling_without_a_retransmission_timeout_skips_the_chunks_in_flight`(원본에서 시간 한도 초과로 실패). 남김: SSN 정렬 비교자가 전순서가 아님(실사용 방식 약 400만 번에서 재현 안 됨 → 고치지 않음, 패닉이 나도 피어 하나만 닫힘), 연결 중 INIT·ERROR 청크에 연결을 닫음(RFC 위반, 그 피어만 끊김), `Endpoint::connect`가 시계를 읽음, `log` 사용 M9: ③에 더함 — 협상 수 밖 스트림의 DATA는 TSN을 인정(SACK)하고 데이터만 버림(RFC 4960 6.5, ERROR 청크는 안 보냄; 전에는 인정하지 않아 누적 TSN이 멈춤). ⑥에 더함 — SACK 갭 블록 루프의 `cumulative_tsn_ack + i`도 순환 산술(재현 `a_sack_gap_block_across_the_tsn_wrap_is_processed`). 재현은 `association/anchor_test.rs` 9개. |
| tokio-rustls(MIT/Apache-2.0) | =0.26.6, `aws_lc_rs`·`tls12`만 | HTTPS 시그널링 | 0.26.5 회귀(#204) 피함, 기한 없음 → 요청 10초 안에서만, 전역 공급자 builder 금지(`.clippy.toml`), 티켓과 세션 저장 끔 |
| cberner/redb(MIT/Apache-2.0) | =4.3.0, `vendor/redb` 사본 | 월드 저장 `data/world.redb` | 사본에서 고친 것: ① 캐시가 파일보다 크면 쓰기마다 페이지 캐시 큐가 자람(#1517, 4.3.0 미수정) → 상류 master f95d4609의 `lru_cache.rs`를 옮김. 재현: 사본 안 `lru_cache::test::queue_stays_bounded_without_eviction`(원본 실패). ② 열 때 체크섬 확인 없이 시스템 트리를 읽어 손상 바이트 하나에 패닉(#1505, #1334 계열. 상류 not planned, PR #1506 미병합) → 열 때 시스템 트리 전체 체크섬을 먼저 확인해 Corrupted 오류로, 디버그 빌드가 데이터 트리를 확인 없이 걷던 것도 먼저 확인. 재현: `xtask/tests/redb_repro.rs`(원본 release 7곳·debug 48곳 패닉, 사본 0). 1 MB 파일 무작위 1바이트 손상 2만 번 × 3상태에서 원본 열기·검사 패닉 26/14/25 → 사본 0. 남김: 슬롯 체크섬까지 맞춘 조작 파일은 최대 4 GiB 할당(#1503, 상류 거절, 무작위 손상으로는 닿지 않음), 실행 중 생긴 페이지 손상의 읽기 패닉(#1334, 시작 검사 뒤의 하드웨어 손상만, 저장 스레드 패닉 → 4절 6번 종료, 다음 시작에서 거부), #1507 HDD 탐색 비용(성능만). 숨은 스레드·시계·난수 없음, 기본 캐시 1 GiB → 64 MiB, 열린 RustSec 권고 없음. 열기·만들기, `begin_read`, `begin_write`, `check_integrity`, `compact`, `commit`은 `.clippy.toml` 금지(임시 코드로 13개 걸림 확인), `storage`만 씀. `check_integrity`는 모든 페이지 체크섬을 보고 할당 상태를 다시 만들어 비교하며, 다르면 고쳐서 파일에 커밋하고 Ok(false), 못 고치면 Err(Corrupted). `compact`는 2단계 커밋을 되풀이해 파일을 줄이고 중간 크래시 뒤에는 다음 열기가 복구한다. Windows는 열린 파일의 바이트 범위를 잠가 다른 핸들의 읽기가 os error 33으로 실패한다(실측) M9: verify가 사본 시험을 `--tests`로 돈다. 고침 ②와 부딪치는 상류 기대 1곳(`check_integrity_failure`: 디버그는 열 때 거부, release는 상류 경로)을 바로잡았다. 저장 형식 4가 `ReadableTable::range`, `Table::retain_in`도 쓴다(상류 시험 `retain_in_*` 있음). M9 후속 R2: 실행 중 백업의 단계 커밋은 `Durability::None`(뒤따르는 지속 커밋 전에는 머리가 디스크에 닿지 않음 → `finish`가 지속 커밋 한 번의 오류를 받은 뒤에만 rename, drop의 닫기 커밋은 오류를 버리므로 기대지 않음; 파일이 커질 때는 redb가 길이를 동기화). |
| go-nethernet(봇만) | gophertunnel go.mod 판 | 봇 NetherNet 다이얼 | 개발 도구. 후보 줄을 `candidate:candidate:`로 두 번 접두, 채널 열림 확인 전에 보냄(Anchor가 둘 다 받음) |
| go-jose, go-oidc(봇만) | gophertunnel go.mod 판 | 봇 토큰 발급과 봇 시험 | 개발 도구 |
| pmmp/BedrockBlockUpgradeSchema(CC0) | 5.3.0 `4e874a16`, `.cache/block-upgrade-schema` → `xtask/data/upgrade/`(파일마다 sha256) | 가져오기의 옛 블록 상태 업그레이드(M9) | (a) `0351`이 저장소 루트에 있어 PMMP 스캔에서 빠짐 → 따로 읽음, (b) #22: 0321부터 출력 버전이 같아 PMMP가 현재 상태 `red_mushroom_block` 10/15를 `mushroom_stem`으로 바꿈 → 같은 버전 묶음의 첫 스키마는 이미 적용된 것으로 건너뜀, gen이 기준 상태 22,091개가 그대로인지 확인, (c) 이름 바꾸기와 평탄화가 겹치면 PHP가 던짐 → gen이 거부, 순서에 따라 결과가 달라지는 속성 이름 바꾸기·127 넘는 byte 값도 gen 거부, (f) 모르는 legacy 값을 meta 0으로 바꿈 → 원본 `{id, val}`을 알 수 없는 블록으로 보존. 골든은 PMMP 업그레이더를 PM5 PHP로 실행(`xtask/php/upgrade.php`: legacy 4,096, 상태 15,560) |
| pmmp/BedrockItemUpgradeSchema(CC0) | 1.18.0 `7f551f62`(PMMP 고정 1.17.0보다 새 판, `photo`→`photo_item`), `.cache/item-upgrade-schema` | 가져오기의 아이템 이름 업그레이드(M9) | 대문자 ID는 PHP가 영원히 못 찾음 → gen이 검사, CRLF → LF |
| df-mc/goleveldb(BSD-2, 개발 도구) | v1.1.9(xtask/go) | 가져오기 리더 대조(`xtask/go/leveldbdump`) | 압축 2(zlib 헤더) 없음, 열 때 폴더에 씀 → 사본을 열어 읽음. 의존하는 klauspost/compress v1.18.1이 철회 표시(issue 1114, gophertunnel 고정이라 바꾸지 않음, 픽스처 대조 일치) |
| Mojang LevelDB·Bedrock 월드 형식 | google/leveldb 문서, BDS 1.26.52 출력(M9 실측), PMMP `LevelDB.php`, dragonfly `mcdb` | 가져오기 | MANIFEST 태그 5(압축 포인터)를 BDS가 씀(설계 목록에 없었음) → 받음. PMMP는 남는 바이트를 로그만 남기고 읽음·사라진 엔티티 처리 없음 → 오류로 고침. 끝나지 않은 로그 쓰기는 LevelDB처럼 버림 |
| memory-stats(MIT/Apache-2.0) | =1.2.0, `always_use_statm` | `/status` 상주 메모리(Windows `GetProcessMemoryInfo`, Linux `/proc/self/statm`; Anchor는 `unsafe`를 쓰지 않아 운영체제 호출을 이 크레이트에 맡김) | 결함: 기본 Linux 경로가 `/proc/self/smaps` 전체를 읽음 → `always_use_statm`, 파일 읽기 → 블로킹 스레드에서만(`.clippy.toml` 금지), windows-sys 0.52 고정 → 중복 허용(`.clippy.toml` 이유), RustSec 권고 없음(2026-10-05). Linux 경로는 Linux 검증 대기 |
| golang.org/x/tools(BSD-3) | v0.42.0(xtask/go) | 추출기의 타입 정보 로드 | 개발 도구, gophertunnel 의존성 버전을 바꾸지 않는 판을 골랐다 |

### 9.2 추천 후보 (2026-10-01 조사, 강제 아님)
| 용도 | 추천 | 가드 |
|---|---|---|
| 저장 | redb(사용자 지정) | 4.4.0 이상(쓰기마다 메모리 증가 #1517 수정), 없으면 수정 커밋(M6: 4.3.0에 수정 커밋을 옮긴 사본, 9.1). 손상 파일 결함(#1503, #1505)과 읽기 체크섬 없음(#1334)은 6절 규칙으로 막는다 |
| 비동기 런타임 | tokio 1.x | 멀티스레드 런타임 하나, 상한 없는 채널 금지 |
| CPU 풀 | rayon | 고정 크기 풀 하나 |
| 암호 | aws-lc-rs | Windows는 NASM 필요. 같은 키의 서명과 ECDH는 PKCS#8로 잇는다(M2 확인) |
| WebRTC | str0m(aws-lc-rs 기능) | 정확한 버전 고정, sctp-proto의 검사 없는 `+1`은 격리나 수정 커밋으로. rtc 크레이트는 피한다(FORWARD-TSN CPU 고갈) |
| zlib | 출력 libdeflater, 입력 miniz_oxide | 상한 있는 해제 API만 |
| Snappy | 직접 구현 | snap 크레이트는 외부 입력 경로에 unsafe가 있어 쓰지 않는다 |
| 직렬화, 설정 | serde, serde_json, toml | 신뢰 못 할 JSON은 길이 상한 뒤 타입 있는 구조체로 |
| 시그널링 HTTP | 직접 구현 + tokio | httparse, hyper는 쓰지 않는다 |
| HTTPS | tokio-rustls, ureq 3 + rustls(aws-lc-rs) | ring을 켜지 않게 기능 지정 |
| 로그, 수학 | tracing, libm | |
| 직접 구현 | RakNet, 오류 타입, CRC, base64, UUID | |

## 10. 마일스톤
| 단계 | 내용 | 통과 기준 |
|---|---|---|
| M0 기반 | 저장소 파일, 툴체인, `cargo xtask verify` | verify PASS, 게이트 자체 시험 통과 |
| M1 프로토콜 | Go 추출기, IR, 생성기, 손 코덱, 배치 압축(zlib, Snappy) | 모든 패킷 생성, P1~P4 통과 |
| M2 RakNet과 틱 뼈대 | 설정 파일(모든 키 검증), RakNet, 세션, 로그인, 인증 캐시, 암호화, 틱 뼈대, 빈 월드 스폰 | P5(RakNet), 설정 시험, 시험용 발급자 로그인, P6(스폰, 큰 스킨, 블록 ID) |
| M3 NetherNet | 시그널링, 신원 키, WebRTC, 데이터 채널, 신원 결합, 두 전송 동시 수신 | P5(NetherNet), P6(NetherNet 스폰, `a=identity` 위치, RakNet 폴백) |
| M4 데이터 | PMMP 덤프, 블록·아이템·레시피 생성, 기준 목록, 해시 골든, 상태 대조 | 골든 통과, 모든 상태가 기준 목록에 있음, 알 수 없는 입력에서 패닉 0 |
| M5 틱과 결정성 | 4단계 틱, 병렬 단계, 입력 로그와 재생 | Windows에서 스레드 1과 N의 재생 해시 동일 |
| M6 월드와 저장 | 영구·임시 월드, 청크 전송, 블록 변경, redb 커밋, 손상 정책, 백업 | 강제 종료 반복과 장애 주입에서 복제 0, 손상 0 |
| M7 게임 기능 | 8.4 범위, 조명, 랜덤 틱, 주민 거래 | 자동 시나리오와 멱등 시험 통과, P6(제작, 화로, 인챈트, 작물, 거래) |
| M8 API | 이벤트, 명령, 폼, 확장 지점, PMMP 대응표, 예제 4개 | 예제가 Anchor를 고치지 않고 동작, 대응표 빈칸 0 |
| M9 보강 | 전체 재검토, 운영 기능(콘솔, 백업, 정상 종료) | 체크리스트 지적 0, 검증 필요 0, 막힘 0, P6 전체 통과 |
- 각 마일스톤을 시작할 때 세부 설계와 판정식을 이 파일에 먼저 적는다. 개발 순서는 RakNet이 먼저지만 실행 우선순위는 NetherNet이 먼저다.

### 10.1 M1 세부 설계 (2026-10-02)
- 출처 고정: gophertunnel master `80c811b`(v1.62.0 뒤 19커밋, 프로토콜 2193, 1.26.50 그대로). 태그 뒤 형식 수정은 ClientBoundDebugRenderer(98c3d1f)이고, 그 밖에 NBT 고정 길이 배열 검사(398154f), 문자열 `ReadFull`(a705cae)이 있다. 같은 프로토콜이라 4절 규칙대로 커밋으로 고정한다. Go 쪽은 의사 버전으로 `xtask/go/go.mod`에 고정한다.
- 파일:
  - `xtask/go/`: Go 모듈 하나. `extract/`(IR 추출기), `oracle/`(시험 도구: `corpus`, `check`, `compress`, `decompress` 명령).
  - `xtask/data/protocol.json`: IR. 커밋한다.
  - `xtask/src/gen.rs`(+`gen/`): 생성기. 출력은 `src/protocol/types.rs`(gophertunnel `protocol` 패키지 타입)와 `src/protocol/packet.rs`(패킷과 `Packet` enum).
  - 손 코드: `src/nbt.rs`(NBT 값과 네트워크 LE 형식, 저장에서도 씀), `src/protocol/io.rs`(Reader, Writer, Error, 상한 도우미), `src/protocol/` 아래 손 코덱 파일들.
  - 배치: `src/protocol/batch.rs`, `src/protocol/compression.rs`(+`compression/zlib.rs`, `compression/snappy.rs`).
  - 시험: `xtask/tests/protocol.rs`(P1~P4), `xtask/tests/compression.rs`. 코퍼스는 시험 때 `target/`에 만들고 커밋하지 않는다.
- 명령: `cargo xtask gen`은 Go 추출 → IR → Rust 생성을 한다. `cargo xtask verify`는 맨 앞에 `gen --check`(다시 만들어 커밋된 파일과 바이트 비교)를 돈다. Go는 `PATH`의 `go`를 쓰고 없으면 실패한다.
- IR(JSON): `source`(모듈, 커밋, 프로토콜, 게임 버전), `packets`(ID, 이름, 방향: gophertunnel `pool.go`의 클라이언트·서버 풀), `types`(필드 이름, Go 타입, 문서, Marshal 문장 목록), `functions`(IO를 받는 자유 함수), `unions`(`lookup*` 함수의 ID→타입 표), `enums`(문자열↔값 변환 함수 표), `constants`. 문장은 IO 호출, 중첩 Marshal, 목록(길이 형식 포함), Optional, if, switch, 지역 변수, 대입, 오류(`InvalidValue`, `UnknownEnumOption`)다. 이 밖의 구문은 추출 실패다.
- 손 코덱 목록 = `protocol.IO`의 원시형이 아닌 메서드(벡터·위치, UUID, 색, NBT, EntityMetadata, ItemStack 계열, StackRequestAction 등 union 틀, GameRule, AbilityValue, Bitset, PackSetting, ShapeData, MaterialReducer, 이벤트 틀, 거래 데이터 틀) + Login 요청 봉투 + 서브청크 페이로드. union의 변형 구조체와 ID 표는 생성하고, 태그를 읽고 쓰는 틀만 손으로 쓴다.
- Rust 표현(결정):
  - Go 구조체는 같은 모양의 평평한 struct다(조건부 필드도 늘 있고 없으면 `Default`). 이유: gophertunnel과 1:1이라 P3 판정과 버전업 비교가 단순하다. 더 나은 공개 모양은 M8 API가 감싼다. 모두 `Debug, Clone, PartialEq, Default`를 구현한다.
  - `Optional` → `Option`, 슬라이스 → `Vec`, 인터페이스 → enum, `mgl32.Vec3` → `[f32; 3]`, `uuid.UUID` → `Uuid`, NBT 컴파운드 → `BTreeMap<String, nbt::Value>`(바이트 순 정렬로 쓴다. 같은 키는 뒤가 이긴다: Go map과 같음), EntityMetadata → `BTreeMap<u32, _>`(gophertunnel도 키를 정렬해 쓴다).
  - 문자열은 `String`이고 UTF-8이 아니면 디코드 오류다(gophertunnel은 검사 안 함). 공식 클라이언트 출력은 관찰할 수 없어 스폰 뒤 디코드 실패 패킷만 버리는 설계로 닫았다(10.2).
  - bool은 0과 1만 받는다. 쓰기는 길이를 자르지 않고 `Error`를 낸다(인코딩도 `Result`).
  - 공개 범위(결정): `anchor::protocol`은 공개 코덱 층이다(패킷과 타입, `Packet` 디코드·인코드, 배치 인코드·디코드, `Compression`). 패킷 훅(8.3)이 패킷 값을 다루고, P1~P4 시험이 xtask에서 부르기 때문이다. `Reader`, `Writer`, `Codec`은 `pub(crate)`다. NBT는 저장에서도 써서 `anchor::nbt`다.
  - 이름은 5.1을 따른다. Rust 키워드인 필드는 raw identifier(`r#type`)다. 문서는 생성기가 IR의 자기 정보(이름, 패킷 ID·방향, 와이어 형식, 상한, 대응 enum)로 만든다(M9 저작권 결정: gophertunnel 주석을 옮기지 않으며, 추출기는 주석을 읽지 않는다).
- 상한(디코드는 하나의 상한 도우미를 거친다):
  - 선언 길이는 남은 바이트 이하. 목록 개수는 1024 이하(gophertunnel `maxSliceLength`, 모든 목록에 같은 값. EntityMetadata 개수 포함).
  - 할당 예산: Reader는 배치 하나마다 예산(2 × `max-batch-bytes`)을 받고, 모든 `Vec`, `String`, NBT 할당은 `개수 × size_of`를 먼저 뺀다. 넘으면 크기 오류. 이유: 개수 상한만으로는 중첩 목록과 큰 원소가 입력 크기의 수십 배를 할당할 수 있다.
  - NBT 깊이 512, NBT 하나 4 MiB. 배치당 패킷 812개.
  - 필드별 상한(Mojang `maxItems`, `maxLength`)은 클라이언트 패킷을 처리하기 시작하는 M2에서 더한다(공통 상한과 예산이 M1의 안전성을 지킨다). → M9(R1-K2)에서 구현: Mojang 스키마의 상한 128개를 `cargo xtask dump`가 gophertunnel 필드에 이어(`xtask/src/dump/limits.rs`: 적용 117행, 안 씀 11행) `xtask/data/reference/protocol_limits.json`(필드 113개)에 쓰고 gen이 디코더에 넣는다. 디코드에만, 목록은 min(필드 상한, 1024), 문자열은 바이트. 스키마 상한이 표에 없거나 표 행이 스키마에 없으면 dump 실패, 어떤 디코더에도 안 쓰이는 상한은 gen 실패. 코퍼스는 상한 안 표본만(oracle `-limits`).
- Go 시험 도구(oracle):
  - `corpus`: 무작위 값을 고르며 쓰는 `protocol.IO` 구현으로 각 패킷의 Marshal을 돌려 표본을 만든다(시드 고정 PCG, 목록 길이 0~4). 평범한 gophertunnel Writer로 쓰고, 패킷 최상위 NBT만 자체 정렬 인코더(`nbt.go`)로 쓴다. gophertunnel이 안에서 쓰는 NBT(아이템 user data, metadata compound)는 compound마다 키 1개 이하라 바이트가 하나로 정해진다. 중첩 다중 키 NBT는 `nested_compounds_with_several_keys_agree_with_gophertunnel`이 Rust R → Go → Rust 값 비교와 R 재인코드로 확인한다(gophertunnel writer를 옮긴 canonWriter는 저작권 규칙으로 지움, M9). gophertunnel Reader로 다시 읽어 성공한 표본만 남긴다.
  - `check`: 받은 바이트를 gophertunnel로 디코드하고 다시 인코딩해 돌려준다(Go map 순서가 무작위라 바이트 비교는 Rust가 한다).
  - `compress`, `decompress`: gophertunnel `packet` 패키지의 zlib과 Snappy로 압축과 해제를 한다.
  - shield ID는 양쪽 모두 고정값을 쓴다.
- gophertunnel 결함과 우리 조치(M1 조사):
  - MaterialReducer: Reader가 `mix << 16`으로 읽는다(`>> 16`이 맞음). Anchor는 `>> 16`, `& 0xffff`로 읽고 범위를 넘는 쓰기는 오류다. 코퍼스는 mix 0~0x7fff만 쓴다.
  - Event 타입 8, 9, 10, 12, 14, 20, 21, 22는 ordinal이 없어 읽기도 쓰기도 안 된다. ordinal 21을 세 이벤트가 나눠 쓴다. 검증 필요(M7): BDS 값(P6).
  - 쓸 때 덮어쓰거나 읽고 버리는 값: ItemDescriptorCount aux(늘 32767), StackRequestAction의 legacy 바이트, Skin `trusted` 문자열, ResourcePackClientResponse·PlayerUpdateEntityOverrides·ScoreboardEntry의 이름 문자열(번호와 대조 안 함), InventoryTransaction `hasLegacy`. ScoreboardEntry는 variant ≥ 4에서 오류 없이 멈춘다. Anchor 생성 코드는 gophertunnel과 같게 두고 M2에서 클라이언트 패킷부터 검사를 더한다.
  - EntityMetadata 개수 상한 없음, NBT 깊이 계수 누수, 부분 읽기 허용, ItemInstance·Login 길이 조용히 자름 → Anchor는 상한과 오류로 고쳤다.
  - Snappy 해제는 `s2.Decode`라 S2 확장도 받는다. Anchor는 표준 Snappy만 받는다.
  - HeightMap 행마다 varuint32 16 접두. 검증 필요(M6): BDS 대조(P6).
- 손 코덱 결정: 서브청크 해제는 엄격하다(비트 수 {0,1,2,3,4,5,6,8,16}만, 지속 팔레트 플래그와 0x7f 거부, 팔레트 길이 ≤ 2^bits, 모든 인덱스 < 팔레트 길이). 검증 필요(M6): BDS가 보내는 형식(P6). NBT 깊이 512 값의 drop은 재귀라 디버그에서 스택 약 1 MiB를 쓴다(시험으로 고정). M2에서 네트워크 워커와 틱 스레드 스택을 4 MiB로 정했다(10.2).
- NBT 목록(결정): `nbt::List`가 원소 타입을 늘 지닌다(빈 목록도). gophertunnel은 빈 Int·Long 목록만 타입을 지키고 나머지는 End로 다시 쓴다(정보 손실). P3(나)는 이 차이 하나만 값 구조 비교로 허용한다.
- 배치 패킷 목록(결정): 헤더는 ID 0~9비트, 보낸 쪽 10~11, 받는 쪽 12~13. 그보다 높은 비트는 읽을 때 무시한다(M2에서 바꿈, 10.2). 출력은 256 KiB 전과 812개에서 끊는다.
- zlib 해제는 마지막 블록 뒤 남는 바이트를 받고 버린다(M2에서 바꿈, 10.2).

#### M1 판정식
- 생성: 추출 실패 0. IR 패킷 수 = gophertunnel 두 풀의 합집합 크기 = Rust `Packet` variant 수. IR의 모든 타입이 생성된다.
- P1: 모든 패킷마다 코퍼스 표본 64개, 모든 표본에서 Rust 디코드가 성공하고 남는 바이트가 0이다.
- P2: 모든 코퍼스 표본 b에서 `encode(decode(b)) == b`. P4에서 디코드에 성공한 모든 변형 입력 m에서 `e = encode(decode(m))`이면 `encode(decode(e)) == e`(바이트 비교라 NaN에도 맞다).
- P3: (가) Go가 만든 바이트 → Rust 재인코딩이 같다(P2 표본). (나) P4에서 디코드에 성공한 변형의 Rust 인코딩 R → Go 디코드 성공 → Go 재인코딩 G → Rust 디코드 → Rust 재인코딩 == R. 배치: zlib과 Snappy 각각 Rust 압축 → Go 해제 == 원문, Go 압축 → Rust 해제 == 원문(크기 0, 1, 255, 256, 64 KiB, 1 MiB, 무작위와 반복 데이터).
- P4: 패킷마다 고정 시드 변형 1000개(비트 뒤집기, 바이트 넣기·빼기, 자르기, 길이 필드 키우기)에서 패닉 0. 상한마다(길이, 개수, 예산, NBT 깊이·크기, 해제 크기, 배치 패킷 수) 넘는 선언이 크기 오류로 끝나는 시험이 있다. Snappy 결함 입력 5종(3절)과 zlib 폭탄은 오류다.
- `cargo xtask verify` PASS(`gen --check` 포함).

### 10.2 M2 세부 설계 (2026-10-03)
- 검증 필요 처리(시작 때):
  - RakNet 버전과 MTU(2.2): 고정한 go-raknet은 프로토콜 11, 서버 쪽 MTU 하한 400·상한 1492, 다이얼러는 1492, 1200, 576 순으로 탐색한다. Anchor는 11만 받고(다르면 IncompatibleProtocolVersion), MTU는 min(요청 크기, `max-mtu-size`)이다. 하한은 구현 때 go-raknet 서버 쪽과 맞춰 아래 상한 목록에 적는다.
  - BDS 프로토콜 번호(4절), 해시 블록 ID(7절), data-driven 블록 정의(7절), BDS zlib 출력의 끝 바이트와 배치 헤더 높은 비트·812개 초과(10.1): 이번 P6에서 확인해 결과를 아래 "P6 결과"에 적는다.
  - 공식 클라이언트 출력에서만 보이는 것(비UTF-8 문자열, 클라이언트 zlib 끝 바이트, 헤더 13비트 위)은 CLAUDE.md 8절대로 모든 경우를 받는 설계로 닫는다. (1) 스폰 뒤 클라이언트 패킷 하나의 디코드 실패는 그 패킷만 버리고 warn으로 남긴다(로그인 순서의 패킷은 끊는다). 버린 패킷도 입력 예산에서 뺀다. (2) 배치 헤더 13비트 위는 읽을 때 무시한다(gophertunnel과 같음, 10.1 결정 변경). (3) zlib 마지막 블록 뒤 바이트는 받고 버린다(10.1 결정 변경, 압축 입력 길이는 이미 재조립 상한 안).
  - aws-lc-rs로 같은 키를 서명과 ECDH에 쓰는 변환(9.2): 구현 때 시험으로 확인한다.
  - NBT 깊이 512 drop의 스택(10.1): 네트워크 워커 스택을 4 MiB로 정하고 디버그 빌드 시험으로 고정한다.
- 모듈(모두 `pub(crate)`, 공개는 `Config` 계열, `Server` 계열, `Error`와 M1의 `protocol`, `nbt`):
  - `config.rs`: 8.5 그대로. `Config { general, developer }`와 값 enum. 검증은 `load`와 `build()`가 같은 함수로 한다.
  - `error.rs`(크레이트 `Error`, `Result`), `server.rs`(`Server`, `ServerBuilder`, `ShutdownHandle`), `runtime.rs`(스레드와 런타임을 만드는 유일한 모듈), `clock.rs`(엔진 시계, 시계 진입점을 부르는 유일한 곳), `tick.rs`(+`tick/`: 틱 루프, `step`, 상태).
  - `raknet.rs`(+`raknet/`): 서버 쪽 RakNet. sans-IO 핵심(메시지 형식, 신뢰성, 조각, ACK/NAK, 혼잡 제어)과 tokio UDP 구동부. 0xfe는 이 층이 떼고 붙인다(세션은 전송과 무관).
  - `session.rs`(+`session/`): 연결 상태 기계(배치, 암호화, 로그인 순서, 예산)와 틱 큐 연결. M3 NetherNet도 같은 세션을 쓴다.
  - `login.rs`(+`login/`): 연결 요청 검증. `auth.rs`: 인증 환경 받기와 `data/auth.json` 캐시. `crypto.rs`(+`crypto/`): aws-lc-rs 감싸기(ES384, RS256, ECDH P-384, SHA-256, AES-256-CTR, 난수), base64, JWS 압축 형식, RakNet 암호화.
- 공개 API(M2): `Server::builder().config(cfg).data_dir(path).build()?.run()`. `data_dir` 기본은 `data`(같은 프로세스에서 서버 여럿을 띄우는 시험과 사용자에게 필요). `Server::shutdown_handle()`과 Ctrl+C로 정상 종료한다. 콘솔 입력은 M9.
- 스레드와 큐:
  - `run()`은 tokio 멀티스레드 런타임 하나(워커 = `network-threads`, 스택 4 MiB, 블로킹 풀 상한 4)와 틱 스레드 하나를 만든다. CPU 풀은 M5에서 만든다(M2는 값 검증만). auto는 network = max(1, L/4), cpu = max(1, max(L,4) − 2 − network)이다(L = 논리 CPU, 2 = 틱과 저장). 직접 정한 값은 network + cpu + 2 ≤ max(L,4)여야 하고, 아니면 시작을 거부한다.
  - 큐는 tokio 상한 `mpsc` 하나다. 틱은 `try_recv`, `try_send`만 쓴다. 불변 스냅샷 방송(인증 키, 핑 응답)은 `tokio::sync::watch`다(큐가 아니라 최신 값 하나).
  - RakNet 소켓 작업 하나가 두 소켓(IPv4, IPv6)과 모든 RakNet 연결 상태를 가진다(잠금 없음). 연결마다 세션 작업 하나가 해제, 복호화, 디코드, 로그인 검증을 한다(작업끼리 병렬).
  - 틱 입력 큐 65,536개, 세션 출력 큐 1,024개. 가득 차면 그 세션을 끊는다(틱은 기다리지 않는다).
- 연결 상한(상수, 설정 아님):
  - 로그인 전 연결 전체 64, IP별 4. 넘으면 RakNet 0x14. 로그인 뒤는 `max-players`(넘으면 PlayStatus 서버 가득 참).
  - 핑: IP별 토큰 버킷(초당 5, 순간 10). IP 표는 4,096개이고, 1분 쓰지 않은 항목은 지우며, 가득 차면 새 IP의 핑에 답하지 않는다.
  - 마감: 연결부터 Login 검증까지 10초, 스폰 완료(SetLocalPlayerAsInitialised)까지 60초(리소스팩 전송은 M8에서 다시 본다). RakNet 무응답 시간은 go-raknet 값을 따른다.
  - 입력: 세션마다 해제 바이트 토큰 버킷(용량 `max-batch-bytes`, 초당 `max-batch-bytes`/4). 인증 전 재조립·해제 합은 8 × `max-batch-bytes`(3절). RakNet 메시지 하나는 3절 재조립 식 + 9바이트(0xfe, 체크섬 8)다.
  - 출력: 세션마다 보내지 못한 바이트가 32 MiB를 넘으면 끊는다.
  - RakNet 내부 상한(조각 수, 동시 조각 묶음, 순서 대기, 재전송 창, ACK 범위 수)은 구현 때 정해 아래에 적는다.
- 세션 순서(서버 쪽): RequestNetworkSettings → NetworkSettings → Login 검증 → (RakNet) ServerToClientHandshake, ClientToServerHandshake → PlayStatus(LoginSuccess), ResourcePacksInfo → 응답(HaveAllPacks) → ResourcePackStack → 응답(Completed) → 틱 Join → StartGame 계열 → RequestChunkRadius → ChunkRadiusUpdated, 청크, PlayStatus(PlayerSpawn) → SetLocalPlayerAsInitialised. 로그인 순서의 패킷(RequestNetworkSettings, Login, ClientToServerHandshake, ResourcePackClientResponse)이 순서를 어기면 끊는다. 그 밖의 패킷은 Join 전에는 대기 큐(32개, 넘치면 끊음)에 두었다가 Join 뒤 순서대로 틱에 넘긴다(gophertunnel도 미뤄 두고, 공식 클라이언트 순서는 관찰할 수 없다). ClientCacheStatus는 받고 버린다(블롭 캐시는 M6). 스폰 때 보내는 패킷 목록과 순서는 P6에서 BDS를 따르고, 아직 채우지 못하는 패킷(아이템, 제작, 생물군계, 명령 등)은 "P6 유예표"에 마일스톤과 함께 적는다.
- 로그인 검증(3절 보충):
  - 온라인: `Token`만 본다(레거시 체인은 무시). RS256만, `kid` 필수, `exp` 필수, `nbf`와 `iat`는 있으면 검사, 시계 오차 60초(go-jose 기본과 같음). `iss`는 OpenID 설정의 `issuer`와 문자열이 같아야 하고 `aud`에 `api://auth-minecraft-services/multiplayer`가 있어야 한다. `xid`는 숫자, `xname`은 비어 있지 않고, `cpk`는 P-384 SPKI(base64)다.
  - 오프라인(`xbox-auth = false`): `Token`이 있으면 서명 없이 클레임만 읽고, 없으면 레거시 체인(첫 토큰 자체 서명 `x5u`)을 읽는다. 이름 규칙은 PMMP다(10.3, M3에서 바꿈). XUID는 비운다.
  - ClientData: ES384, `cpk`로 검증한다. 내용 규칙은 gophertunnel `ClientData.Validate`다.
  - 시각: JWT 시각 검사는 틱 밖이라 벽시계를 쓰고, `clock.rs`가 준다.
- 인증 캐시 `data/auth.json`: 디스커버리의 `auth.prod`, OpenID 설정, JWKS, 받은 시각을 담는다. 시작 때 캐시가 1시간 안이면 그대로 쓰고, 아니면 받는다(실패하면 캐시, 둘 다 없으면 시작 거부). 실행 중 1시간마다 갱신하고, 모르는 `kid`는 5분에 한 번 JWKS만 다시 받는다. 캐시가 손상(JSON 아님, 필드 빠짐)이면 시작을 거부하고 파일을 남긴다. 쓰기는 임시 파일 + rename이다. 응답 본문 1 MiB, 요청 10초 상한.
  - 시험용 발급자: 시험이 자체 RSA 키로 OpenID 설정과 JWKS를 만들어 `auth.json`(받은 시각 = 지금)에 넣고 토큰을 발급한다. 엔진에 시험 전용 진입점은 없다.
- 암호화: 서버 키는 실행마다 새 P-384 키 하나(서명과 ECDH 같은 키), salt는 연결마다 16바이트 난수. 형식은 3절.
- 틱 뼈대: 20 TPS, 엔진 시각은 틱 번호. 세션 ID는 엔진이 연결 순서로 주는 u64(재사용 안 함). 입력은 (세션 ID, 세션 안 순번)으로 정렬한다. M2 상태는 플레이어 표(세션 → 엔티티 ID, 이름, 위치)와 상태 해시 함수다(멱등, 결정성 시험용).
- 시험 도구:
  - Go 봇 `xtask/go/bot`: gophertunnel `packet`, `login`과 go-raknet으로 만든 클라이언트다. Dialer는 시험용 토큰을 넣을 수 없어 쓰지 않는다. 시나리오는 spawn, big-skin(해제 6 MiB Login), online(시험용 발급자 키)이고(기본 스킨 JSON은 고정한 gophertunnel 버전의 모듈 캐시에서 실행 때 읽음, 사본 없음), 받은 패킷을 순서대로 JSON 줄로 기록한다.
  - P5: `xtask/tests/`가 Anchor를 같은 프로세스에서 띄우고 봇을 돌린다(verify에 포함, Go 필요).
  - P6: `cargo xtask p6`가 `.cache/bds/1.26.52.3`(원본의 복사본, `.cache`는 .gitignore)에서 월드 폴더를 지우고 시험 설정(`online-mode=false`, RakNet, Anchor와 같은 압축 설정)으로 BDS를 띄워 봇을 돌리고, 같은 시나리오를 Anchor에 돌려 비교한다. BDS가 필요해 verify에는 넣지 않고 결과를 13절에 적는다.

#### M2 판정식
- 설정: (가) 기본 파일이 8.5 정본 블록과 바이트 동일(시험이 PLAN.md에서 블록을 읽어 비교). (나) 키마다 빠지면 기본값, 타입 오류 거부, 범위 경계의 안은 받고 밖은 거부, 오류 메시지에 키 이름. (다) 모르는 키와 표, hardcore인데 hard 아님은 거부. (라) 파일이 없으면 만들고, 다시 읽으면 같은 `Config`. `config()`를 안 부르면 파일을 읽지도 쓰지도 않는다.
- RakNet: (가) 손실, 중복, 순서 바꿈 주입(고정 시드)에서 받은 메시지 열 = 보낸 열. (나) 상한마다 넘는 입력이 오류나 끊김으로 끝나는 시험. (다) 변형 데이터그램 1만 개(고정 시드)에서 패닉 0. (라) go-raknet 다이얼러와 연결, 핑, 최대 크기 메시지 왕복(P5 안에서).
- 로그인: 시험용 발급자 토큰이 통과하고, 서명, `kid`, `alg`(none, HS256, ES384), `iss`, `aud`, `exp`, `nbf`, `cpk` 형식, ClientData 서명이 하나씩 틀린 경우가 모두 거부된다. gophertunnel이 만든 오프라인 요청이 통과한다.
- 암호화: gophertunnel이 만든 골든(키 유도, 암호문, 체크섬)과 바이트 동일. 1비트 변조와 카운터 어긋남은 오류. 같은 키로 서명한 핸드셰이크 JWT를 gophertunnel이 받는다(P5).
- 틱: (가) 같은 입력을 세션 사이 도착 순서만 바꿔 넣어도 상태 해시가 같다. (나) 같은 요청을 두 번 넣은 상태 해시 = 한 번(Join, Leave, SetLocalPlayerAsInitialised). (다) 접속과 종료 100번 반복 뒤 세션 표, 플레이어 표, 큐 개수가 시작 값과 같다.
- P5(RakNet): 봇이 zlib과 Snappy 각각, 오프라인과 온라인(시험용 발급자) 각각 스폰한다. big-skin이 통과한다. 로그인 마감과 로그인 전 연결 상한이 동작한다.
- P6: 봇 시나리오(spawn, big-skin)가 BDS에서 먼저 통과한다. spawn에서 받은 패킷 열이 BDS와 같다(유예표 패킷과 세계마다 다른 값은 빼고 정해진 필드 비교). 블록 ID가 해시인지 BDS의 StartGame과 청크 팔레트로 확인한다.
- `cargo xtask verify` PASS.

#### M2 결과와 정한 값 (2026-10-03)
- 판정: 설정 시험(공개 14, 단위 7: 8.5 블록 바이트 동일, 42개 키 모두), RakNet(손실 10%·중복 5%·순서 바꿈에서 메시지 열 동일, 상한마다 시험, 변형 1만 개 패닉 0, 실제 go-raknet과 18.7 MiB 왕복), 로그인(시험용 발급자 통과, 틀린 경우 하나씩 거부, gophertunnel 골든 통과), 암호화(gophertunnel 골든과 바이트 동일), 틱((가)(나)(다)), P5(`xtask/tests/spawn.rs` 6개: zlib·Snappy 스폰, 6 MiB Login, 온라인 시험용 발급자, 로그인 마감 10.02초, IP별 로그인 전 4개, 접속·종료 5번 뒤 핑의 접속자 0), P6(아래) 모두 통과. `cargo xtask verify` PASS.
- 바뀐 결정:
  - 설정 파일이 없으면 기본 파일을 쓰고, 그 파일을 읽은 것처럼 적용한다(첫 시작과 그 뒤 시작의 결과가 같다).
  - RakNet 쿠키(security)는 쓰지 않는다. BDS가 security=false로 답하고 공식 클라이언트의 쿠키 처리는 관찰할 수 없어서다(CLAUDE.md 8절). 위조 출발지는 로그인 전 상한으로만 막는다.
  - Join 전 예상 밖 패킷은 끊지 않고 대기 큐에 둔다(세션 순서 항목).
  - `cargo xtask verify`는 검사를 `target/verify`에서 돌린다(`cargo test --workspace`의 기능 통합이 달라 실행 중인 xtask.exe를 다시 빌드하려다 Windows에서 실패해서). 비공개 문서 검사만 `target/verify-private-docs`에서 돈다: 두 문서 빌드가 같은 파일을 써서 한 폴더에서는 바뀐 것이 없어도 매번 서로를 다시 빌드했다(따뜻한 상태 두 검사 합계 약 23~30초 → 약 1초).
- 설정 범위(정본은 `src/config.rs`): `max-players` 1~10000, `chunks-per-tick` 1~256, `spawn-radius`·`tick-radius` 1~64, `random-tick-speed` 0~4096, 스레드 `"auto"` 또는 1~65535(합계 규칙은 `build()`), `motd` 1~256바이트·`level-name` 1~64바이트(`;`와 제어 문자 금지: 핑 응답이 `;`로 나뉜다), `shutdown-message` 1024바이트 이하, `disable-block-ticking`은 `ns:path` 모양. 압축 세 키는 `batch::Settings::new` 한 곳에서 검사한다. toml 파서는 깊이 80에서 멈춘다(1 MiB 깊은 중첩 시험).
- RakNet 상수(정본은 `src/raknet/`): 조각 수 ⌈메시지/345⌉(미리 할당 안 함), 동시 조각 묶음 16, 순서 창 2048(채널 32개 따로), 쥔 바이트 2 × 메시지 + 2048 × 1492, 데이터그램 기록 1024, 신뢰 인덱스 빈칸 8192, 혼잡 창 32(최소 4, 최대 2048, 손실 때 ×3/4), RTO 500ms(100ms~2s), flush당 데이터그램 32, 출력 32 MiB, 세션 입력 큐 1024개·2 × 메시지 바이트, 소켓 버퍼 4 MiB, 갱신 10ms, 핸드셰이크 10초, 무응답 5초 + 2·RTT, 닫기 5초, 핑 응답 1429바이트 이하. IPv6 로그인 전 상한은 /64 단위.
- 세션·틱 상수: 틱당 입력 8192개, 스택 4 MiB(워커와 틱), 블로킹 스레드 4, 종료 유예 10초, 세션 받기 큐 64, Join 전 대기 패킷 32, 첫 메시지 1 KiB. 핑 응답은 BDS 형식 `MCPE;motd;2193;1.26.50;접속자;최대;guid;level;모드;1;포트;포트;0;1;1;`.
- 로그인 상한(정본은 `src/login.rs`, `src/auth.rs`): 체인 JSON 64 KiB, 토큰 16 KiB, 체인 8개, `aud` 16개, RSA 2048~4096비트, JWKS 키 64개, HTTP 10초·본문 1 MiB 미만(DNS는 OS 상한), 캐시 1 MiB.
- P6 결과(BDS 1.26.52.3, `cargo xtask p6 bds`, `p6 anchor`):
  - BDS 프로토콜 2193(핑과 RequestNetworkSettings 수락), RakNet 11, 1492 요청에 MTU 1400으로 답함, security=false, online-mode=false에서도 암호화한다.
  - 해시 블록 ID(StartGame 플래그와 팔레트 4종: air -604749536, bedrock -173245189, dirt -2108756090, grass_block -567203660). 청크는 서브청크 요청 방식이다(M6).
  - BDS zlib 출력에 끝 바이트 없음, 헤더 13비트 위 없음, 배치당 패킷 최대 45개, 해제 배치 최대 약 9 MB(큰 스킨이 PlayerList로 되돌아옴).
  - 순서: PlayStatus(LoginSuccess)와 ResourcePacksInfo가 한 배치에 이 순서로 온다.
  - 스폰 창(유예 패킷 뺌)이 BDS와 같다: NetworkSettings, ServerToClientHandshake, PlayStatus, ResourcePacksInfo, ResourcePackStack, StartGame, ItemRegistry, ChunkRadiusUpdated, PlayStatus. 고정 필드(`xtask/data/p6/fields.txt`)도 zlib, Snappy 모두 같다. 유예표는 `xtask/data/p6/deferred.txt`(M4, M6, M7, M8).
  - StartGame은 BDS 값을 따른다: BaseGameVersion `*`, PlayerGameMode 5(월드 모드를 따름), Generator 2, ServerAuthoritativeSound, RewindHistorySize 40, MSAGamerTagsOnly, LimitedWorld 16, 빈 ServerJoinInformation. GameVersion은 비교에서 뺐다(BDS 빌드 1.26.52, Anchor는 프로토콜 출처의 1.26.50, 둘 다 2193).
- 남은 위험과 검증 필요:
  - BDS 1.26.52가 RakNet 전송을 "플레이어 접속 불가"로 기록한다. 공식 클라이언트가 RakNet으로 들어오는지는 관찰할 수 없다. 실제 접속은 M3 NetherNet이 맡는다.
  - Join 전 대기 큐 32개, ClientData의 엄격한 JSON(대소문자, null, 중복 키 거부), 핑 응답의 서바이벌 외 모드 뒤 숫자(BDS는 `Survival;1`만 봄)는 공식 클라이언트로만 확인된다. 검증 필요(M3): NetherNet P6에서 다시 본다.
  - 같은 XUID의 두 번째 접속과 오프라인 이름 규칙은 M3에서 사용자가 PMMP 방식으로 정했다(10.3).
  - toml의 `unbounded` 기능이 사용자 바이너리에서 켜지면 깊이 상한이 사라진다.

### 10.3 M3 세부 설계 (2026-10-03)
- 검증 필요 처리(시작 때, BDS 1.26.52.3을 `transport=nethernet`, `online-mode=false`로 띄워 go-nethernet 다이얼러로 확인):
  - 시그널링은 TCP `server-port` 이중 스택. `GET /v1/join` → 200, `Content-Type: application/json`, 본문 키 순서 `name, protocol, version, level, players, maxPlayers, gameType`(version은 BDS 빌드 `1.26.52`). 모든 응답에 `Connection: close`. 없는 경로 404 `text/plain` "Not Found", 숫자가 아닌 networkId 400 "Expected /v1/join/{networkId}". TLS 없이 띄운 BDS는 TLS ClientHello에 handshake_failure 경고를 보낸다.
  - 협상 실패도 HTTP 200, `Content-Type: application/sdp`, 본문은 오류 코드 숫자다(신원 없는 오퍼에 `37`).
  - 응답 SDP 순서: `v, o(- <난수> 2 IN IP4 127.0.0.1), s=-, t=0 0, a=group:BUNDLE 0, a=extmap-allow-mixed, a=msid-semantic: WMS, a=identity, m=application <UDP 포트> UDP/DTLS/SCTP webrtc-datachannel, c=IN IP4 <주소>, a=candidate(host, 인터페이스마다), a=ice-ufrag(4자), a=ice-pwd(24자), a=ice-options:trickle, a=fingerprint:sha-256 <대문자 HEX>, a=setup:active, a=mid:0, a=sctp-port:5000, a=max-message-size:262144`. `a=end-of-candidates`는 없다. 후보 모양은 `a=candidate:<foundation> 1 udp <priority> <ip> <port> typ host generation 0 network-id <n>`.
  - `a=identity`는 세션 수준(m= 앞)이고 값은 base64(STD, 패딩) JSON `{"assertion":"{\"fingerprints\":\"<분리 JWS>\",\"token\":\"<JWT>\"}","idp":{"domain":"self","protocol":"default"}}`다(키 순서 그대로). 서버 JWT 헤더 `{"alg":"ES384","x5u":"<base64 SPKI>"}`, 본문 `{"cpk":{"crv":"P-384","kty":"EC","x":..,"y":..},"exp":iat+60,"iat":..}`(cpk는 JWK 객체. Mojang 가이드와 같고 go-nethernet의 base64 문자열과 다르다). 분리 JWS 헤더 `{"alg":"ES384"}`, 서명 대상은 `{"fingerprint":[{"algorithm":"sha-256","digest":"<SDP에 쓴 그대로>"}]}`.
  - 클라이언트 쪽(go-nethernet, Mojang 가이드): 오퍼는 `a=setup:actpass`, 두 채널을 클라이언트가 연다. `ReliableDataChannel`(순서, 신뢰), `UnreliableDataChannel`(비순서, maxRetransmits 0). 조각 머리 1바이트 = 남은 조각 수, 조각 본문 최대 262,143바이트, 최대 256조각. 서버는 ICE controlled, DTLS client(active)다. go-nethernet은 후보 줄을 `candidate:candidate:...`로 두 번 접두해 보낸다(둘 다 받는다).
  - M2 남은 검증 필요(Join 전 대기 32, 엄격한 ClientData JSON, 핑 모드 뒤 숫자)는 BDS NetherNet P6에서도 공식 클라이언트 출력이 아니라 확인할 수 없다. 설계는 그대로 두고 13절에 남긴다.
- 모듈(모두 `pub(crate)`):
  - `nethernet.rs`: 구동부. UDP 소켓(IPv4, IPv6 전용, `nethernet-port`) 두 개와 모든 피어를 한 작업이 가진다(RakNet과 같은 구조, 잠금 없음. DTLS·SCTP가 한 작업에 모이는 비용은 M5에서 실측함, 10.5). 오퍼 처리 작업, 들어온 데이터그램 분배 색인(STUN USERNAME의 서버 ufrag → 피어, 성립 뒤 출발 주소 → 피어, 둘 다 `BTreeMap`), `Connection`(RakNet과 같은 `addr/recv/send/logged_in/close`에 증명된 `client_key`를 더함).
  - `nethernet/signaling.rs`: 최소 HTTP/1.1 서버(tokio TCP, 선택적 TLS). 라우팅과 상한만 하고 오퍼 내용은 모른다.
  - `nethernet/sdp.rs`: 오퍼 파서와 응답 작성기(위 BDS 모양).
  - `nethernet/identity.rs`: `data/identity.key`, 서버 JWT와 분리 JWS 서명, 오퍼 신원 검증.
  - `nethernet/peer.rs`: str0m `Rtc` 하나(소켓 없음): ICE controlled, DTLS active, SCTP client, 채널 검사, 조각 재조립과 분할(`nethernet/message.rs`).
  - `event.rs`(공개): 이벤트 우선순위와 `PlayerDuplicateLogin`. M8 이벤트 체계의 첫 조각이다.
  - 세션은 `Transport` enum(RakNet, NetherNet, 닫힌 집합)으로 두 전송을 같은 흐름에 태운다.
- 흐름: HTTP 작업이 POST를 받으면 → 오퍼 작업이 SDP를 파싱하고 신원을 검증(온라인은 JWKS, 모르는 `kid`는 세션과 같은 한 번 갱신) → 구동부에 피어 생성을 요청(로그인 전 상한 넘으면 거부) → 구동부가 ICE 자격과 DTLS 지문, 광고 후보를 돌려줌 → 오퍼 작업이 서버 신원으로 서명해 응답 작성 → HTTP 200. 두 채널이 열리면 구동부가 `Connection`을 accept 작업에 넘기고, 세션은 RakNet과 같은 순서(ServerToClientHandshake 포함)를 밟되 암호화를 켜지 않는다(BDS NetherNet P6: 핸드셰이크를 보내고 암호화하지 않음). Login 검증 뒤 Login의 클라이언트 키가 오퍼에서 증명된 `cpk`와 다르면 NotAuthenticated로 끊는다(신원 없는 오프라인 오퍼는 결합 없음).
- 광고 주소: `public-ip`가 있으면 그것, 없으면 시그널링 TCP 연결의 로컬 주소(클라이언트가 실제로 닿은 주소. 표준 라이브러리에 인터페이스 나열이 없어 이 방식을 쓴다. IPv4 매핑 IPv6는 IPv4로). 포트는 `nethernet-port`. 그 주소 체계의 UDP 소켓이 없으면 코드 11로 거부한다. 후보 우선순위 2122260223(BDS 값).
- 신원(identity.rs):
  - 키 파일 `data/identity.key`는 P-384 PKCS#8 DER(4 KiB 이하만 읽음). 없으면 만들어 임시 파일에 쓰고 동기화한 뒤 rename. 있는데 읽거나 파싱하지 못하면 시작을 거부하고 파일을 건드리지 않는다. 시작 때 한 번 읽는다(블로킹 풀).
  - 오퍼 검증: 토큰은 온라인이면 Login `Token`과 같은 규칙(3절, 10.2)으로 검증하고, 오프라인이면 서명 없이 `cpk`와 `exp`(있으면)만 본다. `cpk`는 base64 SPKI 문자열과 P-384 JWK 객체를 받는다. 분리 JWS는 ES384만, 오퍼의 지문 한 줄(sha-256만)로 만든 정규 JSON에 대해 검증한다. 실패는 코드 37.
- SDP 상한과 규칙: 본문 64 KiB, 줄 1024개, 후보 32개(UDP만 쓰고 TCP 후보는 버림), 미디어 섹션은 정확히 하나(`application`, `UDP/DTLS/SCTP`, `webrtc-datachannel`), `ice-ufrag` 4~256자, `ice-pwd` 22~256자, 지문은 sha-256 한 줄(32바이트), `setup`은 actpass나 passive(그 밖은 거부), `max-message-size`는 필수이고 1024 이상(보내는 조각은 min(상대 값, 65536) − 1바이트. str0m 직접 API가 송신 SCTP 메시지를 64 KiB로 고정한다. 받는 쪽은 262,144), `sctp-port`는 5000. 파싱 실패는 코드 13.
- 시그널링 상한: 동시 TCP 연결 전체 64, IP별 4(IPv6는 /64), 넘으면 503 후 닫음. 머리 8 KiB, 본문 64 KiB, 요청 하나에 10초(읽기, 협상, 쓰기 포함). 메서드 GET·POST만, `Content-Length`와 `chunked` 둘 다 오면 400, 응답 뒤 항상 닫음. TLS 설정이 없으면 첫 바이트 0x16에 TLS handshake_failure 경고(BDS와 같음)를 보내고 닫는다. TLS는 tokio-rustls(aws-lc-rs 공급자), 인증서와 키 PEM은 시작 때 읽고 틀리면 시작 거부.
- 응답 코드: 성공 200 SDP, 신원 실패 200 "37", SDP 실패 200 "13", 내부 실패 200 "11", 로그인 전 상한 초과 503(3절), 그 밖은 BDS와 같은 400·404.
- 피어 상한(구동부 상수, 구현 때 값을 아래 결과에 적음): 로그인 전 피어 전체 64·IP별 4(RakNet과 따로 센다. 전송마다 상한이 있어 합은 128), 응답부터 두 채널이 열릴 때까지 10초, 다른 라벨·매개변수의 채널이나 세 번째 채널이면 끊음, 재조립은 10.2 메시지 상한(`max-batch-bytes` 식, 0xfe와 체크섬 없음), 조각 수 어긋남·빈 조각은 끊음. 비신뢰 채널은 262,144바이트 이하 메시지를 읽고 버리며 trace로 센다. 받는 큐 1024개·2 × 메시지 바이트, 보내지 못한 바이트 32 MiB, 명령 큐 4096. DTLS 인증서는 시작 때 하나 만들어 모든 피어가 쓴다(지연 초기화 금지, 피어마다 키 생성 비용 없음).
- 이벤트(event.rs, 공개): `Priority`(`Lowest`~`Highest`), 봉인된 `Event` 트레이트, `ServerBuilder::on(priority, |e: &mut E| ...)`. 리스너는 빌드 때 우선순위로 정렬(같은 우선순위는 등록 순서)하고 틱 직렬 단계에서 부른다. M8에서 맥락 인자와 `monitor`를 더할 때 모양이 바뀔 수 있다(공개 전이라 semver 영향 없음).
- 중복 로그인(사용자 결정, PMMP `NetworkSession::setAuthenticationStatus`): 같은 플레이어 = 온라인은 같은 XUID, 오프라인은 대소문자 무시 같은 이름(PMMP `strcasecmp`). 틱의 Join에서 `PlayerDuplicateLogin`을 쏘고, 취소되지 않으면 기존 플레이어를 Disconnect(사유 LoggedInOtherLocation, 문구 기본 "Logged in from other location" = PMMP eng)로 끊고 새 플레이어를 받는다. 취소되면 새 접속을 같은 문구로 끊는다. 이벤트의 사유와 화면 문구는 리스너가 바꿀 수 있다. 시점은 PMMP(로그인 검증 직후)와 달리 Join이다: 게임 상태와 사용자 콜백은 틱에만 있고(4절 2, 5), 로그인에서 틱을 기다리면 세션과 틱의 왕복이 생긴다. 찾기는 (키 → 세션) 색인으로 하고 Leave에서 지운다. PMMP의 XUID 불일치 검사(같은 이름, 다른 XUID)는 키가 XUID라 생기지 않는다.
- 오프라인 이름(사용자 결정, PMMP `Player::isValidUserName`): 1~16바이트, `A-Z a-z 0-9 _ 공백`만, 소문자로 `rcon`, `console`이면 거부. 온라인 이름(토큰 `xname`)은 검사하지 않는다.
- 봇과 시험 도구:
  - 봇에 `-transport raknet|nethernet|auto`를 더한다. nethernet은 go-nethernet `Dialer`(트리클 끔)와 `endpoint.Client`로 접속하고, 신원은 온라인이면 시험 발급자 토큰(Login과 같은 토큰), 오프라인이면 자체 서명 토큰이다. auto는 https → http GET 뒤 NetherNet을 시도하고 실패하면 RakNet으로 간다(위 클라이언트 순서). 결과에 실제 전송과 NetherNet 실패 이유, 받은 응답 SDP를 기록한다.
  - 시나리오 추가: `kicked`(스폰 뒤 LoggedInOtherLocation으로 끊기면 통과), `rejected`(스폰 전 끊기면 통과), `bind-mismatch`(오퍼 키와 Login 키를 다르게 해 거부되면 통과).
  - P6: `p6 bds`가 BDS를 `transport=nethernet`으로도 띄워 nethernet spawn을 돌리고, `transport=raknet` BDS에서 auto 봇이 RakNet으로 폴백해 스폰하는지 본다. `p6 anchor`는 같은 시나리오를 Anchor NetherNet에 돌려 스폰 창과 응답 SDP 줄 구조(값 뺀 키 순서)를 비교한다.

#### M3 판정식
- 시그널링: 요청 형식마다(GET, POST, 없는 경로, 잘못된 networkId, 머리·본문 초과, 둘 다 있는 본문 길이, chunked, 느린 요청 10초, 연결 상한, TLS 미설정에 ClientHello, TLS 설정에 HTTPS 요청) 기대 응답이나 끊김. 변형 요청 1만 개(고정 시드)에서 패닉 0.
- SDP: BDS 응답과 go-nethernet 오퍼 표본 파싱, 상한마다 넘는 입력 거부, 변형 1만 개 패닉 0, 작성한 응답의 줄 순서가 BDS와 같음.
- 신원: 키 파일 생성·재사용·손상 시 시작 거부(파일 그대로), 서버 JWT를 go-nethernet(selfSigned 검증)이 받음(P5), 오퍼 신원은 시험 발급자 토큰 통과, 서명·지문·`alg`·만료·`cpk` 형식이 하나씩 틀리면 37, 온라인에서 신원 없음 37, 오프라인은 받음, Login 키 불일치는 끊김.
- 조각: 크기 0, 1, 262,143, 262,144, 상한 경계 왕복. 조각 수 어긋남, 빈 조각, 상한 초과는 오류.
- 피어: str0m 클라이언트와 메모리 안 왕복으로 두 채널 열림과 최대 크기 메시지 왕복, 잘못된 채널은 끊김, 성립 마감 동작.
- 틱: 중복 로그인 취소·비취소에서 남는 플레이어가 맞음, 같은 Join 두 번은 한 번과 같은 상태 해시, 접속·종료 반복 뒤 색인과 표가 비어 있음.
- 오프라인 이름: PMMP 규칙 경계(0·1·16·17바이트, 허용 밖 문자, `rcon`·`CONSOLE`).
- P5(NetherNet, verify에 포함): zlib·Snappy 스폰, 온라인 시험 발급자, big-skin, RakNet 봇과 NetherNet 봇 동시 스폰, 폴백(NetherNet 협상이 실패하는 설정에서 auto 봇이 RakNet으로 스폰), 로그인 전 피어 상한(503), 결합 불일치 거부, 중복 로그인 두 경우(비취소: 먼저 들어온 봇이 끊기고 새 봇 스폰, 취소: 새 봇이 끊기고 먼저 들어온 봇 유지), 접속·종료 반복 뒤 핑 접속자 0.
- P6: BDS NetherNet에서 봇 spawn 통과, Anchor NetherNet 스폰 창과 응답 SDP 구조가 BDS와 같음(zlib, Snappy), BDS RakNet에서 auto 봇 폴백 스폰.
- `cargo xtask verify` PASS.

#### M3 결과와 정한 값 (2026-10-03)
- 판정: 시그널링(단위 14, 변형 요청 1만 개 패닉 0), SDP·신원(go-nethernet 오퍼와 BDS 응답 골든, 규칙마다 거부, 변형 1만 개 패닉 0, 키 파일 생성·재사용·손상 거부), 조각(13), 피어(str0m 클라이언트와 메모리 안 왕복, 262,143바이트 조각 송신, 열림 전 데이터, 잘못된 채널·지문·마감, 변형 1만 개 패닉 0), 틱(중복 로그인 취소·비취소, 우선순위, 멱등, 100회 반복 뒤 색인 비움), 오프라인 이름(PMMP 경계), P5 15개(RakNet 6, NetherNet 9), vendor 재현 시험(str0m 2, dimpl 5, sctp-proto 9: 원본에서 모두 실패, 사본에서 모두 통과), P6(아래) 모두 통과. `cargo xtask verify` PASS.
- P6(BDS 1.26.52.3 `transport=nethernet`, `cargo xtask p6 bds`, `p6 anchor`): NetherNet 스폰 창이 zlib, Snappy 모두 BDS와 같고 응답 SDP 줄 구조도 같다. BDS는 NetherNet에서도 ServerToClientHandshake를 보내고 암호화는 켜지 않는다(Anchor도 같음). BDS RakNet에서 auto 봇이 폴백해 스폰한다. 게임 UDP 포트는 BDS가 연결마다 `server-port`부터 하나씩 올린다(Anchor는 `nethernet-port` 하나).
- BDS 송신(봇이 조각 단위로 관찰, 두 압축과 spawn·big-skin): 조각 본문 최대 262,143바이트(마지막 아닌 조각은 모두 262,143), 응답 `max-message-size` 262144, 비신뢰 채널로는 보내지 않음, 클라이언트 첫 메시지 전에는 보내지 않음. BDS는 봇이 보낸 65,535바이트 조각도 받는다.
- 관찰할 수 없는 두 항목(사용자 결정): 보내는 쪽은 BDS 값과 같다(조각 262,143바이트, 비신뢰 채널 미사용, 첫 메시지 전 무송신). 받는 쪽은 65,535바이트 조각과 262,143바이트 조각(P5 go-nethernet 봇), 채널 열림 전 신뢰 채널 데이터를 모두 상한(메시지 하나 크기) 안에서 받는다.
- BDS와 다른 결정(기록만): 같은 이름 두 번째 접속을 BDS는 끊지 않고 `이름(2)`로 바꾼다(사용자 결정은 PMMP 방식). 오프라인 BDS는 오퍼 키와 Login 키가 달라도 받는다(Anchor는 2.1대로 끊는다).
- 바뀐 결정:
  - 폴백 시험은 도달 불가 IPv4 `public-ip`로는 실패를 만들 수 없다. 서버 쪽 ICE 검사가 클라이언트에 peer-reflexive 후보를 주기 때문이다. IPv6를 끈 채 IPv6 `public-ip`(광고할 소켓 없음, 코드 11)로 협상 실패를 만든다.
  - 송신 조각은 BDS처럼 262,143바이트다(SDP 항목). str0m 직접 API는 원격 `max-message-size`를 받지 않아, 원격 정보만 str0m SDP API로 넣는다. 그 오퍼에는 클라이언트 setup을 `passive`로 적는다(`actpass`면 str0m이 DTLS server를 골라 BDS와 역할이 달라짐). 클라이언트에게 가는 응답은 계속 `sdp`가 BDS 모양으로 쓴다.
  - str0m 송신 결함(9.1)도 사용자 결정의 vendor 규칙으로 고쳤다. 사용자가 사본으로 정한 크레이트는 sctp-proto와 dimpl이었지만, "보내는 쪽은 BDS 값" 결정을 지킬 다른 길이 없어 같은 규칙을 str0m에 적용했다(되돌릴 수 있음, 보고에 적음).
  - dimpl 사본이 P1·P2를 고쳐, 피어의 임시 가드(반복 레코드 버림, Certificate 길이 검사)는 뺐다(같은 결함을 두 곳에서 막지 않음).
  - 성립 마감(10초)과 무응답(열린 뒤 10초)은 피어가 직접 센다. str0m ICE는 끊김을 알리기만 하고 SCTP는 끝없이 재전송해서다.
  - 신뢰 채널 메시지가 비신뢰 채널 열림보다 먼저 오면(go-nethernet은 열림 확인을 기다리지 않고 보냄) 메시지 하나 크기까지 모았다가 `Open` 뒤에 넘긴다.
  - IP별 묶음(IPv4 하나, IPv6 /64)은 RakNet `ip_key` 하나를 세 곳(RakNet, 시그널링, 피어)이 쓴다.
- 상수(정본은 `src/nethernet.rs`, `src/nethernet/`): 시그널링 연결 64·IP별 4, 머리 8 KiB(넘으면 431), 본문 64 KiB(넘으면 413), 청크 틀 8 KiB, 요청 10초, PEM 1 MiB. SDP는 위 항목, 신원 JSON 32 KiB, 토큰 25 KiB, 서버 JWT 60초. 피어: 로그인 전 64·IP별 4, ufrag 4자·pwd 24자(crypto 모듈 난수, 겹치면 3번까지 다시), 데이터그램 1,500바이트, 피어당 출발 주소 8개, 받는 큐 1024개·2 × 메시지, 보내지 못한 바이트 32 MiB, 명령 큐 4096, 오퍼 큐 64, 갱신 10ms, SCTP 송신 버퍼 1 MiB(가장 큰 메시지 4개).
- str0m 계열 결함과 조치(9.1, CLAUDE.md 7절): 사용자 결정(vendor 사본)으로 고쳤다. 그 밖에 설정으로 없앤 것은 Auto 모드(DTLS 1.2 고정), fastrand ICE 자격(직접 생성), 전역 공급자(`RtcConfig`로 전달)다. 남은 패닉 가능성(SSN 정렬 비교자 등)은 피어마다 `catch_unwind`로 그 피어만 닫는다(CLAUDE.md 4.6의 연결 격리를 한 작업 안의 피어 단위로 적용).
- 남은 위험과 검증 필요:
  - `Join` 전 대기 32와 엄격한 ClientData, 핑 모드 뒤 숫자는 공식 클라이언트로만 확인된다(BDS P6로 닫을 수 없다).
  - dimpl의 레코드 순서 바뀜 문제(#168/#169)와 sctp-proto의 수신 큐 정렬 비용은 M5에서 확인하고 처리했다(10.5).
  - dimpl, sctp-proto는 `log`를 써서 Anchor(`tracing`)만 쓰는 바이너리에서는 그 기록이 사라진다. 사용자 바이너리가 다리를 둘지 정한다(M8 문서).
  - 모든 피어의 DTLS·SCTP가 구동 작업 하나에서 돈다. M5 실측으로 약 500명까지 충분하다(10.5).
  - `identity.key` rename 뒤 폴더 fsync가 없다(Linux 크래시에서 새 키가 생길 수 있음, Linux 검증 때).

### 10.4 M4 세부 설계 (2026-10-03)
- 검증 필요 처리(시작 때):
  - 출처 고정: dragonfly v0.11.5 `4c7b5074`(gophertunnel v1.62.0, 1.26.50 지원), CloudburstMC/Data `659ce1e`(26.50 데이터). 두 목록은 같다(블록 상태 22,091개의 이름·속성·타입·값·버전, 아이템 2,076개의 이름·런타임 ID·버전·컴포넌트 여부). 그래서 12절 2번(기준 목록 출처)은 dragonfly + CloudburstMC로 정한다.
  - PMMP 실행: 일반 PHP(`C:\php`)에는 PMMP가 쓰는 확장(`pmmp\encoding` 등)이 없어 레지스트리를 못 띄운다. pmmp/PHP-Binaries `php-build-524`의 PHP 8.4 PM5 Windows 빌드(zip sha256 `cf4023b8…9be4`)를 `.cache/php-pm5/`에 두고 쓴다(개발 도구). PMMP 의존성은 `composer install --no-dev --ignore-platform-reqs`(PMMP `composer.lock` 고정)로 `.cache/pmmp/vendor`에 둔다.
  - 덤프 실측: PMMP 상태 11,420(직렬화 오류 0) → 서로 다른 Bedrock 상태 9,532. 그중 536개(115개 이름)가 26.50 기준 목록에 없다. 모두 26.50이 더한 속성 때문이다: 울타리·판유리·철창·구리 철창·트립와이어 57개에 `minecraft:connection_{north,east,south,west}`(byte), 계단 58개에 `minecraft:corner`(`none`, `inner_left`, `inner_right`, `outer_left`, `outer_right`). 이름이 사라지거나 값이 바뀐 상태는 없다. 같은 Bedrock 상태로 가는 PMMP 상태끼리 조명·랜덤 틱 값이 다른 경우 0. PMMP 아이템 이름은 모두 기준 목록에 있다.
  - 연결 속성(7절 검증 필요): dragonfly는 이웃으로 계산해 보낸다. BDS에서 울타리·판유리·계단을 이웃과 함께 놓고 보내는 상태를 봇으로 본다(P6 `blocks`). 결과에 따라 서버 계산(M6 블록 변경, M7 동작)인지 기본값 고정인지 정한다. 상태 표는 어느 경우든 추가 속성의 모든 값을 담는다(아래).
- 파이프라인:
  - `cargo xtask dump`: 고정 커밋인지 확인한 뒤 (1) `.cache/pmmp`에서 `xtask/php/dump.php`를 PM5 PHP로 돌려 `xtask/data/pmmp/`에, (2) `xtask/go/reference`로 dragonfly와 CloudburstMC를 읽어 `xtask/data/reference/`에 쓴다. `--check`는 `target/`에 다시 만들어 바이트 비교한다. 출력은 커밋한다. verify에는 넣지 않는다: PMMP는 보관된 마지막 커밋이라 바뀌지 않고, PM5 PHP·composer·두 git 체크아웃이 필요해서다(대신 `gen --check`가 커밋된 데이터에서 만든 코드를 검사한다).
  - 덤프 내용(정렬, 한 줄에 한 항목, NBT는 타입 있는 JSON `{"byte":1}`): `blocks.json`(PMMP 상태마다 PMMP 타입 이름, Bedrock 이름·상태, 빛 방출, 빛 필터(PMMP `getLightFilter` 원값), 직사광 차단, 랜덤 틱, PMMP가 정확한 블록 재료에 붙이는 레거시 meta), `items.json`(PMMP 아이템 형태마다 Bedrock 이름·meta·블록 상태·NBT와 PMMP 이름들: VanillaItems, 블록의 아이템 형태, StringToItemParser 별칭을 Bedrock 형태로 합침, 1,613개), `creative.json`(PMMP 크리에이티브 목록 1,694개, 순서 그대로, 분류와 그룹), `recipes.json`(PMMP `CraftingManagerFromDataHelper::make` 결과: 모양 924, 무형 1,111, 화로 계열 209, 물약 195, 물약 용기 2. PMMP가 읽지 않는 smithing은 PMMP 아이템 해석기로 읽음: 변환 9, 장식 1. 모르는 아이템이 든 레시피는 PMMP처럼 뺀다).
  - 기준 목록(`xtask/data/reference/`): `blocks.json`(dragonfly 상태 + CloudburstMC `network_id`), `items.json`(dragonfly 아이템, 런타임 ID 순, 컴포넌트 데이터 포함). 두 출처가 다르면 도구가 실패한다.
  - 상태 업그레이드 `xtask/data/block_upgrade.json`: PMMP 업그레이드 스키마 형식(`addedProperties`)으로 1.26.30 → 1.26.50 차이를 적는다(손으로 쓴 데이터, 7절 "저장 업그레이드 규칙"과 같은 형식). 기본값은 BDS의 아이템 형태 값이다(`corner` = `none`, `connection_*` = 0, CloudburstMC 크리에이티브 목록으로 확인, P6로 다시 확인). (M9: 이 손 파일은 지우고 BedrockBlockUpgradeSchema 5.3.0의 `0351_1.26.40_to_1.26.50.json`을 읽는다. 0351이 8개 항목을 더 가졌고 버전 필드만 달랐다)
  - `cargo xtask gen`이 커밋된 데이터로 `src/block/table.rs`, `src/item/table.rs`, `src/crafting/table.rs`를 만든다(5절 생성 파일). 아래 중 하나라도 어기면 gen이 실패한다: 업그레이드 뒤 기준 목록에 없는 상태, 쓰이지 않는 업그레이드 항목, 같은 Bedrock 상태의 값 충돌, 기준 목록에 없는 아이템 이름, 풀리지 않는 레시피·크리에이티브 아이템.
- 블록(`anchor::block`, 공개):
  - Anchor 블록 상태 = PMMP 상태의 Bedrock 직렬화(중복 제거)에 업그레이드를 적용한 것 + 추가 속성의 나머지 값(기준 목록에서 옛 속성이 같은 상태 전부). 추가 값은 원래 PMMP 상태의 값을 물려받는다. 순서는 (이름, 상태의 정규 JSON) 정렬이고 `BlockState`는 그 번호(u16)를 가진 `Copy` 값이다.
  - 상태마다: 이름, 속성(byte·int·string), 네트워크 ID(기준 목록의 `network_id`), 빛 방출, 빛 필터, 직사광 차단, 랜덤 틱. 조회: 번호 O(1), 네트워크 ID → 상태(정렬 배열 이분 탐색), 이름+속성 → 상태(정렬 배열). 모든 표는 `static`이라 시작 비용과 할당이 없다.
  - 해시 함수(FNV-1a 32, dragonfly `network_block_hash.go`)는 지금은 시험에만 둔다(골든). 표의 네트워크 ID는 기준 목록 값이다. 커스텀 블록(M8)이 생기면 런타임으로 옮긴다.
  - PMMP 블록 타입(VanillaBlocks)과 PMMP 상태 번호는 공개하지 않는다(동작은 M7). 같은 Bedrock 상태로 겹치는 PMMP 상태의 차이(침대 색 등)는 M6·M7에서 블록 엔티티로 둔다.
- 아이템(`anchor::item`, 공개):
  - `ItemType` = PMMP 아이템 형태(Bedrock 이름, meta, 블록 상태, 형태 NBT)의 번호(u16, `items.json` 순서), `Copy`. 런타임 ID는 기준 목록(1.26.50)에서 온다. 조회: (이름, meta) → 형태, (런타임 ID, meta) → 형태, 블록 상태 → 아이템 형태.
  - ItemRegistry는 기준 목록 전체(2,076개, 컴포넌트 데이터 포함)를 보낸다. 번호 표일 뿐 구현이 아니고(1절 "채우지 않는다"와 무관), PMMP와 BDS도 전체를 보낸다. Anchor가 만드는 아이템은 PMMP 형태뿐이다.
  - 크리에이티브: PMMP 목록과 그룹을 그대로 보낸다(CreativeContent).
  - 생성 코드의 NBT는 `nbt::Static`(정적 트리)로 두고 시작 때 한 번 `nbt::Value`로 바꾼다.
- 레시피(`crafting`, 내부): 덤프의 모든 레시피를 표로 만들고 CraftingData를 PMMP `CraftingDataCache`·`TypeConverter` 규칙으로 만든다(정확한 블록 재료는 덤프의 레거시 meta, meta 와일드카드는 32767, 태그는 태그 이름, 레시피 네트워크 ID는 1부터 차례로). 제작 판정과 사용자 레시피 API는 M7·M8.
- 스폰 패킷:
  - ItemRegistry(전체), CreativeContent, CraftingData를 보낸다. 셋은 모든 플레이어에게 같아서 `build()` 때 한 번 인코드·압축해 `Arc`로 나눠 쓴다(3절 "한 번만 압축"). 세션은 받은 배치를 (RakNet이면) 암호화만 해서 보낸다. 순서는 BDS를 따른다.
  - 유예표 변경: InventoryContent, PlayerHotBar(인벤토리 상태)와 TrimData, UnlockedRecipes, VoxelShapes(PMMP는 보내지 않음, 장식·레시피 책·충돌 모양 동작과 함께)는 M7로 옮긴다.
- 시험 도구: 봇 시나리오 `blocks`(BDS 콘솔로 울타리·판유리·계단을 이웃과 함께 놓고 받은 블록 갱신을 기록). P6 비교에 ItemRegistry 전체 비교, CreativeContent와 CraftingData의 아이템 표현 대조를 더한다.

#### M4 판정식
- 덤프: `cargo xtask dump --check`가 커밋된 파일과 바이트 동일. 기준 목록 두 출처가 같다(도구가 확인).
- 해시 골든: 기준 목록 22,091개 모두 Rust FNV-1a(이름, 정렬한 상태 NBT) = CloudburstMC `network_id`.
- 상태 대조: Anchor 상태가 모두 기준 목록에 있고 네트워크 ID가 같다. PMMP 상태 11,420개가 모두 Anchor 상태 하나로 간다. 업그레이드 항목이 모두 쓰인다. 값 충돌 0.
- 아이템·레시피: PMMP 아이템 형태가 모두 런타임 ID를 가진다. 생성된 크리에이티브 수와 레시피 수가 덤프와 같다. 모든 레시피·크리에이티브 아이템이 Anchor 아이템 형태로 풀린다.
- 알 수 없는 입력: 모든 조회(네트워크 ID, 이름+속성, 런타임 ID+meta, 이름+meta)에 고정 시드 무작위 입력 10만 개씩, 패닉 0이고 없는 것은 `None`.
- P6: BDS `blocks` 시나리오 결과를 적고 연결 속성 방침을 정한다. Anchor 스폰 창이 BDS와 같다(RakNet·NetherNet, zlib·Snappy, 유예표 갱신 뒤). `ItemRegistry Items`가 BDS와 같다. BDS 크리에이티브 목록에 있는 아이템이면 Anchor 크리에이티브 항목은 BDS 항목 하나와 같은 값(NetworkID, MetadataValue, BlockRuntimeID, NBT)이고, CraftingData의 아이템 표현은 BDS가 그 아이템에 쓰는 표현이다. BDS가 아예 쓰지 않는 아이템(교육판 아이템 등)과 레시피 내용 차이는 내용 차이라 목록(note)으로 남긴다(M4에서 바꿈, 아래 결과).
- `cargo xtask verify` PASS.

#### M4 결과와 정한 값 (2026-10-03)
- 판정: 덤프(`dump --check` 바이트 동일, 기준 목록 두 출처 같음), 해시 골든(22,091개 모두 같음), 상태 대조(Anchor 상태 12,468개가 모두 기준 목록에 같은 ID로 있음, PMMP 상태 11,420개가 모두 Anchor 상태로 감, 업그레이드 115개 이름 모두 쓰임, 값 충돌 0), 아이템·레시피(PMMP 아이템 형태 1,613개 모두 런타임 ID, 레시피 7종 수가 덤프와 같음), 알 수 없는 입력(블록 조회 2종·아이템 조회 2종에 고정 시드 10만 개씩, 패닉 0), P6(아래) 모두 통과. `cargo xtask verify` PASS(anchor 단위 323, block 6, item 2, config 14, server 9, xtask 24, protocol 17, compression 5, P5 spawn 15, dimpl 5, sctp-proto 사본).
- P6 결과(BDS 1.26.52.3, `p6 bds`, `p6 anchor`):
  - `blocks` 시나리오: BDS는 연결 속성과 `corner`를 서버에서 계산해 보내고, 이웃이 놓이면 먼저 있던 블록을 바뀐 상태로 다시 보낸다(울타리끼리, 울타리와 돌, 판유리 L자, 철창과 구리 창살은 종류가 달라도 연결, 계단 inner·outer, 트립와이어와 갈고리). 혼자 놓인 블록은 기본값(연결 0, `none`)이다. 결과 표는 `target/p6/bds-<압축>/blocks.txt`. Anchor 상태 표는 추가 속성의 모든 값을 담고, 계산은 M6·M7에서 한다.
  - 스폰 창이 RakNet·NetherNet, zlib·Snappy, 폴백 모두 BDS와 같다: … StartGame, ItemRegistry, CreativeContent, CraftingData, ChunkRadiusUpdated, PlayStatus. ItemRegistry는 런타임 ID로 맞춘 전체 내용과 크기(113,450바이트)가 BDS와 같다. BDS의 순서는 런타임 ID순도 이름순도 아니지만 실행마다 같고, 클라이언트는 ID로 찾으므로 비교는 순서를 보지 않는다.
  - BDS가 크리에이티브 목록에 두지 않는 PMMP 항목 205개(교육판 화학 원소·화합물, 강화 유리 등)와 레시피 내용 차이(BDS의 multi 레시피 14개, UserData 무형 1,456개, 물약 210 대 195, 레시피 ID와 블록 이름 `deprecated` 등)는 note로 남는다.
- 바뀐 결정(10.4 위):
  - 블록 아이템의 네트워크 형태: PMMP 기본 상태 대신 바닐라 크리에이티브 상태(BDS가 보내는 상태, 기준 목록 `item_blocks.json` 1,020개: CloudburstMC와 dragonfly가 같고 블록 목록의 ID와 같음)로 보낸다. 이름에 PMMP 블록 아이템 타입과 바닐라 상태가 하나씩일 때만 쓰고, 아이템이 놓는 상태는 그대로 PMMP 상태다. 바닐라 상태가 PMMP에 없는 상태여도 네트워크 ID로만 쓴다(유약 바른 테라코타 아래 방향, 횃불 `unknown`, 사다리, 머리). 이유: P6에서 PMMP 기본 상태(계단 방향 3, 울타리 문 북쪽 등)가 BDS와 달랐다(7절 "다르면 BDS").
  - 크리에이티브: PMMP가 모르는 인챈트를 버려 생긴 빈 인챈트 책 61개는 뺀다(1,694 → 1,633). 크리에이티브 아이템 네트워크 ID는 BDS처럼 1부터 센다(PMMP는 0부터). 불길한 배너는 BDS처럼 meta 15(PMMP는 0)이고, 이 보정은 덤프의 모든 아이템이 거치는 한 곳(`gen/data.rs`)에 둔다.
  - P6 아이템 대조 규칙(판정식): BDS가 그 아이템을 목록에 두면 형태가 같아야 하고, 아예 두지 않으면 내용 차이(note)다. 이유: 교육판 아이템은 PMMP 목록의 내용이고 BDS는 교육 기능이 꺼져 숨긴다. 형태(ID, meta, 블록 ID, NBT)의 차이는 여전히 실패다.
  - 공유 배치: 세 패킷은 `build()`가 아니라 `run()` 시작 때 한 번 인코드·압축한다(설정과 같은 시점에 실패를 알린다). 틱은 `Arc`만 넘기고(`Outbound::Send`의 `Message::Shared`), 세션은 앞의 패킷을 먼저 보내고 배치를 (RakNet이면) 암호화해 보낸다. 크기는 ItemRegistry 113 KB, CraftingData 381 KB, CreativeContent 50 KB로 `max-batch-bytes` 최소(1 MiB) 안이다.
  - 실드 런타임 ID(358)는 생성 표의 상수로 두고 세션 코덱이 쓴다(M3까지 0).
  - 덤프는 아이템 태그를 쓰지 않는다(M4에서 쓰는 곳이 없음, 제작 판정을 하는 M7에서 더한다).
  - NetherNet 닫기(M3 결함, M4 끝에 찾음): 세션이 닫으면 구동부가 피어를 바로 지워(SCTP abort) 마지막 패킷(Disconnect)이 클라이언트에 닿지 않을 수 있었다(P5 취소된 중복 로그인 시험이 약 10번에 1번 실패). 이제 닫힌 피어는 보낸 것이 모두 확인(SCTP ACK)되거나 5초(RakNet 닫기와 같음)가 지날 때까지 남고, 구동부는 닫힘 신호를 보기 전에 큐의 명령을 먼저 처리한다. 고친 뒤 P5 spawn 20회 연속 통과, 재현 시험 `sent_messages_are_drained_once_acknowledged`.
- 정한 값과 규칙(정본은 생성기와 `src/block.rs`, `src/item.rs`, `src/crafting.rs`):
  - 블록 조회: 네트워크 ID 정렬 배열 이분 탐색, 이름+속성은 이름 범위 안 이분 탐색(속성 8개 이하, 입력 순서 무관, 할당 없음). `PropertyValue<'a>`는 빌린 문자열을 받는다.
  - 아이템 조회(PMMP 규칙): 같은 이름(런타임 ID)·블록 중 meta가 같은 타입, 여럿이면 형태 NBT가 스택 NBT에 든 가장 구체적인 것, 없고 이름·블록의 타입이 하나면 그 타입(meta는 스택 상태: 도구 내구도, 폭죽 별 색). (이름, meta)는 유일하지 않다(배너, 버섯 블록). 블록 → 아이템 역조회는 덤프에 `asItem` 정보가 없어 두지 않는다(M7).
  - 크리에이티브 그룹: PMMP `CreativeInventoryCache` 규칙(처음 나올 때 번호, 같은 분류·그룹의 연속 구간이 한 그룹, 익명 그룹은 분류마다 하나이고 이름 있는 그룹이 나오면 새로 시작, 아이콘 NBT에 `___GroupBugWorkaround___`), 분류 번호 1~4. 107그룹.
  - CraftingData(PMMP `CraftingDataCache`·`TypeConverter`): 네트워크 ID 1부터 모양·무형·화로(PMMP `FurnaceType` 순)·대장장이 순, UUID nil, priority 50, 재료는 1.26.40 형식인 이름+meta(와일드카드 32767, 정확한 블록 재료는 PMMP 레거시 meta, 태그는 이름), 물약은 런타임 ID, 레시피 ID는 네트워크 ID의 10진 문자열(PMMP의 4바이트는 UTF-8이 아닐 수 있음), 해금 조건은 "항상"(`Some(context 1, 재료 없음)`, BDS도 늘 넣음), smithing은 dragonfly처럼 보냄, `clear_recipes` true.
- 남은 위험과 검증 필요:
  - 공식 클라이언트에서만 보이는 것: 교육판 아이템이 교육 기능을 끈 클라이언트의 크리에이티브 창에 어떻게 보이는지, BDS와 다른 레시피 ID·블록 이름(`crafting_table` 대 `deprecated`)이 레시피 책에 주는 영향. PMMP가 같은 값으로 쓰여 왔으므로 그대로 둔다. 검증 필요(M7): 레시피 책·제작 P6에서 다시 본다.
  - 클라이언트가 블록 아이템을 보낸 형태가 Anchor가 보낸 네트워크 블록 ID와 다르면 그 아이템은 `None`이다. 검증 필요(M7): 인벤토리 요청 P6.
  - Anchor 디코더의 목록 상한 1,024(10.1)는 ItemRegistry·CraftingData 전체를 디코드하지 못한다. 서버는 이 패킷을 받지 않으므로 영향이 없고, 시험은 1,024개씩 나눠 왕복한다.

### 10.5 M5 세부 설계 (2026-10-03)
- 검증 필요 처리(시작 때, 서브에이전트가 따로 한다):
  - dimpl 레코드 순서 바뀜(#168/#169): 피어 시험 틀에서 핸드셰이크 데이터그램 순서를 바꿔 넣어 성립하는지, 마감(10초) 안에 끝나는지 본다. 멈추면 vendor 규칙(9.1)으로 고치거나 영향(그 피어만 실패)을 적는다.
  - sctp-proto 수신 큐 정렬 비용: 순서가 섞인 청크를 받는 비용을 실측한다(조각 256개 메시지, 손실·순서 바꿈).
  - NetherNet 구동부 한 작업의 비용: 피어 수와 바이트에 따른 데이터그램 처리 시간을 실측해 한 작업으로 몇 피어를 감당하는지 적는다. 모자라면 M6 전에 나눌 방법을 적는다.
- CPU 풀(`src/pool.rs`, `pub(crate)`): rayon `ThreadPool` 하나(스레드 = `cpu-threads`, 스택 4 MiB, 이름 `anchor-cpu`)를 `runtime.rs`가 `run()` 시작 때 만든다. 쓰는 길은 `Pool::map(&[T], f) -> Vec<R>` 하나이고 결과는 입력 순서다(스레드 수와 무관하게 같은 결과). rayon 전역 풀을 만드는 진입점(`join`, `scope`, `spawn`, 병렬 반복자 메서드 등)은 `.clippy.toml` 금지 목록에 두고 `pool.rs`만 푼다. 정상 종료 때 풀의 스레드가 모두 끝날 때까지 기다린다(rayon은 drop에서 기다리지 않아 `exit_handler`로 센다). rayon을 쓰는 이유: 틱 상태를 빌려 쓰는 상주 스레드 풀은 `unsafe` 없이 직접 만들 수 없다(6절 1번).
- 틱 단계(`step`): 1 입력(직렬, (세션, 순번) 순: 접속, 패킷, 종료가 상태를 바꾸고 플레이어에게 보낼 일을 표시) → 2 계산(병렬 읽기 전용) → 3 적용(직렬) → 4 출력(병렬 읽기 전용: 표시된 플레이어마다 패킷 목록을 만든다) → 전달(직렬, 플레이어 순서로 큐에 넣음). M5에는 계산할 게임 로직이 없어 2·3 단계는 그 일이 생기는 마일스톤(M6 청크, M7 엔티티)에서 더한다. 스폰 패킷(StartGame, 공유 배치, 청크 반경 응답, 청크, PlayStatus)은 입력 단계가 표시만 하고 출력 단계가 만든다.
- 입력 로그(`tick/log.rs`):
  - 틱마다 정렬된 입력을 적는다: 세션 ID, 순번, 종류(접속: 이름·XUID·UUID, 패킷: 클라이언트가 보낸 원래 바이트, 종료). 세션이 디코드할 때 원래 바이트를 입력에 같이 넣어 틱이 다시 인코드하지 않는다. 엔진 시각은 틱 번호라 따로 적지 않고, 비동기 결과는 생기는 마일스톤(M8)에서 더한다.
  - 상한 64 MiB(바이트 합). 넘으면 더 적지 않고 "잘림"으로 표시한다(재생은 잘린 곳까지만). 마지막 DB 커밋에서 비우는 것은 M6(그 전에는 시작부터).
  - 틱 스레드 패닉: 틱 스레드의 가드가 풀릴 때(`catch_unwind` 없이 drop에서 `thread::panicking()`) 로그를 메인 스레드로 넘기고, `Server::run`이 `data/crash/input-<unix 초>.log`로 쓴다(틱 스레드는 파일을 쓰지 않음). 형식: 머리(`ANCHOR-INPUT-LOG`, 형식 버전, 프로토콜 버전, 잘림 여부) 뒤에 틱마다 (틱 번호, 입력 수, 입력들), 길이는 LE 고정 폭.
  - 재생: `replay(settings, shared, listeners, pool, log)`가 같은 `step`을 돌려 상태를 만든다(크레이트 내부, 재생 도구는 M9 운영 기능).
- 결정성 도구: 틱 코드의 실수 `max`·`min`·`clamp`(`f32`, `f64`)를 `.clippy.toml`로 막는다(엔진 도우미는 처음 필요한 M7에서 만든다).

#### M5 판정식
- 결정성: 고정 시드로 만든 입력(세션 64개, 400틱, 접속·종료·청크 반경·초기화·중복 접속·모르는 패킷)을 풀 스레드 1개와 N개(N = max(논리 CPU, 4))로 돌려 틱마다 상태 해시와 모든 출력이 같다. 같은 입력의 로그를 재생한 상태 해시와 출력이 실행 때와 같다. Windows에서 `cargo test`로 돈다.
- 로그: 인코드 → 디코드 왕복이 같고, 상한을 넘으면 잘림 표시와 함께 멈추며, 깨진 로그(변형 1만 개, 고정 시드)는 패닉 없이 오류다. 틱 스레드 패닉 시험에서 `data/crash/`에 로그가 남고 그 로그를 재생한 상태가 패닉 직전과 같다.
- 풀: 결과 순서가 입력 순서이고, 종료 뒤 풀 스레드가 남지 않는다. 금지 목록의 rayon 진입점이 실제로 걸리는지 임시 코드로 확인한다(6절).
- 검증 필요 3개의 결과를 적는다.
- `cargo xtask verify` PASS.

#### M5 결과와 정한 값 (2026-10-03)
- 판정: 결정성(시드 8개, 시드마다 세션 64개·400틱, 풀 1개와 max(L,4)개의 틱별 상태 해시와 모든 세션의 출력이 같음, 로그 재생 해시가 풀 1개·여러 개 모두 실행 때와 같고 재생이 같은 로그를 다시 기록함), 로그(쓰고 읽기 같음, 깨진 로그 변형 1만 개 패닉 0, 상한에서 잘림과 미완성 틱 제외, 빈 틱도 상한에 셈), 패닉(리스너 패닉으로 틱 스레드가 죽으면 로그가 메인 스레드로 넘어가 `data/crash/`에 쓰이고, 그 로그의 마지막 틱 전까지는 재생되고 전체 재생은 같은 곳에서 다시 패닉), 풀(입력 순서 유지, 잡 패닉 전파, 종료 뒤 스레드 0), 검증 필요 3개(아래) 모두 통과. `cargo xtask verify` PASS.
- 바뀐 결정과 정한 값:
  - 실수 `max`·`min`·`clamp`는 M0부터 이미 `.clippy.toml`에 막혀 있었다(10.5의 항목은 할 일 없음).
  - 세션이 넘겨 준 뒤 큐가 가득 차거나 닫혀 틱이 잊은 세션은 입력이다(재생이 같은 곳에서 잊어야 같은 상태가 됨). 로그는 틱마다 그 목록을 따로 적는다.
  - 로그는 패킷을 원래 바이트가 아니라 디코드된 값으로 갖고(틱이 다시 인코드하지 않음, 복사 없음), 상한 계산용으로 세션이 넘긴 바이트 수만 더 받는다(`InputKind::Packet { packet, size }`). 쓸 때(메인 스레드) 인코드한다.
  - 한 틱의 같은 플레이어 청크 반경 요청은 마지막 반경으로 한 번 답한다(출력 단계가 표시를 모아 만든다).
  - 크래시 로그 파일 이름은 `input-<unix 초>-<마지막 틱>.log`, 같은 이름이 있으면 덮지 않는다.
- 검증 필요 처리 결과(release 실측, 이 Windows 기기):
  - dimpl 레코드 순서(#168/#169): 데이터그램 사이 순서 바뀜·절반 중복·첫 전송 손실에서 300회 모두 성립, 무작위 손실 5% 300/300, 10% 288/300, 30% 132/300. 멈추는 경우는 한 데이터그램 안의 레코드 순서가 바뀔 때뿐(보내는 쪽만 만들 수 있음)이고 그 피어만 10초에 닫힌다. 고치지 않는다(9.1).
  - 10초 안 실패의 원인은 SCTP INIT 재전송 타이머(3초에서 두 배)다(10% 손실에서 4%). str0m RTO 초기값을 1초로 낮췄다(M6에서 닫음, 10.6 SCTP).
  - sctp-proto 수신 큐: 실제로 이차였다(순서가 어긋난 폭을 키우면 청크당 0.96 µs → 9.1 µs). 정렬된 삽입으로 고쳐 0.63~0.71 µs로 일정(256 × 262,144바이트 역순 115 ms → 40.6 ms, 10% 손실 44.7 → 20.3 ms). 9.1 ⑦⑧.
  - NetherNet 구동 작업 하나: 피어 송신 26.3 ms/MiB → 3.55 ms/MiB(피어가 str0m에 자리가 없을 때도 262 KB 조각을 매번 복사하던 것과 sctp-proto ⑧을 고침), 수신 2.91 ms/MiB, 틱당 트래픽(50 ms마다 2,560 B 내림·256 B 올림) 초당 206 µs, 시스템 호출 `send_to` 4.2 µs·`recv_from` 0.47 µs. 플레이어 한 명(50 KB/s 내림, 5 KB/s 올림) 초당 약 0.53 ms, 접속 1회(2 MiB) 약 16 ms. 작업 하나로 약 500명까지는 충분하다(코어의 약 30%와 접속 몰림). 넘으면 받는 작업 하나가 분배하고 피어를 K개 작업에 나눈다(그때 설계). 측정은 `#[ignore]` 시험: `cargo test --release --lib nethernet::peer::tests::driver_cost_per_peer -- --ignored --nocapture`, `... handshake_under_random_loss ...`, `cargo test --release --manifest-path vendor/sctp-proto/Cargo.toml --lib receive_cost -- --ignored --nocapture`.
- 남은 위험: 로그 상한(64 MiB)은 DB 커밋으로 비우는 M6 전에는 시작부터 센다. 빈 틱만 있어도 약 29시간 뒤에는 잘린다. 재생 도구(크래시 로그를 읽어 재생하는 명령)는 M9 운영 기능이다.

### 10.6 M6 세부 설계 (2026-10-03)
- 검증 필요 처리(시작 때):
  - redb 가드(9.2): crates.io 최신이 4.3.0이라 "4.4.0 이상"을 지킬 수 없다. #1517을 고친 상류 커밋을 찾아 4.3.0과 비교하고, 고쳐지지 않았으면 vendor 규칙(1절)으로 사본을 두고 재현 시험을 단다. 7절 절차 전체를 같은 일에서 한다.
  - HeightMap 접두와 서브청크 형식(10.1): BDS가 보내는 SubChunk를 P6 봇이 모두 기록하고 Anchor 디코더(`protocol::PalettedStorage`)로 읽어 본다.
  - SCTP 성립 실패(10.5): sctp-proto 초기 RTO(3초)를 낮췄을 때 손실 10%의 10초 안 성립률을 실측해 정한다(서브에이전트).
  - 12절 5번(LevelDB 가져오기): 포함하고 M9 운영 기능에서 만든다. LevelDB 리더, Bedrock 디스크 서브청크, 옛 블록 상태 업그레이드 스키마 전체가 필요한 큰 일이고 M6 판정과 무관하며, Anchor 저장 형식이 정해진 뒤 만드는 것이 맞다.
- 일 나누기(같은 파일을 동시에 고치지 않는다): W 월드(`src/world*`, 블록 표 확장과 PMMP 덤프), S 저장(`src/storage*`, redb, `runtime.rs`의 저장 스레드), P P6(`xtask/src/p6*`, `xtask/go/bot`, `xtask/data/p6`), N SCTP RTO(`src/nethernet/peer.rs`)를 worktree에서 병렬로 하고, 틱 통합(`src/tick*`, `src/server.rs`)은 합친 뒤 한다. 강제 종료 시험은 마지막에 한다.

#### 월드 (`anchor::world`, 공개)
- 좌표 `BlockPos { x, y, z: i32 }`, `ChunkPos { x, z: i32 }`(`Copy`). 차원은 오버월드 하나, 높이 -64..=319(서브청크 24개, 번호 -4..=19).
- `World`: 이름, 종류(`Kind::Persistent`, `Kind::Temporary`: 저장 안 함), 시드, 스폰, 생성기, 로드된 청크 색인. `block(pos) -> Option<block::State>`(로드 안 된 청크는 `None`), `set_block(pos, state) -> Result`(범위 밖, 로드 안 된 청크, 격리된 청크는 오류). 레이어 1(물 잠김)은 담고 저장하지만 바꾸는 API와 동작은 M7이다.
- 청크: 서브청크 24칸 `Option<Arc<SubChunk>>`(빈 칸은 `None`). 쓰기는 `Arc::make_mut`라 저장 스냅샷이 쥔 서브청크만 한 번 복사된다(COW). 높이맵(열마다 가장 높은 비공기 블록), 바이옴(서브청크마다 팔레트, Bedrock 3D 바이옴), 바뀜 표시(저장용)를 가진다.
- 서브청크: 레이어마다 팔레트 + 비트 배열(폭 {0,1,2,3,4,5,6,8,16}, Bedrock 디스크·네트워크와 같은 묶음이라 인코딩에 다시 묶지 않는다). 팔레트 항목은 알려진 상태(`block::State`) 또는 그 청크의 "알 수 없는 상태" 번호다. 알 수 없는 상태는 디스크 원본(NBT)을 그대로 보존해 다시 쓰고, 네트워크로는 `minecraft:info_update`를 보낸다(7절).
- 생성기(`world::Generator` 트레이트, 공개): `Send + Sync`, 시드와 청크 좌표만 받는 순수 함수(5절)로 빈 청크를 채운다. 기본 `Flat`은 BDS 기본 평지(y -64 bedrock, -63·-62 dirt, -61 grass_block, 바이옴 plains, P6 세계와 같음), `Void`는 모두 공기(plains). 생성된 뒤 바뀌지 않은 청크는 저장하지 않고 다시 생성한다(생성기를 바꾸면 안 바뀐 청크도 바뀐다: rustdoc).
- 연결 속성(7절, 10.4): `set_block`이 놓인 블록과 수평 이웃 4개의 연결 상태를 다시 계산한다. 울타리·판유리·철창·트립와이어·계단 `corner`는 BDS P6 결과(10.4 `blocks.txt`)를, 벽(`wall_connection_type_*`, `wall_post_bit`)과 그 밖의 판정(무엇이 단단한 면인가)은 PMMP `readStateFromWorld` 규칙을 따른다. 판정에 쓰는 PMMP 값(면 지지 형태 등)은 M4 덤프에 더한다. 생성기가 쓴 상태는 다시 계산하지 않는다(생성기 책임, rustdoc).
- 네트워크 인코딩: 서브청크 버전 9(레이어, 런타임 팔레트), 바이옴 팔레트, 경계 블록 0개. 같은 틱에 여러 플레이어가 요청한 서브청크는 한 번만 인코딩해 `Arc`로 나눈다(3절).
- 색인: 청크 색인은 처음에 `BTreeMap<ChunkPos, _>`이고, 블록 조회 비용을 재서 더 빠르면 반복 API가 없는 조회 전용 해시 색인(고정 해시, 4.4)으로 바꾼다(이유를 남김).

#### 청크 전송 (틱)
- 방식은 BDS와 같은 서브청크 요청 방식이다: LevelChunk(바이옴과 경계 블록만, `SubChunkCount` 0, `SubChunkLimit` = 바닥부터 비공기 서브청크가 있는 칸 수) → 클라이언트 SubChunkRequest → SubChunk(결과, 페이로드, 높이맵). 블롭 캐시는 쓰지 않는다(`CacheEnabled` false. PMMP·dragonfly 기본도 같음, ClientCacheStatus는 받고 버림). 정확한 값과 순서는 P6 BDS 기록을 따른다.
- 시야: 플레이어마다 중심 청크(위치)와 허가한 반경 안의 원(x² + z² ≤ r²)을 가까운 순서로 보낸다. 틱당 `chunks-per-tick`개까지, 로드된 청크만. 스폰 전에는 `spawn-radius` 안이 다 보내지면 PlayStatus(PlayerSpawn)를 보낸다. 시야를 벗어난 청크는 보낸 목록에서 지운다. 중심이나 반경이 바뀌면 NetworkChunkPublisherUpdate.
- SubChunkRequest: 그 플레이어에게 보낸 청크만 답하고, 아니면 ChunkNotFound. 범위 밖은 IndexOutOfBounds, 빈 서브청크는 SuccessAllAir(dragonfly `ViewSubChunks`와 같음, 값은 P6). 오프셋 수는 디코더 공통 상한(1024) 안이고, 한 틱에 답하는 서브청크 수에 플레이어마다 상한을 둔다(넘친 요청은 다음 틱).
- 위치: PlayerAuthInput의 위치를 그대로 받아 시야 중심으로 쓴다(이동 검증은 M7).
- 스폰 창의 M6 패킷(유예표 `M6` 줄): BiomeDefinitionList는 CloudburstMC `biome_definitions.json`(26.50)으로 만들어 시작 때 한 번 인코드하는 공유 배치다. SetSpawnPosition, JigsawStructureData, CurrentStructureFeature는 P6에서 BDS 값을 보고 보낼지와 값을 정한다(바닐라 구조물은 범위 밖).
- 로드 상한: 플레이어가 없는 청크는 내리고(바뀐 청크는 커밋될 때까지 메모리에 둠), 로더로 보낸 요청 수와 틱당 반영하는 결과 수에 상한을 둔다.

#### 블록 변경 (틱)
- 틱 안의 변경은 월드의 변경 목록(위치별 마지막 상태)에 모이고, 출력 단계가 그 청크를 받은 플레이어에게 보낸다. 형식은 BDS를 따른다(10.4 결과: 한 블록이면 UpdateBlock, 여럿이면 서브청크마다 UpdateSubChunkBlocks, flags 3). BDS가 같은 위치를 한 단계에 두 번 보내는 것은 따라 하지 않는다(위치별 마지막 상태가 같으면 같은 결과).
- 예약 작업 API(공개, 잠정: M8에서 맥락 인자가 넓어질 수 있음): `ServerBuilder::every(주기 틱, |ctx: &mut Context| ..)`(PMMP `TaskScheduler::scheduleRepeatingTask`). `Context`는 틱 번호, 월드 조회·변경, 저장 요청(`save`: 다음 틱 경계에 커밋)을 준다. 빌드 때 등록하고 틱 직렬 단계(입력 뒤)에서 등록 순서로 불러, 재생이 같은 결과를 낸다. P6와 강제 종료 시험이 이 API로 블록을 바꾼다(시험 전용 진입점 없음).
- 엑스트라 월드: `ServerBuilder::world(이름, Kind, 생성기)`. 기본 월드는 설정(`level-name`, `level-type`, `level-seed`)에서 온다. 플레이어는 M6에서 늘 기본 월드에 들어온다(월드 이동은 M7).

#### 저장 (`storage`, 내부)
- 파일 `data/world.redb` 하나. 표: `meta`(형식 버전, 엔진 틱), `worlds`(이름 → 번호, 시드, 스폰, 생성기 이름), `chunks`(키 13바이트 = 월드 번호 u32, x i32, z i32 빅엔디안 부호 비트 뒤집음, 종류 u8: 0 지형. 블록 엔티티·엔티티 종류는 M7), `players`(XUID 10진 문자열, 오프라인은 `offline:` + 소문자 이름: 위치, M7에서 늘어남). 임시 월드는 쓰지 않는다. ops·화이트리스트·밴은 M9, 사용자 표는 M8.
- 값: 머리(형식 버전, 압축 방식, 원래 길이, 체크섬) + 본문. 체크섬은 저장된 바이트 전체에 대해 검사하고, 읽을 때 길이와 개수는 할당 전에 상한 검사한다(디스크는 외부 입력). 압축 방식(deflate 또는 Snappy, 둘 다 이미 있음)은 실측으로 정한다. 블록은 이름과 상태(Bedrock 디스크 팔레트 모양 NBT `{name, states, version}`)로만 쓴다.
- 시작: 닫힌 파일을 `data/backups/world-<unix 초>.redb`로 복사(최근 3개만 남김) → 열기 → `check_integrity()` → 손상이면 시작 거부(원본 그대로, 백업이 남음) → 형식 버전이 더 새것이면 거부 → `compact()`. 정상 종료 때 마지막 커밋 뒤 `compact()`. redb가 확인 과정에서 파일을 고치는지 7절 분석으로 확인하고, 고친다면 고치기 전 바이트가 백업에 있음을 시험한다.
- 시작 검사 설정(M9 후속 U9, `integrity-check`): `always`(기본)는 늘 `check_integrity()`, `never`는 하지 않음, `after-unclean-shutdown`은 정상 종료 표시가 없을 때만 한다. 표시는 `meta` 표의 키 `closed`(빈 값)다. 정상 종료의 `Storage::close`가 compact 뒤 마지막 커밋으로 쓰고, 시작의 첫 커밋(없는 표를 만드는 커밋)이 지운다. 그래서 표시가 있으면 마지막 실행이 정상 종료로 닫은 뒤 커밋이 없었다(원자성: 표시는 커밋 하나로만 바뀜, 멱등: 쓰기·지우기가 절대값). 커밋·읽기 실패가 있었던 종료, 틱·저장 스레드 패닉, 강제 종료는 표시를 남기지 않는다. 표시는 시작 전에 읽기 전용 열기(`open_read_only`, 파일을 쓰지 않음)로 읽고, 열지 못하면(복구 필요·손상·사용 중) 표시 없음으로 보아 검사한다. 실행 중 백업은 표시 없는 파일의 복사라, 되돌려 쓰면 다음 시작이 검사한다. 시작 복사와의 관계: `backups = true`의 시작 백업은 검사와 상관없이 늘 만든다(백업 설정이 정함). `backups = false`의 임시 사본은 검사가 파일을 고치거나 거부할 때 바이트 그대로 되돌리기 위한 것이라 검사하는 시작에서만 만든다. 검사도 백업도 없는 시작에서 더 새 형식으로 거부된 파일은 열기가 쓴 redb 머리만큼 바뀐 채 남는다(바이트 그대로 보존은 사본이 있는 시작만). 시험: `a_start_checks_and_copies_the_file_as_its_settings_say`, `only_a_file_closed_after_a_normal_stop_skips_the_check_after_an_unclean_one`.
- 실행 중 검사 `/check`(M9 후속 U9, op, `anchor.command.check`, 게임 안·콘솔, PMMP에 없어 이름·문장은 Anchor): 다음 스냅샷에 검사 표시 → 커밋 뒤 저장 스레드가 그 커밋의 읽기 트랜잭션 하나로 모든 값을 8 MiB 단계씩 읽어 봉인(머리·CRC-32C)을 확인한다(`seal::verify`, 압축 풀기·디코드 없음, redb가 읽다가 낸 손상은 그 표를 끝내고 하나로 셈). 단계 사이에 밀린 요청을 먼저 처리하고, 실행 중 백업이 있으면 백업이 먼저다. 틱은 기다리지 않는다. redb `check_integrity()`는 닫힌 파일(트랜잭션 없음)이 필요하고 손상을 고치면 파일을 덮어써(2.1 손상 규칙) 실행 중에는 쓰지 않는다. 한 번에 하나(틱의 요청자 한 칸, 두 번째는 거부 문장). 결과 `Response::Checked`(통과 값 수, 손상 수와 첫 손상의 표·오류, 끝내지 못한 이유)는 틱 입력이라 입력 로그에 남고(형식 11) 요청자에게 Anchor 문장으로 간다(그새 나간 플레이어는 버림, 콘솔은 로그). 저장 스레드도 info·error로 남긴다. 파일 다시 열기(실패 재시도)는 검사를 실패로 끝내 답하고, 정상 종료는 진행 중 검사를 버린다(compact가 읽기 트랜잭션을 기다리지 않게).
- 커밋: 저장 주기(`save-interval-minutes`), `Context::save`, 커밋되지 않은 채 내린 청크가 상한(4096개)을 넘음, 정상 종료 중 하나가 생기면 틱 끝에 스냅샷(바뀐 서브청크 `Arc`, 바뀐 플레이어 데이터, meta)을 저장 스레드(큐 1칸)에 넘긴다. 저장 스레드가 바쁘면 다음 틱 경계로 미룬다(틱은 기다리지 않음). 저장 스레드가 직렬화·압축하고 쓰기 트랜잭션 하나(`set_quick_repair(true)`)로 커밋한다. 실패는 핸들을 다시 열어 3번 재시도, 그래도 실패하면 모든 플레이어를 내보내고 입력 로그를 남긴 뒤 종료한다(6절).
- 입력 로그: 스냅샷을 뜬 틱부터 새 구간을 시작하고, 그 커밋이 성공했다는 결과가 틱에 들어오면 앞 구간을 버린다(10.5 "잘림 위험" 해소). 크래시 로그는 마지막 성공 커밋부터다.
- 로드: 청크·플레이어 데이터는 틱이 저장 스레드에 상한 있는 요청으로 보내고, 저장 스레드가 읽기 트랜잭션으로 읽은 뒤 디코드와 (없는 청크의) 생성을 CPU 풀에서 병렬로 한다. 결과는 다음 틱 이후의 입력이고 로그에 도착(키, 출처 = 디스크·생성·손상, 내용 체크섬)으로 적는다. 재생은 같은 DB와 생성기로 다시 만들고 체크섬으로 같은지 확인한다. 같은 키의 데이터가 메모리(로드됨 또는 커밋 대기)에 있으면 로드하지 않는다. 메모리 항목은 그것을 담은 커밋의 성공을 틱이 안 뒤에만 지우므로, 그 뒤의 읽기는 늘 새 커밋을 본다.
- 손상: 실행 중 체크섬·디코드가 실패한 청크는 격리한다(메모리의 격리 집합: 빈 청크로 보내고, 바꾸지 않고, 저장하지 않음, 원본은 표에 그대로, error 한 번). 플레이어 데이터가 손상이면 그 플레이어의 접속을 거부한다(원본 그대로, error). 자동 복원은 하지 않는다(2.1).
- 스레드: 저장 스레드 하나(`anchor-save`, `runtime.rs`가 만듦, 예산의 "저장 1")가 읽기와 커밋을 한다. 커밋 동안 읽기는 기다린다(커밋은 저장 주기마다라 드묾).

#### M6 판정식
- 월드: 서브청크·청크 get/set을 배열 모델과 고정 시드 무작위 10만 연산으로 대조(팔레트 폭 늘고 줄음, 두 레이어). 스냅샷 뒤 변경이 스냅샷에 보이지 않음(COW). 높이맵이 전수 계산과 같음. 연결 속성: P6 `blocks` 단계마다 BDS와 같은 결과. 생성기 결정성(같은 시드·좌표 = 같은 바이트). 네트워크 인코딩이 Anchor 디코더와 왕복.
- 저장: 무작위 청크·플레이어 값 왕복 동일, 깨진 값 변형 1만 개(고정 시드)에서 패닉 0이고 오류, 상한 초과 선언은 할당 전 오류, 알 수 없는 블록 원본 보존 왕복. 시작 정책: 손상 파일은 시작 거부(파일 바이트 그대로), 더 새 형식 거부, 백업 3개 유지. 실행 중 손상 청크 격리(빈 청크, 변경 거부, 원본 유지).
- 장애 주입(redb `StorageBackend` 감싸기): 쓰기·동기화·길이 변경 실패를 N번째 호출마다 넣어 커밋이 재시도로 성공하거나 3번 실패 뒤 종료하고, 동기화 안 된 쓰기를 버리는 전원 차단 모의 뒤 다시 열면 무결성 통과이고 상태가 앞이나 뒤 커밋 하나와 정확히 같음. 고정 시드 1,000회.
- 강제 종료: 시험이 자기 시험 바이너리를 자식 프로세스로 띄워 서버를 돌리고(예약 작업이 매 틱 토큰 블록 하나를 정해진 경로로 옮기고 몇 틱마다 저장 요청), 무작위 시점에 그 자식만 PID(핸들)로 죽인 뒤 다시 연다. 50회 모두: 무결성 통과, 체크섬 실패 0(손상 0), 토큰이 정확히 하나(복제 0, 손실 0), 토큰 위치 = 커밋된 엔진 틱의 경로 위치(청크와 meta가 함께 커밋됨).
- 멱등과 누수: SubChunkRequest·RequestChunkRadius·같은 블록 변경을 두 번 넣은 상태 해시 = 한 번. 접속·이동·종료 100회 반복 뒤 로드 청크, 대기 요청, 색인, 큐가 시작 값.
- 결정성: M5 시험에 청크 로드·생성 결과 도착, PlayerAuthInput 이동, 예약 작업의 블록 변경, 커밋 결과를 더해 스레드 1과 N의 틱별 해시·출력이 같고 재생이 같다.
- P6: `spawn`에서 LevelChunk·SubChunk·NetworkChunkPublisherUpdate의 정해진 필드와 서브청크 페이로드(같은 평지)가 BDS와 같고, `blocks` 단계마다 위치별 마지막 상태와 패킷 종류·flags가 BDS와 같다(RakNet·NetherNet, zlib·Snappy). 유예표의 M6 줄을 모두 없애거나 이유와 함께 옮긴다.
- SCTP RTO 결정과 실측을 적는다. `cargo xtask verify` PASS.

#### M6 결과와 정한 값 (2026-10-03)
- 판정: 월드(get/set 무작위 10만 연산 여러 묶음, COW, 높이맵 전수 대조, 생성기 결정성, 인코딩 왕복, 평지 페이로드 BDS와 같음), 저장(아래 저장 판정), 장애 주입(redb 백엔드 감싸기 1,000회), 강제 종료(`tests/crash.rs` 50회: 무결성 통과, 블록 정확히 하나, 커밋된 틱의 위치, 80% 이상이 커밋 뒤에 죽음), 멱등과 누수(SubChunkRequest·RequestChunkRadius·같은 블록 변경 두 번 = 한 번, 접속·종료 100회 뒤 붙든 청크·대기 요청·격리·플레이어 로드 표가 시작 값), 결정성(시드 8개, M5 시나리오에 이동·NaN 이동·서브청크 요청·예약 작업 블록 변경·저장 요청·청크 로드 결과를 더해 풀 1개와 max(L,4)개의 틱별 해시·출력이 같고 로그 재생이 같음), P6(아래) 모두 통과. `cargo xtask verify` PASS.
- 일 나누기 결과: 서브에이전트 5개(W 월드, S 저장, P P6, N SCTP, G 바이옴)가 각자의 worktree에서 병렬로 하고, 틱 통합(`tick/view.rs`, `tick/save.rs`, `tick/task.rs`, `save.rs`, `server.rs`)과 강제 종료 시험은 합친 뒤 했다. worktree와 브랜치는 모두 지웠다.
- P6 BDS 사실(BDS 1.26.52.3, 봇이 스폰 청크 둘레 3x3에 요청하고 probe 3개를 보냄):
  - LevelChunk: SubChunkCount 0, CacheEnabled false, BlobHashes 빈 목록, Dimension 0. SubChunkLimit = 바닥(-4)부터 가장 높은 비공기 서브청크까지의 칸 수(평지 1, dragonfly `HighestFilledSubChunk`와 같음). 페이로드는 바이옴 24칸(같으면 0xff)과 경계 블록 0개, 평지는 26바이트 `01 02 ff×23 00`.
  - 시점: BDS는 반경 요청 전부터 청크를 보내고(첫 틱 15개, 그 뒤 틱마다 20개), PlayStatus(PlayerSpawn)는 청크 95개 뒤(반경이 아니라 개수·시간 기준)다. 스폰 뒤에도 x²+z² ≤ 290의 895개를 보낸다.
  - NetworkChunkPublisherUpdate: 틱마다, Position = floor(눈 위치)(M4의 [0,32769,0]은 안전 스폰 찾기 높이 32767 + 1.62), Radius = 반경 × 16, SavedChunks 빈 목록.
  - SubChunk: 요청 하나에 패킷 하나, 엔트리는 요청 오프셋 순서, Position은 요청 중심. 지형은 Result 1(평지 -4는 1,049바이트, 2비트, 팔레트 [bedrock, dirt, grass_block, air] = 첫 사용 순서), 위 공기는 Result 6에 높이맵 3/3(TooLow), 열이 섞인 공기는 6에 타입 1·Render 4(AllCopied), 범위 밖 5, 보낸 적 없는 청크 2(높이맵 0/0).
  - 높이맵 값 = 열의 최고 비공기 블록 y − 서브청크 바닥 y + 1, 위 서브청크면 16, 아래면 -1, 모두 아래면 TooLow. 10.1 "검증 필요(M6)" 둘을 닫았다: BDS의 모든 서브청크 페이로드(216칸)가 Anchor 엄격 규칙을 통과하고, HeightMap 행마다 varuint32 16 접두가 있다.
  - BiomeDefinitionList는 CloudburstMC `stripped_biome_definitions.json`과 같다(정의 89개, BiomeID -1, 문자열 187개). JigsawStructureData는 CloudburstMC `jigsaw_structure_data.nbt`와 같다. SetSpawnPosition은 ItemRegistry 뒤 한 번(SpawnType 0, 두 위치 i32::MIN×3, Dimension 3). CurrentStructureFeature는 첫 청크 틱에 "".
  - blocks 47단계(10.4의 25단계 + 벽, 네더 벽돌 울타리, 울타리 문, 판유리와 벽, 계단과 돌·울타리, 제거 2개, 높은 블록): 벽은 혼자면 기둥 1, 한 줄의 끝은 기둥 1, 곧게 지나는 가운데는 기둥 0, L자 모서리는 기둥 1, 연결은 늘 short. 벽과 울타리는 연결되지 않고 벽과 판유리는 연결된다. 네더 벽돌 울타리와 나무 울타리는 연결되지 않는다. 울타리 문 양옆 울타리는 문 쪽으로 연결된다. 트립와이어를 놓으면 `suspended_bit` 1, 마주 보는 두 갈고리 사이 한 줄이면 줄과 갈고리가 `attached_bit` 1. flags 3, layer 0, UpdateSubChunkBlocks의 Position은 서브청크 원점, SyncedUpdateEntityUniqueID u64::MAX. BDS는 같은 위치를 한 단계에 두 번 보내기도 한다.
- P6 Anchor 결과(`p6 anchor`, RakNet·NetherNet, zlib·Snappy, 폴백): 스폰 창(청크 패킷 제외), 청크 비교(LevelChunk 필드 집합, SubChunk 엔트리 37개 모두 결과·높이맵·페이로드 해시·레이어 내용, 엄격 규칙, 스폰 전 가까운 순서, 반경 뒤 Publisher 열), blocks 47단계(단계 위치와 이웃 8칸의 위치별 마지막 상태와 flags, 패킷 종류), CloudburstMC 바이옴 대조 모두 BDS와 같다. 다른 것은 note: 청크 수와 시점(BDS 95개 뒤 스폰, Anchor는 `spawn-radius` 원 49개 뒤), BDS가 한 블록을 UpdateSubChunkBlocks로 보내거나 두 패킷으로 나누는 경우, BDS 랜덤 틱의 잔디→흙 2개.
- 바뀐 결정과 정한 값(10.6 위):
  - 스폰 청크: 월드마다 스폰 둘레 3 × 3 청크를 시작부터 붙든다(PMMP `isSpawnChunk`). 플레이어 없이도 예약 작업이 블록을 바꿀 수 있다. 스폰을 옮겨도 붙든 청크는 시작 때 것이다(검증 필요(M8): 스폰 이동 API).
  - 벽 기둥은 BDS 규칙(곧게 지나지 않으면 기둥)에 PMMP 규칙(위가 공기가 아니면 기둥)을 더했다. 트립와이어의 `suspended_bit`·`attached_bit`는 M7 동작으로 미루려 했으나 P6가 블록 변경으로 비교하므로 M6 연결 규칙에 넣었다(BDS를 따름. 줄 길이 40 상한은 바닐라).
  - (2026-10-04 바꿈, 사용자 실제 접속) JigsawStructureData는 SyncWorldClocks 뒤, VoxelShapes 앞에 CloudburstMC `jigsaw_structure_data.nbt` 내용으로 보낸다(없으면 공식 클라이언트가 `disconnectionScreen.missingStructureData`로 끊는다. 처음 결정 "바닐라 구조물이 범위 밖이라 안 보냄"은 틀렸다). CurrentStructureFeature는 첫 청크 틱 자리(SetActorData 뒤, UpdateAttributes 앞)에 `""`로 한 번 보낸다(BDS는 창 밖에서도 `""`를 되풀이하지만 값이 바뀌지 않는다). 블롭 캐시는 쓰지 않는다.
  - 스폰 창 원칙(2026-10-04, 사용자 결정): 공식 클라이언트가 필요할 수 있는 패킷은 BDS의 위치와 내용으로 보낸다. 세계와 무관한 패킷은 BDS와 같은 내용으로 보내고, 유예표에는 Anchor에 없는 것(자연 몹)과 클라이언트 동작에 영향이 없는 것(참가 플레이어의 spawn 소리 LevelSoundEvent, PMMP·dragonfly도 안 보냄)과 레시피 해금(UnlockedRecipes, PMMP처럼 모두 해금)만 `-`로 남긴다. SyncActorProperty 13개는 CloudburstMC `entity_properties.nbt`(파일 순서 = BDS 순서, 내용·크기 같음), CameraPresets·CameraAimAssistPresets는 BDS 캡처 `xtask/data/reference/bds_camera.json`(BDS 캡처 54개에서 값이 같음), CameraSpline은 빈 목록으로 보낸다. `p6`의 CloudburstMC 확인은 이제 차이를 숨기지 않는다(Biome·Jigsaw 둘 다, 양쪽 캡처 모두; 검토 R9-D1).
  - Publisher는 시야 중심이나 반경이 바뀔 때만 보낸다(BDS는 틱마다. PMMP처럼 바뀔 때만으로 대역을 줄이고, 비교는 연속 중복을 하나로 본다).
  - 블록 변경 패킷: 서브청크마다 한 블록이면 UpdateBlock, 여럿이면 UpdateSubChunkBlocks.
  - 6절 백업: 읽기 트랜잭션 복사 대신 열기 전 닫힌 파일을 배타 잠금 아래 복사한다(S, 9.1 redb).
  - 커밋 실패(3회 재시도 뒤)나 저장 스레드 종료(패닉)는 틱을 멈추고 모든 플레이어를 내보낸 뒤 마지막 커밋 이후 입력 로그를 `data/crash/`에 쓰고 `Error::Storage`로 끝낸다. 실패 뒤에는 최종 스냅샷을 만들지 않는다.
  - 입력 로그: 형식 2(시작 틱을 담음). 저장 스레드 답(청크 출처, 플레이어 데이터, 커밋 결과)을 틱마다 기록한다. 스냅샷은 출력 전달 뒤 틱 경계에서 뜬다(전달에서 잊은 세션이 상태를 바꾸므로).
  - `Generator::name`(저장된 생성기 이름, 다르면 경고 후 설정한 생성기를 씀)을 더했다. 새 월드의 빈 `level-seed`는 crypto 난수로 한 번 정해 저장한다. 엔진 틱은 커밋된 meta에서 이어진다.
  - CPU 풀을 틱과 저장 스레드가 `Arc`로 나눠 쓴다(디코드·생성·봉인은 풀에서 병렬). 풀의 종료 수신기는 잠그지 않는 `Mutex`에 둔다(`Sync`만을 위해, `off-tick`).
  - 공개 API(잠정, M8): `world` 모듈(`World`, `BlockPos`, `ChunkPos`, `Chunk`, `Generator`, `Flat`, `Void`, `Kind`, `Error`), `Context`, `ServerBuilder::every`, `ServerBuilder::world`, `Error::DuplicateWorld`·`WorldDamaged`·`WorldNewerFormat`·`Storage`.
- 상수(정본은 `src/tick/view.rs`, `src/tick/save.rs`, `src/save.rs`): 동시 청크 로드 1024, 틱당 로드 요청 256, 플레이어당 대기 서브청크 요청 256(넘으면 버림)·틱당 답 64, 내린 미커밋 청크나 나간 플레이어의 미커밋 데이터가 4096개를 넘으면 커밋, 요청 큐 64, 답 큐 2048, 틱당 반영하는 답 1024, 좌표 상한 3천만(월드 경계), 블록 상태 `version` 18,168,865(기준 목록 값), 시야 창은 틱당 전송 수의 4배.
- 저장(S): 값 머리 12바이트(형식 버전 u16, 방식 u8(0 저장, 1 Snappy), 예약 u8, 레코드 길이 u32, CRC-32C u32. CRC는 키·머리 앞 8바이트·본문을 덮음), 읽을 때 버전 → 체크섬 → 해제 순, `FORMAT_VERSION` 1, 레코드 상한 16 MiB(M9: 형식 4, 6절. 16 MiB의 근거였던 "청크 약 12 MiB"는 틀렸다: 바닐라 최악 약 22 MB, 커스텀 약 226 MB), 디코드 예산 64 MiB, 64바이트 미만 무압축. 압축은 Snappy(실측: 지형 청크 15,089 B에서 Snappy 18.3%·압축 18~21 µs·해제 10~12 µs, deflate 1단계 12.2%·31~35 µs·20~22 µs). CRC-32C는 손으로 쓴 8바이트씩 계산(2.4~2.8 GB/s, 하드웨어 명령은 `unsafe` 필요). redb 캐시 64 MiB, 재시도 3, 백업 3, 월드 목록 1024, 이름 256바이트. 시작: 닫힌 파일을 배타 잠금 아래 백업(잠기면 InUse) → 열기 → `check_integrity()` → 형식 확인 → `compact()` → 빈 커밋으로 표 만들기. Ok(false)(redb가 제자리에서 고침)도 손상이다. 손상이거나 더 새 형식이면 백업 바이트를 파일에 다시 써 시작 전 그대로 두고 그 백업을 지운다. 백업 정리는 성공한 시작 뒤에만.
  - 저장 판정: 무작위 레코드 왕복, 깨진 봉인 값 1만 개(절반은 체크섬을 다시 맞춤) 패닉 0, 상한 초과는 할당 전 오류, 알 수 없는 블록 NBT 보존, 손상 파일·더 새 형식은 시작 거부(바이트 그대로), 백업 3개, 다른 프로세스가 연 파일 거부, 쓰기·길이 변경·동기화 실패는 재시도로 성공하거나 3회 뒤 오류, 크래시·전원 차단 1,000회(동기화 안 된 쓰기를 모두 잃거나 일부만 순서대로 남기고 페이지 단위로 찢음, 4분의 1은 종료 compact 중) 모두 무결성 통과·앞 또는 새 커밋과 정확히 같음.
  - 실측(release): 봉인 지형 42~51 µs, 해제 40~49 µs, 1,000청크 커밋 24~85 ms, 10,000청크 128~802 ms(fsync 편차), 읽기 0.7~2.7 µs/청크, 같은 1,000청크를 50번 다시 써도 파일 크기 그대로(#1517 고침 확인), 종료 compact 195~233 ms, 시작(복사·검사·compact) 171~243 ms.
- 월드(W): 청크 색인은 조회 전용 해시 색인(고정 Fibonacci 해시, 선형 탐사, 반복 API 없음)과 순서용 `BTreeSet`이다. release 실측 조회 BTreeMap 14~29 ns → 1.3~1.7 ns(청크 25~1,089개), `block` 16~39 ns → 5~7 ns. 팔레트는 항목마다 사용 수를 세어 4096개를 넘지 않고, 가득 차면 넓히고 1/4 이하만 쓰면 좁힌다. 디스크와 생성기 결과는 압축한다(첫 사용 순서 = BDS 순서). 연결 갱신 범위는 놓인 블록, 수평 이웃 4개, 아래 블록, 면이 바뀐 이웃의 수평 이웃이고, 로드되지 않은 이웃은 공기로 보고 바꾸지 않는다. 생성기가 쓴 상태와 `Chunk::set_block`은 다시 계산하지 않는다. 덤프에 상태마다 `full_faces`(PMMP `getSupportType` FULL인 수평 면)와 `connect`(fence 그룹, thin, wall, fence_gate, stair 모양별 면, tripwire, tripwire_hook)를 더했다. PMMP와 다르게 고친 것: 네더 벽돌 울타리가 나무 울타리에 한쪽만 붙음 → 같은 클래스끼리만 양쪽(BDS와 같음).
- 바이옴(G): `cargo xtask dump`가 CloudburstMC `stripped_biome_definitions.json`을 `xtask/data/reference/biomes.json`(이름순)으로 옮기고(ID·생성 데이터·0 아닌 밀도·모르는 필드가 있으면 실패) `gen`이 `src/world/biome/table.rs`를 만든다. 이름순·처음 나온 문자열 순으로 보내고(BDS는 바닐라 번호순), BiomeID -1이라 클라이언트는 이름으로 찾는다. 페이로드 7,667바이트로 BDS와 같다.
- SCTP(N, 10.5 검증 필요 닫음): str0m 사본에서 SCTP RTO 초기값을 3초에서 1초로 낮췄다(RFC 9260 16절 RTO.Initial, pion/sctp 1초, libwebrtc dcsctp INIT·COOKIE 1초). 성립 마감 10초는 그대로다. sctp-proto RTO.Min이 1초라 첫 RTT 측정 뒤 RTO는 그대로이고 첫 측정 전 타이머만 짧아진다. 실측(release, 손실마다 3,000회, 10초 안 성립률): 3초 99.43%·96.20%·42.3%(손실 5·10·30%), 3초+마감 20초 99.77%·98.70%·67.2%, 양쪽 1초(채택) 99.93%·99.40%·65.9%, 양쪽 0.5초 100%·99.63%·73.7%. 0.5초는 RTO.Min보다 작아 고르지 않았고, 마감 연장은 반쯤 열린 피어를 오래 붙잡으면서 1초 RTO보다 못했다. 30%에서는 RTO와 상관없이 약 9%가 10초 안에 DTLS를 끝내지 못한다.
- 남은 위험과 검증 필요:
  - 크래시 로그는 마지막 커밋부터지만, 그 시점의 실행 상태(접속한 세션, 시야, 로드된 청크 위치)는 DB에 없다. 커밋 뒤 로그만으로 재생하려면 구간 시작에 실행 상태 체크포인트가 필요하다. 검증 필요(M9): 재생 도구와 함께 만든다(시험은 시작부터의 로그로 재생한다).
  - 입력 로그 상한 64 MiB를 한 저장 주기 안에 채우면(많은 플레이어의 PlayerAuthInput) 그 구간은 잘린다. 저장 주기가 길수록 위험하다(재생 도구 M9에서 다시 본다).
  - 스폰 때 PlayStatus 시점(BDS 95개 뒤, Anchor `spawn-radius` 원 뒤)과 틱마다 Publisher를 보내지 않는 차이는 공식 클라이언트에서만 영향이 보인다(봇은 같게 동작).
  - BDS의 바이옴·문자열 순서와 다른 BiomeDefinitionList 순서가 공식 클라이언트에 영향이 없는지는 관찰할 수 없다.
  - 저장 시작 비용이 파일 크기에 비례한다(1 GB 이상은 검증 필요(M9)). 백업 3개가 파일 크기의 3배를 쓴다. redb 남은 결함(#1503 조작 파일, #1334 실행 중 하드웨어 손상)은 9.1.
  - `tick` 결정성 시험이 디버그에서 약 150초 걸린다(청크 생성과 해시). verify 시간이 늘었다.
  - PowerShell에서 verify를 돌리면 MSVC 링커의 한국어 메시지가 `linker_messages` 경고가 되어 실패한다(aws-lc jitterentropy 심볼 내보내기). Bash에서 돌린다.

### 10.7 M7 세부 설계 (2026-10-03)
- 검증 필요 처리(시작 때, P6 서브에이전트가 BDS로 확인해 결과에 적는다):
  - 8.6 거래: 오퍼 NBT의 나머지 키, 거래 요청의 액션 순서와 `netId` 대응, 레거시 `villager`로 Trade2 화면이 열리는지, `DemandBasedPrices` 값, 거래 뒤 갱신 방식.
  - 10.1 Event 패킷의 ordinal 없는 타입 8개: BDS가 보내는지 본다. 보내지 않으면 Anchor도 보내지 않고 닫는다.
  - 10.4 레시피 책과 블록 아이템 형태: 제작 P6에서 클라이언트(봇)가 보내는 요청의 아이템 형태와 레시피 ID가 BDS에서 통하는지, Anchor가 같은 요청을 받는지 대조한다.
  - 12절 7번 `Vec3`: f64로 정한다(아래 공통 도구).
- 공통 도구(메인이 먼저 만드는 약속):
  - `anchor::math`(공개): `Vec3 { x, y, z: f64 }`, `Aabb`(모두 `Copy`). 틱 상태의 실수는 f64이고 프로토콜 경계에서 f32로 바꾼다. 결정적 `min`·`max`·`clamp`(같은 입력에 늘 같은 비트, ±0은 정해진 쪽)와 사칙연산·`sqrt`만 쓰는 `sin`·`cos`(다항식 근사. std 삼각함수는 플랫폼마다 다를 수 있어 금지 목록에 있다).
  - `random`(내부): 카운터 방식 난수. `Random::new(world_seed, tick, stream)`가 SplitMix64를 세 값의 혼합으로 시작한다. 상태를 저장하지 않으므로 재시작·재생·병렬 단계에서 같은 값이 나온다(dragonfly·PMMP 전역 난수의 비결정성을 고친다). `stream`은 용도 태그와 위치·엔티티·세션의 혼합이고, 한 틱에 같은 `stream`을 두 번 쓰지 않는다.
- 모듈과 책임:
  - 규칙 코드는 `src/tick/` 아래 틱 코드다(틱 상태의 비공개 필드를 쓴다, 4절 4번): 생존, 인벤토리 창과 요청 실행, 부수기·놓기·쓰기, 엔티티 갱신·물리·추적, 블록 갱신(랜덤·예약·이웃, 농사, 액체, 불), 작업대 계열, 거래.
  - 값 타입은 공개 모듈에 둔다: `item::ItemStack`(불변 값, 8.1), `entity::Id`(색인 + 세대, `Copy`), 조명·블록 엔티티 조회(`world`). M7은 시험·예약 작업에 필요한 만큼만 공개하고 나머지 공개 API는 M8이다.
  - 이벤트는 M8이다. M7 규칙 코드는 취소할 수 있는 행동을 "검사 → 적용" 두 함수로 나눠 M8이 그 사이에 이벤트를 넣을 수 있게 한다.
- 데이터(PMMP 덤프에 더함, 출처는 7절과 같은 PMMP 6a7cc02 실행):
  - 블록 타입마다 PMMP 클래스와 트레이트 이름(동작을 고르는 키), 부수기 정보(단단함, 폭발 저항, 도구 종류, 채굴 등급), 불(가연성, 번짐 정도, 영원히 탐), 단단함·투명·대체 가능, 마찰, 상태마다 충돌 상자와 6면 지지 형태, 연료 시간, 실크 터치 여부, 타입 태그.
  - 아이템마다 최대 스택, 내구도, 도구(종류, 등급, 채굴 효율, 공격력), 갑옷(칸, 방어, 강인함, 넉백 저항), 음식(회복, 포화, 배고픔 필요, 먹은 뒤 효과), 연료(시간, 남는 것), 인챈트 가능성과 태그, 재사용 대기.
  - 표: 상태 효과, 물약, 인챈트(희귀도, 최대 레벨, 아이템 태그, 비용 식의 상수, 서로 안 되는 것), 아이템 태그(태그 재료와 인챈트용), 배너 무늬, 장식 무늬와 재료(TrimData는 BDS 값과 대조).
  - 드롭과 경험치처럼 PMMP가 코드로 정하는 것은 규칙 코드가 PMMP를 옮기고, 결정적인 경우(도구 없음, 맞는 도구, 실크 터치)는 덤프 값과 같은지 시험한다.
- 조명(6절 규칙 그대로):
  - 서브청크마다 블록빛·하늘빛 4비트 배열. 처음 계산은 청크가 로드·생성될 때 저장 스레드의 CPU 풀에서 하고(PMMP `LightPopulationTask`처럼 틱 밖), 이웃 청크 경계는 틱이 증분으로 맞춘다.
  - 블록 변경 뒤 증분 갱신(PMMP `LightUpdate`: 줄이기 BFS 뒤 늘리기 BFS). 틱당 처리 노드에 상한을 두고 남은 일은 다음 틱이다. 큐가 상한에 닿으면 그 청크를 "다시 계산"으로 표시한다(일을 버리지 않음).
  - 계산 안 된 위치는 `None`. `lighting = false`는 계산하지 않고 블록빛 0·하늘빛 15다. 클라이언트로 보내지 않는다.
- 블록 갱신:
  - 랜덤 틱은 6절 그대로다(정렬 순서, 엔진 난수, 3x3 로드·조명 조건).
  - 예약 갱신(PMMP `scheduleDelayedBlockUpdate`: 액체, 낙하 블록 등): 월드마다 (틱, 순번) 순서 큐, 틱당 처리 상한. 청크와 함께 저장한다(PMMP는 저장하지 않아 재시작하면 흐르던 물이 멈춘다 → 고쳐 가져옴).
  - 이웃 갱신(PMMP `onNearbyBlockChange`): 블록이 바뀌면 6방향 이웃에 한 번. 같은 틱의 연쇄는 상한 있는 FIFO이고 넘친 일은 다음 틱이다.
  - 동작: 농사, 랜덤 틱 계열, 액체와 양동이, 낙하 블록, 불(8.4)은 PMMP 클래스 동작을 옮긴다.
- 플레이어(생존): 체력, 배고픔(`hunger = false`면 바뀌지 않음), 경험치, 상태 효과, 피해(낙하, 불, 용암, 익사, 질식, 폭발, 공격)와 죽음·리스폰, 게임 모드와 능력, 속성, 근접 공격(PMMP 넉백·무적 시간), 음식, 침대와 수면(월드 시간, 밤 넘기기), 이동 검증(PMMP 규칙). 스폰 창의 M7 유예 패킷은 BDS 값으로 보낸다(P6).
- 인벤토리:
  - 플레이어 인벤토리(36칸, 갑옷 4, 왼손, 커서, 제작 칸 2x2·3x3, 작업대 입력·결과 칸)와 블록·엔티티 컨테이너. 아이템은 서버 권위 `ItemStackRequest`(따로 오거나 PlayerAuthInput 안)로만 옮긴다. 레거시 `InventoryTransaction`은 UseItem·UseItemOnEntity·ReleaseItem만 받고 그 안의 아이템 목록은 믿지 않는다(PMMP와 같음).
  - 요청 하나는 한 단위다: 모든 액션을 복사본에 적용해 보고 하나라도 실패하면 모두 버리고 오류로 답한다. 스택 네트워크 ID는 플레이어마다 늘어나는 번호이고 요청에 적힌 ID가 그 칸의 현재 ID와 다르면 거부한다.
  - 멱등: 요청 ID는 플레이어마다 최근 것(상한 있는 기록)을 기억하고, 다시 오면 상태를 바꾸지 않고 오류로 답한다.
  - 창 닫기·떠남·죽음: 커서·제작 칸·작업대 입력 칸 아이템은 인벤토리로 돌아가고 자리가 없으면 발밑에 떨어진다(PMMP). 같은 컨테이너를 여럿이 보면 바뀐 칸을 다른 사람에게도 보낸다.
  - 제작 판정: 모양 레시피는 모양과 거울상을 정확히 검사한다(8.4 dragonfly 결함). 무형은 재료 다중집합 일치, 태그 재료는 아이템 태그 표. 레시피 네트워크 ID → 레시피는 O(1)이다.
- 작업대 계열: 화로·용광로·훈연기(PMMP 시간과 연료), 모닥불, 양조기, 인챈트대(PMMP `EnchantingHelper`, 인챈트 시드는 플레이어 데이터), 석재 절단기는 PMMP, 모루·대장장이 작업대·베틀·신호기 결제는 dragonfly를 참고한다(8.4). 상태는 블록 엔티티이고 청크와 함께 저장한다.
- 엔티티:
  - 월드마다 저장소. `entity::Id`(색인 + 세대), 런타임 ID는 `next_runtime_id`(고유 ID = 런타임 ID), 청크 색인으로 근처를 찾는다. 반복 순서는 Id 순.
  - 종류는 닫힌 enum: 떨어진 아이템, 경험치 구슬, 낙하 블록, 점화된 TNT, 발사체(화살, 눈덩이, 달걀, 엔더 진주, 투척·잔류 물약), 물약 구름, 주민형 NPC(거래), 사람형 NPC(PMMP `Human`). 사용자 정의 엔티티는 M8 확장 지점이다.
  - 물리는 PMMP `Entity::move`(중력, 저항, 블록 충돌 상자로 축별 충돌). 계산 단계(병렬, 읽기 전용)에서 다음 위치를 구하고 적용 단계에서 반영한다.
  - 추적: 엔티티가 있는 청크를 받은 플레이어에게 생성·이동(BDS 형식)·제거 패킷을 보낸다.
  - 다른 플레이어의 이동(U8, BDS와 다름): 위치나 yaw·pitch가 바뀐 틱에 `MoveActorAbsolute`(눈 위치, 땅 플래그, 회전 [pitch, yaw, yaw], teleport 플래그 없음)로 보낸다(PMMP `broadcastMovement`, dragonfly `ViewEntityMovement`). 클라이언트는 이것을 다음 프레임들에 걸쳐 보간하고, `MovePlayer`는 곧바로 옮겨서 BDS처럼 보내면 1초에 20번 뚝뚝 끊긴다. `MovePlayer`는 본인 위치 보정(되돌리기)과 순간이동(mode 2, 보는 사람에게도)에만 쓴다. 회전은 1바이트(1.40625°) 단위로 간다. 예전에는 다른 플레이어의 회전을 보내지 않았다(`MovePlayer` 회전 0). P6 `others`의 `moves of the other player` 차이는 intended.txt에 둔다.
  - 떨어진 아이템: 줍기 지연, 합치기, 6,000틱 뒤 사라짐(PMMP). 월드마다 엔티티 수 상한이 있고, 상한에서는 새 엔티티가 필요한 플레이어 행동을 거부한다(아이템을 버리지 않음).
- 거래: 8.6 그대로.
- 저장: 형식 2. 플레이어 레코드 = 위치 + NBT(인벤토리, 체력, 배고픔, 경험치, 효과, 게임 모드, 스폰 지점, 인챈트 시드 등. 각 규칙 모듈이 자기 키를 읽고 쓴다). 청크 종류 1 블록 엔티티, 2 엔티티, 3 예약 갱신(각각 NBT 목록). 모두 지형·플레이어와 같은 커밋에 든다. 형식 1 값은 그대로 읽는다(추가 데이터 없음).
- 프로토콜 상수: 입력 플래그, 컨테이너 ID, 창 종류, 엔티티 메타데이터 키·플래그, 레벨 이벤트, 액터 이벤트, 플레이어 액션, 속성 이름, 능력 비트, 게임 규칙 이름 등은 추출기가 gophertunnel 상수를 IR에 담고 `gen`이 만든다(손으로 옮겨 적지 않음).
- 일 나누기: 단계 A(데이터, 조명, P6 조사, 저장 형식, 프로토콜 상수) → 합침 → 단계 B(인벤토리·제작 판정, 엔티티 기본, 부수기·놓기·쓰기, 블록 갱신, 생존) → 합침 → 단계 C(작업대 계열, 엔티티 확장, 거래, 보존·강제 종료 시험, P6 Anchor 대조). 단계마다 약속(타입, 함수 시그니처, 틱 연결)을 메인이 먼저 만든다. 같은 파일은 한 서브에이전트만 고치고 공유 파일(lib.rs, Cargo.toml, .clippy.toml, PLAN.md, tick.rs 분기)은 메인이 고친다.

#### M7 판정식
- 데이터: `dump --check`, `gen --check`. 새 값이 같은 Bedrock 상태끼리 충돌 0. 덤프에 결정적 드롭(도구 없음, 맞는 도구, 실크 터치)을 담고 규칙 코드의 드롭이 모든 블록 타입에서 같다.
- 조명: 고정 시드 무작위 블록 변경 1만 번 뒤 증분 결과 = 전수 계산(청크 경계 포함). 틱당 상한을 지키고 남은 일이 다음 틱들에 끝난다. PMMP 규칙 값(방출에서 1씩 줄어듦, 필터, 열린 하늘 15, 직사광 차단)과 `lighting = false` 값.
- 블록 갱신: 랜덤 틱 대상·순서·난수가 결정적이고, 성장 확률이 PMMP 식과 통계적으로 맞는다(고정 시드 표본, 허용 오차를 시험에 적음). 예약 갱신이 저장·재시작 뒤 이어진다. 액체 흐름이 PMMP 규칙의 정해진 장면 결과와 같다.
- 인벤토리와 보존: 고정 시드 무작위 요청 10만 개(유효·무효 섞음)에서 아이템 총량이 만들기·버리기·소비·떨어뜨리기로 설명되는 만큼만 바뀌고(떨어뜨린 것은 엔티티로 셈), 스택 상한·빈 칸 규칙을 어기지 않는다. 같은 요청 두 번 = 한 번(상태 해시). 요청 중 떠남·창 닫기·죽음에서 커서·제작 칸·입력 칸 아이템이 돌아온다(복제·소실 0).
- 제작과 작업대: 모든 모양 레시피(거울상 포함)와 무형 레시피가 맞는 칸에서 만들어지고, 한 칸 어긋난 모양과 모자란 재료는 거부된다. 화로·모닥불·양조·인챈트·모루·대장장이·석재 절단기·베틀·신호기가 PMMP(없으면 dragonfly) 규칙의 시험 표와 같고, 각각 보존·멱등·중간 끊김 시험을 통과한다.
- 거래: 8.6 원칙(요청 ID 한 번, 최신 리비전, 재료·남은 횟수 검사, 반복 상한 256, 실패하면 모두 되돌림, 창 닫기·대상 사라짐에 입력 칸 반환)과 보존·멱등.
- 엔티티: 물리 결정성, 줍기·합치기 보존, 상한에서 거부, 생성·제거 반복 뒤 표가 시작 값.
- 자동 시나리오(틱 시험, 패킷 입력): 나무 캐기 → 판자 → 작업대 → 곡괭이 → 돌 캐기 → 화로 굽기 → 농사(뼛가루, 수확) → 인챈트 → 거래까지 한 흐름이 통과한다.
- 강제 종료: `tests/crash.rs`에 아이템 흐름(플레이어 인벤토리, 컨테이너, 떨어진 아이템 사이 이동)을 더해 50회 모두 무결성 통과, 아이템 총량 그대로(복제 0, 소실 0), 상태 = 커밋된 틱.
- 결정성: M6 시험 스크립트에 M7 입력(인벤토리 요청, 부수기·놓기·쓰기, 상호작용, 공격)을 더해 스레드 1과 N의 틱별 해시·출력이 같고 재생이 같다. 상태 해시는 구조체 분해(`..` 없이)로 모든 필드를 써서 새 필드가 빠지면 컴파일이 실패한다.
- 누수: 접속·종료, 창 열고 닫기, 엔티티 생성·제거, 예약 갱신 반복 뒤 모든 표·큐가 시작 값.
- 성능: 플레이어 수, 엔티티 수, 랜덤 틱, 조명 갱신의 틱 비용을 실측해 적는다. 측정으로 고른 곳은 이유를 남긴다.
- P6: `craft`, `furnace`, `enchant`, `crops`, `trade` 시나리오가 BDS에서 먼저 통과하고, Anchor에서 정해진 필드(응답 상태, 바뀐 칸 내용, 블록 갱신 상태, 창 종류, 오퍼 모양)가 BDS와 같다(난수 값인 인챈트 선택지와 거래 목록 내용은 뺀다). 유예표의 M7 줄을 모두 없애거나 이유와 함께 옮긴다. 검증 필요 4개의 결과를 적는다.
- `cargo xtask verify` PASS.

#### M7 결과와 정한 값 (2026-10-04)
- 판정: 데이터(`dump --check`, `gen --check`, 같은 Bedrock 상태 값 충돌 0, 결정적 드롭 34,786 골든이 규칙 코드와 같음), 조명(무작위 변경 1만 번 = 전수 계산, 경계·상한·넘침 수렴, PMMP 값, `lighting = false`), 블록 갱신(랜덤 틱 결정성, 성장 확률 PMMP 식 통계, 예약 갱신 저장·재시작, 액체 5장면 PMMP와 같음), 인벤토리·보존(무작위 요청 10만 개, 두 번 = 한 번, 중간 닫기·떠남·죽음 반환, 커서·제작 칸·창 칸 저장), 제작과 작업대(모든 제작대 레시피 거울·밀림·부족, 화로·모닥불·양조·인챈트·모루·대장장이·석재 절단기·베틀·신호기 규칙 표·보존·멱등·중간 끊김), 거래(8.6 원칙 시험), 엔티티(물리 결정성, 줍기·합치기 보존, 상한 거부, 1만 개 생성·제거 뒤 표 시작 값), 자동 시나리오(원목에서 거래까지 패킷만으로), 강제 종료(블록 50회 + 아이템 흐름 50회: 무결성, 총량 같음, 커밋된 틱), 결정성(스크립트 클라이언트 M7 입력, 풀 1개와 max(L,4)개 틱별 해시·출력 같음, 재생 같음, 조명 포함), 누수(규칙 100회 순환 뒤 모든 표), 성능 실측, P6(아래) 모두 통과. `cargo xtask verify` PASS.
- 검증 필요 처리 결과(10.7 처음 넷): 8.6 거래 다섯 개는 P6로 닫음(아래 거래), 10.1 Event 종류 8·9·10·12·14·20·21·22는 BDS가 보내지 않음 → Anchor도 보내지 않음, 10.4 레시피 책·블록 아이템 형태는 BDS가 클라이언트 아이템 형태(결과 항목, 손 아이템 블록 ID)를 검사하지 않음 → Anchor도 서버 칸과 스택 ID로만 판정(10.4의 "형태가 다르면 None"은 요청 처리에 쓰지 않음), 12절 7번 `Vec3`는 f64.
- 일 나누기 결과: 단계 A(D 데이터, L 조명, S 저장 형식, K 프로토콜 상수, P P6 조사), 단계 B(I 인벤토리, E 엔티티, B 상호작용, R 블록 갱신, H 생존), 단계 C(C1 작업대 계열, C2 엔티티 확장, C3 거래, C4 생존·월드 통합, C5 P6 하네스, W 연결, T1 시험, F1 수정·시나리오, F2 P6·성능, G P6 맞춤). 약속은 메인이 스텁으로 먼저 만들었고(`tick/rules.rs`, 모듈 스텁), 합칠 때 겹친 공유 파일은 3방향 병합 뒤 손으로 풀었다. worktree와 브랜치는 모두 지웠다.
- 구조(정본은 코드):
  - 공통: `anchor::math`(`Vec3`, `Aabb`, 결정적 `min`·`max`·`clamp`·`sin`·`cos`·`atan2`, fdlibm 계수를 비트로), `random`(카운터 방식, `tag`·`key`). 틱 규칙 모듈 `tick/{inventory,block_entity,entity,interact,update,survival,trade}`와 `tick/rules.rs`(청크 소유 모듈의 load·unload·take_changed를 extras 보관소와 잇는 곳). `Player::send`(outbox, 틱당 4,096, 넘치면 끊음, 출력 단계가 보냄), `State::broadcast`, 런타임 ID 색인. 상태 해시는 모든 구조체를 `..` 없이 분해한다(빠지면 컴파일 실패, 뺀 필드마다 이유 주석).
  - 패킷 분기: PlayerAuthInput은 PMMP 순서(이동 → 블록 액션·아이템 상호작용 → 스택 요청), 나머지는 소유 모듈로. 틱 순서: 입력 → 작업 → 생존 → 부수기 → 블록 갱신 → 엔티티 → 블록 엔티티 → 거래 → 조명(월드당 16,384단위) → 넘친 outbox 정리 → 뷰 → 출력.
- 데이터(D): 블록 상태마다 PMMP 타입(`block::Kind` 799), 클래스 261·트레이트 24·타입 태그 9, 부수기 정보(도구 호환·부수기 시간 PMMP, 칼 ×1.5, 예외 3), 불 값, 물리 플래그, 마찰, 6면 지지, 충돌 상자, `with`(속성 한 칸), 아이템 형태 역조회. 아이템 종류 323·클래스 112: 스택, 내구도, 도구·등급 7, 갑옷, 음식·효과·남는 것, 연료, 인챈트 허용 집합, 쿨다운, 던지는 힘, 물약, 태그 76. `effect` 모듈: 효과 27, 물약 43, 인챈트 23(레벨별 인챈트 힘). 공유 상태는 `gen/data.rs` SHARED(배너·벽 배너·침대·물 가마솥)만 허용하고 그 밖의 불일치는 gen 실패. 결정적 드롭은 4,096 시드에서 같으면 결정적. PMMP 결함: 벽걸이 간판 충돌 상자 뒤집힘(버림, 검증 필요(M9) BDS 상자), 장식 무늬·재료 없음(TrimData는 BDS·`crafting::TRIM_*`).
- 프로토콜 상수(K): IR `const_decls`(gophertunnel protocol·packet 패키지 export 상수 3,132개), `protocol::consts`(enum 175, 상수 7, 제외 334는 이유 표), `protocol::flags::FlagSet`. 검증 필요(M8): ParticleType 9 이후 번호(gophertunnel이 PMMP와 다름, 입자를 보낼 때 BDS 대조), `UnWaxOxidised` 0xfa0a, `EntityDataFlagCount` 131.
- 저장(S, C4): 형식 3. 레코드 디코드가 버전을 받는다(형식 1·2 읽음). 플레이어 = 이름 + 위치 f64 + extra NBT(모듈마다 자기 키, Bedrock·PMMP 이름). 청크 종류 1 블록 엔티티, 2 엔티티, 3 예약 갱신(각 NBT 목록, 98,304개 상한, 빈 목록은 삭제), 지형과 한 트랜잭션으로 로드·커밋, 하나라도 손상이면 청크 격리. 월드 레코드에 시간(형식 3). 입력 로그 형식 4(청크 답 CRC-32C, 플레이어 extra, 스킨). 플레이어가 슬롯 밖에 든 것(커서, 제작 칸, 창 칸, 떨어뜨리지 못한 것)은 `PendingItems`로 커밋에 든다. 대기 스택 + 커서·제작 칸·창 칸 스택 ≤ 78을 어길 요청은 거부한다(`Unreturnable`, 증명은 `MAX_SAVED_PENDING` 문서).
- 조명(L): 4비트 블록·하늘빛을 블록 저장과 따로(스냅샷 복사 없음, 저장 안 함), 로드·생성 때 저장 스레드 풀에서 계산, 경계는 틱에서 양방향, 내린 청크가 준 빛은 거둠(로드 안 된 청크 = 어둠). 큐 상한(변경 16,384, 재계산 65,536, 제거·전파 262,144, 넘치면 그 청크만 다시 계산). 시간 보정 23칸 표. PMMP 결함 7개를 고침(상한 없음, 경계 전파 없음, 계산 중 변경 버림, 약한 광원까지 어둡게, 빛 1 뒤를 안 봄, 내린 청크의 빛이 남음, 플랫폼 `cos`). 실측: 평지 청크 0.1 ms, 빽빽한 청크 4.0~4.7 ms, 횃불 108~117 µs.
- 블록 갱신(R, C4): 예약 갱신(위치마다 하나, 월드당 틱 4,096, 저장), 이웃 갱신(변경 피드, 바뀐 블록 + 6이웃, 큐 2^18, 틱당 16,384), 랜덤 틱(6절), 모든 PMMP 클래스의 랜덤·예약·이웃·뼛가루 규칙, 지지 상실, 낙하 블록, 나무 생성기 전부, 불(TNT 점화 80틱), 액체(PMMP와 같은 장면), 구리 산화(dragonfly). 바뀐 블록이 담을 수 없는 블록 엔티티는 지우고 내용을 떨어뜨린다(PMMP `Tile::close`는 버림 → 고침), 로드한 청크의 맞지 않는 블록 엔티티도 같다. 물 잠김은 PMMP에 없어 없다. PMMP 결함: 다시 예약 비교, 예약 저장 안 함, 경계의 로드 안 된 이웃을 공기로 봄, 거대 균류 덩굴 y 0. 그대로 둔 PMMP 빈틈: 짙은 참나무 묘목, 덩굴 성장, 비.
- 상호작용(B, C1, W): 서버 권위 부수기(PMMP 속도, 부수기 시간을 강제(PMMP 5는 안 봄), PlayerAuthInput마다 진행·틱 수+20 상한·1틱 허용, 모르는 액션·면이면 그 패킷 블록 액션 버림, gophertunnel 형식 StopBreak), PMMP 놓기 전부(PMMP 실행 결과 202,358개 대조, `xtask/php/place.php`), 쓰기(문·버튼·레버 등 상태만, 컨테이너, 침대, 가마솥, 도구 변환, 양동이, 뼛가루 2005, 케이크, 쿨다운), 엔티티 훅(던지기·활·TNT·공격·상호작용, 놓을 자리 엔티티 검사). 거부하면 늘 클릭·대상 블록을 다시 보낸다. 첫 드롭 엔티티를 못 만들면 부수기를 거부한다. 폭발은 블록마다 입자를 보내지 않는다.
- 인벤토리(I, C1): Stack 변환(PMMP TypeConverter, 정규형만 필드, 나머지 NBT 보존), 창 ID 2부터, 자기 인벤토리 255, 닫기 답 247, 실행기는 덮개에 적용 후 전부 통과해야 커밋, 수동 제작은 정확한 모양·거울(PMMP 다중집합만 → 고침), 재료 매칭은 이분 매칭(PMMP 탐욕 → 고침), 레시피 책은 클라이언트 재료 무시, 요청당 액션 60·플레이어 틱당 요청 80·요청 ID 256개 기억·요청당 드롭 1. 스택 네트워크 ID는 플레이어마다 증가(PMMP; 요청 안 규칙과 컨테이너 틱의 개수 변화 유지는 BDS). 블록 엔티티: 상자·겹상자·덫 상자·통·호퍼·엔더 상자·셜커·작업대, 화로 계열(PMMP 시간, ContainerSetData 0~3, 경험치는 dragonfly 값으로 쌓아 꺼낼 때 구슬: PMMP에 없음), 모닥불, 양조기, 인챈트대(PMMP 규칙, 선택지 ID는 CraftingData 다음부터, BDS 모양), 모루(dragonfly, 호환 안 되는 인챈트만이면 거부), 대장장이, 석재 절단기, 베틀(PMMP에 아이템 없는 무늬 제외), 신호기, 간판·화분·액자·주크박스(상태만)·독서대·조각된 책장·머리·침대 색·배너. UnlockedRecipes는 보내지 않는다(PMMP, 모든 레시피 항상 해금). 숫돌은 PMMP에 없다.
- 엔티티(E, C2): 저장소 하나(Id 순, 세대), 월드당 16,384(넘으면 만들지 않고 호출자가 아이템을 지님), 물리 PMMP(풀에서 계산, Id 순 적용), tick-radius 안만 틱, 떨어진 아이템(지연 10, 합치기, 6,000틱, 부분 줍기는 같은 엔티티로: PMMP 결함 고침), 경험치 구슬(수선, 쿨다운), 낙하 블록, 발사체(눈덩이·달걀·엔더 진주·경험치 병·투척·잔류 물약·화살: PMMP 치명 화살 결함 고침), TNT·폭발(PMMP 광선, 크기 8, 틱당 TNT 64·블록 8,192, 로드 안 된 청크에서 멈춤), NPC(주민 `villager_v2`·사람, 기본 무적, 저장된 `Invulnerable`로 끔), 플레이어끼리 보기(AddPlayer BDS 값, 다른 플레이어는 MovePlayer). 런타임 ID는 저장하지 않고 로드 뒤 다음 엔티티 틱에 준다.
- 생존(H, C4, W): 스폰 창 BDS 순서·값(크기까지 시험), PlayerList(로그인 스킨 그대로, 4 MiB 상한), 이동 검사(15블록, 스폰 전 무시), 피해·전투·넉백·치명·쿨다운, 죽음·리스폰(Respawn 셋, 다른 월드), 배고픔(포화 시작 5: BDS), 경험치(PMMP 식, 진행은 선형으로 고침), 효과, 먹기(32틱의 3/4), 토템 양손, 약, 시계(SyncWorldClocks 256틱마다, 월드 시간 저장), 수면(피해로 깸), 하드코어 죽음은 관전자, 게임 규칙 값은 실제 동작. PMMP 결함: 속도 속성 곱 떠돎, 레벨 진행, 한 명 깨면 모두 취소, 가시 무한 반사, 시프트 넘침, 스킨 교체·무작위 fullSkinId.
- 거래(C3): 8.6 그대로. 오퍼 NBT는 BDS 키, 상한 128, 못 쓰는 저장 오퍼는 그대로 다시 씀. netId는 CraftingData 다음부터 서버 카운터, 처음 열 때 매기고 다시 열어도 유지. UpdateTrade 재전송은 등급이 오를 때만. 반복 256, 보상 구슬 거래마다 3~6. Damage 32767은 아무 데미지.
- 바뀐 결정(10.7 위): 저장 형식은 3까지(월드 시간), 로그 형식 4. 결정성 시험은 시드 4개(스크립트 클라이언트로 틱마다 더 많은 입력, 약 86초). 강제 종료 아이템 흐름은 공개 Context에 상자·엔티티가 없어 크레이트 안 시험이다(M8에서 공개 시험으로 옮길 수 있음). 이벤트 훅 자리는 규칙 함수의 검사·적용 경계다(M8).
- P6(BDS 1.26.52.3, `p6 bds zlib snappy`, `p6 anchor zlib snappy`): 스폰 창·big-skin·blocks·nethernet(zlib·snappy)·fallback과 M7 시나리오 craft·furnace·enchant·crops·trade가 모두 BDS와 같다. P6로 맞춘 BDS 값: 창을 열 때 BlockActorData 없음, 컨테이너 자기 틱에서 개수만 바뀐 칸은 스택 ID 유지, 떨어진 아이템 위치 = 발 + 0.125·MoveActorDelta `Ticks` 0·움직이는 틱마다(PMMP 0.01 문턱 버림), 주민 메타데이터 101~104(SkinId는 BDS가 무작위라 0 고정)와 거래 뒤 바뀐 키만 SetActorData, 화로 `LastFuel`에 `WasPickedUp`, 인챈트 선택지 `Slot` = 아이템 플래그(곡괭이 1024)·[착용, 손, 아이템 자신] 세 목록(규칙은 `xtask/data/p6/m7-facts.md` 4절), 거래 지불 Damage 32767 = 아무 데미지(저장·전송 그대로). 사실 전체는 `xtask/data/p6/m7-facts.md`. 유예표의 M7 줄은 모두 지우거나 `-`로 옮겼다. Event 줄은 보이기만(Anchor는 보내지 않음, 통계용).
- 성능(release, 28스레드, 시야 8, 300틱 평균/p99): 1명 0.014/0.25 ms, 20명 0.20/0.66, 100명 1.77/3.95(플레이어끼리 추적이 대부분), 아이템 1,000 0.17/0.32, 10,000 2.2~2.5/2.9~3.8, 작물 20,736 랜덤 틱 0.023, 블록 1,000 변경 조명 켬 변경 틱 1.97·48틱에 끝, 끔 1.13. TNT 100 연쇄 최악 1.66 ms. 재실행: `cargo test --release --lib tick::tests::bench -- --ignored --nocapture`.
- M9 공기(사용자 실제 접속 보고, P6 craft에 물속 단계): 물속 첫 틱에 Breathing을 끄고 공기 −1/틱, −20이면 0으로 하고 익사 2. 물 밖이면 바로 Breathing을 켜고 공기 +4/틱으로 최대까지(BDS). 스폰 창 데이터에는 Breathing이 없고 Initialised 뒤 첫 틱에 키 0만 보낸다. 플래그 단어는 바뀐 키만(0/92). PMMP와 다름(+5, 다 찰 때까지 숨 못 쉼) → BDS를 따름. 원인은 플레이어 Breathing 플래그를 한 번도 안 보낸 것.
- 남은 위험과 검증 필요:
  - 검증 필요(M9, P6): 갑옷·책의 인챈트 `Slot` 값, MovePlayer 대 MoveActorDelta(봇 둘 필요), 등급 상승 때 UpdateTrade 재전송, demand≠0 가격 표시, 신호기·모루·대장장이 요청 모양, 메타데이터 키 136~140 타입, 사람 NPC AddPlayer 필드, 구름 메타데이터, 다른 플레이어 AddPlayer 72키.
  - 공식 클라이언트만 볼 수 있는 것: 레시피 책 재료 목록, PMMP 모양과 다른 값을 보내는 곳(없도록 맞춤).
  - 로드 때 지운 화로 블록 엔티티에 쌓인 경험치는 사라진다(아이템은 대기 드롭). 셜커가 폭발 1/size 판정에 걸리면 내용이 사라진다(PMMP 규칙). 변경 피드가 가득하면 블록 엔티티 제거가 늦다.
  - 4스레드 기계 성능은 따로 재야 한다. 결정성 시험이 디버그에서 약 86초다.

#### M7 진행 상태
- 끝남(2026-10-04): 단계 A·B·C 모두 합침, worktree·브랜치 정리, verify PASS. M7 뒤 사용자 지시(커밋, M8 백그라운드 세션)는 진행했다(13절).

### 10.8 M8 세부 설계 (2026-10-04)
- 검증 필요 처리(시작 때 확인, 결과는 "M8 결과"에 적는다):
  - 10.7 K: ParticleType 9 이후 번호, `UnWaxOxidised` 0xfa0a, `EntityDataFlagCount` 131 → `particle` 명령과 입자 API를 만들 때 BDS(`/particle`)와 대조한다.
  - 10.6: 스폰 이동 API → `World::set_spawn`이 붙든 스폰 청크를 옮긴다(PMMP `setSpawnLocation`은 옮기지 않음 → 고침).
  - 10.2: 리소스팩 전송이 있을 때 스폰 마감 60초 → 팩 전송 시간은 마감에서 뺀다(팩 크기 상한과 전송 속도 상한으로 끝이 있음).
  - 10.3: dimpl, sctp-proto의 `log` 기록 → `Server` rustdoc에 "`log` 다리는 바이너리가 정한다"를 적는다.
  - 10.4: 블록 해시 함수 → 커스텀 블록이 생기므로 런타임 코드로 옮긴다(골든 시험은 그대로).
  - 유예표 M8 줄: AvailableCommands, SetCommandsEnabled는 명령으로 보낸다. CameraPresets·AimAssist·Spline은 PMMP에 카메라 API가 없어 `-`로 옮기되, BDS 값을 보내는 편이 같으면 보낸다(P6로 정함). → 2026-10-04 바꿈: BDS 값으로 보낸다(10.6 스폰 창 원칙).
- 대응표(판정의 중심):
  - 원본: `xtask/php/api.php`가 PMMP 6a7cc02(`src/`와 `generated/`)를 PHP 리플렉션으로 읽어 `xtask/data/pmmp/api.json`을 만든다(class-like 1,510, 선언된 public 메서드 9,531, 실행 1초, 두 번 실행 바이트 같음). `@internal` 클래스와 메서드는 행이 아니다. 행 = 비internal class-like 하나 + 그 클래스가 선언한 비internal public 메서드 하나씩.
  - 상태: 구현함(코드에 `#[doc(alias)]`), 다름(alias + 상태 파일의 이유), 병합함(상태 파일의 대상 alias, 대상의 위치·시험을 물려받음), 안 함(상태 파일의 이유). 상태 파일 `xtask/data/pmmp/status.txt`는 줄마다 `<패턴> <상태> <대상|이유>`이고, 패턴은 FQN, 클래스 접두(`pocketmine\plugin\**`), 메서드 글롭(`*::get*`)이다. 위에서 처음 맞는 줄이 이긴다. alias가 있으면 구현함이 먼저다.
  - 위치·시험: 위치는 alias가 붙은 항목의 `파일:줄`. 시험은 시험 함수 바로 위 `// pmmp: A::b, C::d` 주석이 덮는 alias다(src와 tests 모두 읽음). 구현함·다름·병합함 행에 시험이 없거나, 어떤 상태도 없거나, alias가 api.json에 없거나 겹치면 빈칸이다.
  - alias 이름은 `짧은클래스::메서드`, 짧은 이름이 겹치는 47개는 네임스페이스로 구별한다(형식은 표 도구가 정하고 rustdoc이 받는지 확인).
  - 생성: `cargo xtask table [--check]`가 `xtask/data/pmmp/table.md`(행마다 FQN, 상태, 위치, 시험, 이유)를 쓴다. verify가 `--check`(최신 여부와 빈칸 0)를 돌린다. 안 함 행 수를 표 머리와 보고에 적는다.
- 이벤트(8.2를 구체화):
  - 등록: `ServerBuilder::on::<E>(Priority, |e: &mut E, ctx: &mut Context| ..)`, `ServerBuilder::monitor::<E>(|e: &E, ctx: &Context| ..)`. 모니터는 적용 뒤에 부르고 `&Context`라 바꿀 수 없다(PMMP MONITOR를 타입으로).
  - 부르는 법(`tick/dispatch.rs`): 규칙 코드가 검사 → 이벤트 값 → 리스너 → 다시 검사 → 적용 → 모니터. 리스너가 상태를 바꿔 다시 검사가 실패하면 취소로 본다(PMMP는 리스너가 바꾼 상태를 다시 보지 않고 적용해 복제·소실이 생김 → 고침). 리스너도 모니터도 없으면 이벤트를 만들지 않고 다시 검사도 하지 않는다.
  - 재진입 없음: 리스너 안의 직접 API 호출은 이벤트를 쏘지 않는다. 그 결과로 생기는 일(예: `kick` → 떠남)은 틱의 정해진 자리에서 처리되고 거기서 이벤트가 난다. `ctx.defer(f)`는 지금 콜백이 끝난 뒤 같은 틱에 돈다(틱당 4,096개, 넘치면 `Err`). `ctx.perform(action)`은 게임 규칙을 거치는 행동(이벤트 포함)을 defer로 넣는다. 행동 타입은 `action` 모듈의 봉인된 트레이트 구현이다.
  - 값: 이벤트는 스택 값, 필드는 `pub(crate)`(규칙 코드가 구조체 식으로 만듦), 공개는 게터·세터·`cancel`. 플레이어·엔티티·월드는 `Copy` Id로 담는다. 이름은 PMMP에서 `Event`를 뗀 것이고, 하위 클래스 계층은 한 구조체 + 원인 enum으로 합친다(예: `EntityDamageByEntity` → `EntityDamage`의 `Source`).
  - 목록: PMMP 구체 이벤트 128개 중 범위 안을 모두 가져온다. 뺌: 플러그인(PluginEnable·Disable), PHP·운영 내부(LowMemory, NetworkInterface* 3개, QueryRegenerate, UpdateNotify, PlayerCreation), 레드스톤(PressurePlateUpdate), M7에 동작이 없는 것(동작이 생기는 곳에서 가져옴). 병합: PlayerPreLogin → PlayerLogin(틱에서 한 번, 킥 이유 목록), DataPacketReceive·Send·Decode → 패킷 훅, ChunkPopulate → ChunkLoad(`is_new`), EntityEffect → EntityEffectAdd·Remove, EntityCombustBy* → EntityCombust. 더함: `TradeOpen`, `Trade`(8.6). WorldInit·Load·Unload는 월드가 빌드 때 고정이라 안 함.
  - 파일: `event.rs`(우선순위, 트레이트, 목록 구조, 리스너 표 매크로)와 `event/{block,entity,inventory,player,server,world}.rs`(PMMP 네임스페이스와 같음). 모두 `anchor::event::*`로 평평하게 공개한다.
- 플레이어·엔티티·서버 API(PMMP Player·Human·Entity·Server를 구체화):
  - 읽기는 `ctx.player(id) -> Option<&Player>`, 쓰기는 `ctx.player_mut(id) -> Option<&mut Player>`. `Player`는 틱 상태의 플레이어 구조체 그 자체(필드 비공개)이고 `anchor::player`에서 공개한다. 게터는 `&T`·`Copy`다.
  - 다른 플레이어나 월드에 닿는 변경(텔레포트, 게임 모드, 이름표, 스킨, 킥)은 플레이어에 표시하고, 틱의 정해진 자리(입력·작업·규칙 뒤, 뷰 전)에서 보는 사람에게 보낸다. 값은 바로 바뀐다(`teleport` 뒤 `position()`이 새 값).
  - 메시지 계열은 `Player::send(Message)` 하나다(`sendMessage`, `sendTip`, `sendPopup`, `sendJukeboxPopup`, `sendTranslation`, `sendActionBarMessage`, `sendTitle`·`sendSubTitle`, `sendToastNotification` → 병합). 원시 패킷은 `Player::send_packet(Packet)`(PMMP `NetworkSession::sendDataPacket`).
  - 같은 모양으로 `ctx.entity(id)`, `entity_mut`, `world(id)`, `world_mut`, `worlds()`, `players()`(Id 순). `player::Id`가 `PlayerId`를 대신하고 `world::Id`를 더한다.
- 명령과 권한:
  - `ServerBuilder::command(Command::new(이름, 설명).permission(..).alias(..).overload(&[Param..], 처리기))`. 인자 타입은 Bedrock AvailableCommands 타입(정수, 실수, 문자열, 대상, 좌표, 메시지, enum, 블록·아이템 이름 enum 등)이고, 처리기는 `FnMut(&mut Context, &Sender, &Args) -> Result<(), command::Error>`다. 자동완성은 AvailableCommands로 보낸다(권한 없는 명령은 그 플레이어에게 보내지 않음).
  - 입력: CommandRequest(플레이어), 표준 입력 줄(`console-input`, 틱 밖 스레드가 상한 큐로 넣음, M9 콘솔 일부를 당겨옴). 결과는 CommandOutput(BDS 형식, P6)이고, 콘솔 출력은 tracing info로 낸다.
  - PMMP 기본 명령 42개 중 범위 안은 Anchor 기본 명령으로 넣는다(같은 이름을 사용자가 등록하면 사용자 것이 이김, `ServerBuilder::without_command`). 뺌: dumpmemory, gc, plugins, timings(PHP·플러그인). status는 TPS·MSPT만.
  - 권한: `Permission::new(이름, 기본값)` 상수를 빌드 때 등록, 기본값은 PMMP와 같은 op·notop·true·false. op·밴(이름, IP)·화이트리스트는 `world.redb` 표에 두고(10.6에서 M9로 둔 것을 M8로 당김: 명령과 로그인 정책이 쓴다), 바뀌면 다음 틱 경계에 커밋을 요청한다(6절). `white-list` 설정을 실제로 쓴다. 실행 중 권한 덧붙임(PMMP PermissionAttachment)은 저장하지 않는다.
- 폼·메뉴 창·표시:
  - 폼: `Player::send_form(Form, 처리기)`. 모달·메뉴·사용자 정의(라벨, 입력, 토글, 슬라이더, 드롭다운, 단계 슬라이더). 답 JSON은 64 KiB 상한, 보낸 폼의 모양으로 검사하고 맞지 않으면 버린다. 플레이어당 대기 폼 16개, 폼 ID는 플레이어마다 늘어나는 번호, 같은 ID의 답은 한 번만.
  - 메뉴 창(8.3, InvMenu식): 상자·겹상자·호퍼 모양의 가짜 컨테이너, 칸 클릭 처리기, 아이템은 서버 것이라 클라이언트가 가져가지 못함. 가짜 블록 위에 연다.
  - 가짜 블록: 플레이어마다 위치별 덮어쓰기(상한 4,096), 청크·블록 갱신 때 다시 적용.
  - 스코어보드 사이드바·목록, 보스바, 타이틀(메시지 계열), 토스트.
- 확장 지점(8.3의 나머지):
  - 패킷 훅: 수신 훅(입력 단계 직렬, 패킷을 바꾸거나 버림), 송신 훅(출력 단계 병렬, `Fn + Sync`, 게임 상태에 닿지 못함, 패킷을 바꾸거나 버림). 미리 인코드한 공용 스폰 배치는 훅을 거치지 않는다(rustdoc).
  - 시작 게임 설정 훅(StartGame 서식에 한 번), 리소스팩(설정 `resource-packs`, `.mcpack`·`.zip`, 키 파일, 강제, 조각 전송, 팩 크기와 개수 상한. zip은 직접 읽음: 상한 있는 중앙 디렉터리와 miniz_oxide 해제).
  - 예약·비동기: `ctx.schedule(지연, f)`, `schedule_repeating`, 취소 핸들(PMMP TaskHandler). `ctx.spawn_blocking(일, 끝 처리기)`: 일은 블로킹 풀(상한)에서, 결과는 다음 틱 입력이다. 결과 타입은 serde 직렬화가 되어야 하고 입력 로그에 기록된다(로그 형식 5, 1 MiB 상한). 재생은 일을 다시 돌리지 않고 기록된 결과를 넣는다.
  - 사용자 데이터: 플레이어별 비영속 Extensions(타입 키), 영속 플레이어 데이터와 서버 전역 영속 값은 빌드 때 등록한 serde 타입이고 같은 DB 커밋에 든다(저장 형식 4: 플레이어 레코드에 사용자 값, `user` 표).
  - 커스텀 블록·아이템·엔티티: 정의(식별자, 속성, 최소 컴포넌트: 이름·아이콘·최대 스택·단단함·드롭·충돌 상자·지오메트리·재질), 동작 트레이트(사용, 놓기, 부수기, 틱). 엔진이 StartGame 블록 속성, ItemRegistry 컴포넌트, AvailableActorIdentifiers, 블록 해시 ID를 채운다. 텍스처는 리소스팩이 준다.
  - 레시피 등록(모양·무형·화로, CraftingData에 포함), 주민 거래 API(NBT 대신 타입 있는 `Offer`).
- 예제 4개(`examples/`, 각각 `anchor.toml`을 읽음): `lobby`(폼 메뉴, 명령, 스코어보드, 보스바, 메뉴 창, 다른 월드로 이동), `pvp`(키트 명령, 피해·죽음 이벤트, 킬 수 영속 데이터, 권한), `custom`(커스텀 블록·아이템·엔티티, 리소스팩, 레시피, 주민 상점), `minigame`(임시 월드, 예약 작업 카운트다운, 타이틀, 구역 밖 부수기 취소, 비동기 작업, 패킷 훅). 시험은 xtask가 예제 바이너리를 임시 폴더에서 띄우고 Go 봇 시나리오(채팅, 명령, 폼 답, 메뉴 클릭, 스코어보드·보스바·타이틀 수신)를 돌린다(`xtask/tests/examples.rs`, verify의 `cargo test`에서 돈다).
- P6(BDS): 명령(AvailableCommands 모양, CommandRequest에 대한 CommandOutput 고정 필드, 공통 명령 say·give·tp·gamemode·time·effect·xp). 폼은 BDS 스크립트 팩(`@minecraft/server-ui`)으로 보낼 수 있으면 ModalFormRequest JSON 모양을 대조하고, 못 하면 "검증 필요(M9)"로 둔다. 입자 번호(위 검증 필요).
- 일 나누기(메인이 약속을 먼저 만든다: lib.rs 모듈, event.rs 틀, `tick/dispatch.rs`, `Player` 공개, `send` → `send_packet`, tick.rs 새 패킷 분기와 스텁):
  - 단계 A: A1 플레이어·엔티티·서버 API와 메시지, A2 명령·권한·op·밴·화이트리스트 저장, A3 리소스팩·시작 게임 훅·패킷 훅, A4 이벤트 타입 전부, A5 대응표 도구.
  - 단계 B(이벤트 연결, 틱 모듈마다 하나): B1 interact, B2 inventory·block_entity·trade(+메뉴 창), B3 survival, B4 entity, B5 update·spawn·세션(로그인·채팅·명령 이벤트).
  - 단계 C: C1 기본 명령, C2 폼·스코어보드·보스바·가짜 블록, C3 예약·비동기·사용자 데이터, C4 커스텀 콘텐츠·레시피·거래 API, C5 봇 기능과 P6.
  - 단계 D: 예제 4개와 봇 시나리오, 대응표 상태 파일과 시험 주석 채우기, 전체 verify.

#### M8 판정식
- 대응표: `cargo xtask table --check` 빈칸 0, 구현함·다름·병합함 행마다 위치와 시험이 있다. 안 함 행 수를 보고한다.
- 이벤트: 가져온 이벤트마다 시험 하나 이상(리스너가 불림, 취소하면 상태가 그대로, 필드를 바꾸면 결과에 반영, 모니터는 적용 뒤 상태를 봄). 리스너가 상태를 바꿔 검사가 깨지면 취소(보존 시험: 무작위 리스너가 상태를 바꿔도 아이템 총량 그대로). 리스너 없는 틱의 상태 해시와 출력이 M7과 같음. 재진입 없음(직접 호출이 이벤트를 쏘지 않음), defer 상한.
- 명령: 파싱(모든 인자 타입, 잘못된 인자, 권한 없음), AvailableCommands가 권한별, 콘솔 입력, 기본 명령마다 시험. op·밴·화이트리스트가 저장·재시작 뒤 유지되고 로그인 정책이 쓴다.
- 폼·메뉴: 잘못된 답(모양, 범위, 크기, 모르는 ID, 두 번)이 버려지고 처리기는 한 번만, 메뉴 창에서 아이템 복제 0.
- 비동기·예약: 결과가 입력 로그에 들고 재생이 같은 해시, 취소한 작업은 돌지 않음, 반복 뒤 표·큐가 시작 값(누수).
- 사용자 데이터: 영속 값이 강제 종료 뒤 커밋된 틱 값(복제·소실 0), 손상 값은 원본 보존.
- 커스텀 콘텐츠·리소스팩: 봇이 팩을 받고 커스텀 블록을 놓고 부수고 커스텀 아이템을 씀. 팩 상한, 잘못된 zip에서 패닉 0(변형 입력).
- 예제: 네 예제가 Anchor를 고치지 않고(공개 API만) 빌드되고 봇 시나리오를 통과한다.
- P6: 명령 시나리오가 BDS와 같은 정해진 필드, 유예표 M8 줄 0, 검증 필요 결과를 적는다.
- 결정성·성능: M7 결정성 시험이 리스너·작업이 있는 스크립트로도 스레드 1과 N에서 같다. 리스너가 있을 때의 틱 비용을 실측해 적는다.
- `cargo xtask verify` PASS.

#### M8 결과와 정한 값 (2026-10-04)
- 판정: 대응표 빈칸 0(`cargo xtask table --check`, 10,217행: 구현 1,663, 병합 3,525, 안 함 5,029, 다름 0), 이벤트(가져온 이벤트마다 리스너·취소·필드·모니터·다시 검사 시험, 영역마다 무작위 리스너 보존 시험), 재진입 없음·defer 상한, 리스너 없음 = 빈 리스너(해시·출력·로그 바이트), 명령(인자 형 전부, 권한별 AvailableCommands, 콘솔, 기본 명령 37개), op·밴·화이트리스트 저장·재시작, 폼·메뉴(잘못된 답 버림, 처리기 한 번, 메뉴에서 복제 0), 예약·비동기(로그 재생 같음, 누수 0), 사용자 데이터(강제 종료 뒤 커밋된 틱 값), 커스텀 콘텐츠(봇이 팩을 받고 블록·아이템·엔티티를 씀, 저장·재시작 뒤 유지), 예제 4개(Anchor를 고치지 않고 봇 시나리오 통과), 결정성(바쁜 리스너·작업·폼·작업 결과로 스레드 1과 N 같음, 재생 같음), P6(아래), 성능(아래), 체크리스트 감사 지적 수정(K1~K3). `cargo xtask verify` PASS(마지막 실행 8분 56초).
- 검증 필요 처리(10.8 처음): 입자 번호 → PMMP 번호가 BDS와 맞음(14 아이템 깨짐, 15 눈덩이; 19 이상은 검증 필요(M9)), `UnWaxOxidised`·`EntityDataFlagCount`는 이번 기능에서 쓰지 않아 그대로 둠(검증 필요(M9)), 스폰 이동 → `set_spawn`이 붙든 스폰 청크를 옮김, 팩 전송 → 스폰 마감에 30초 + 조각마다 2초를 더함, `log` 다리 → `Server` rustdoc, 블록 해시 → 런타임 `block/hash.rs`, 유예표 M8 줄 → AvailableCommands·SetCommandsEnabled는 보냄, Camera 세 줄은 `-`(PMMP에 카메라 API 없음).
- 구조(정본은 코드):
  - 이벤트: `event.rs`(`listeners!` 표, `List`(리스너 + 모니터, `Option<Box<..>>`), 봉인 `Event`)와 `event/{block,entity,inventory,player,server,world}.rs` 93개(발생하지 않던 4개는 감사 뒤 뺌), `tick/dispatch.rs`(`heard`·`listened`·`call`·`watch`, `defer` 4,096·틱당 실행 16,384, `perform`), `action`(봉인 `Action`: `BreakBlock`·`UseItemOn`·`UseItem`·`AttackBlock`·`InteractEntity`·`Damage`·`SetGameMode`·`Chat`·`Kick`·`Transfer`·`Attack`·`Explode`). 규칙은 검사 → 이벤트 → 리스너 → 다시 검사(실패면 취소) → 적용 → 모니터. EntitySpawn·ItemSpawn은 엔티티 틱이 알림(다른 규칙 중간과 직접 API에서 리스너가 돌지 않게).
  - 공개 모듈: `player`(`Player`, `Id`, `Message`), `entity`(`Entity`, `Mut`, `Custom`, `Behaviour`, `Animation`, `PaintingMotive`), `world`(`Id`, `Sound`, `Particle`), `command`, `permission`, `form`, `display`(스코어보드·보스바), `menu`, `inventory`, `tile`, `custom`(아이템·블록·레시피·`Offer`), `task`, `data`, `pack`, `action`. 쓰기는 직접 API(이벤트 없음)이고 다른 플레이어에게 보일 변화는 표시해 두었다가 `player::sync`가 보낸다.
  - 설정·저장: op·밴·화이트리스트는 redb `access` 표, 서버 사용자 값은 `user` 표, 플레이어 사용자 값은 extra NBT `anchor:data`(저장 형식 3 그대로, 표만 더함). 입력 로그 형식 6(주소, 콘솔 줄, 작업 결과).
- 바뀐 결정(10.8 위에서): 명령 처리기는 `Result<Output, Error>`, 폼·스코어보드·보스바·가짜 블록·메뉴는 `Player`가 아니라 `Context` 메서드(처리기가 `&mut Context`를 받고 표가 `State`에 있음), 사용자 데이터 저장 형식은 4로 올리지 않음(표와 키만 더함), op·밴·화이트리스트와 콘솔 입력을 M9에서 M8로 당김, 커스텀 블록·아이템은 `&'static` 정의를 값에 담는 표현(전역 레지스트리 금지: 4.1), 기본 입장·퇴장 메시지는 BDS처럼 다른 플레이어에게만, 채팅 메아리는 BDS 모양(리스너 형식이 있으면 그 글), 청크 해제는 늘 뷰 단계, PlayerDataSave 취소는 무시(원자 저장), PlayerResourcePackOffer는 `resource_pack`에 병합(팩 교환은 틱 전 세션). 12절 3번(PMMP와 다른 이벤트 의미)은 8.2·10.8대로 정함.
- PMMP를 고쳐 가져온 것(대표): 리스너가 바꾼 상태를 다시 검사(복제 방지), 화로 결과가 출력 칸에 안 들어가면 취소(덮어써 잃음), LeavesDecay를 실제로 썩을 때만, `close_forms`가 대기 폼도 지움, 꽉 찬 인벤토리에 삼지창을 넣어 잃음, 스폰 이동 때 붙든 청크, 셜커 상자 안 셜커 상자.
- P6(BDS 1.26.52.3): 스폰 창(SetCommandsEnabled·AvailableCommands 포함)·NetherNet·폴백·M7 게임 시나리오 SAME, `p6 api`: kick·forms·particles·script SAME, commands는 의도된 차이 4가지(`/say` PMMP식, give 아이템 표시 이름 표 없음, gamemode 뒤 UpdateAbilities(PMMP식), 문법 오류 문장(검증 필요(M9)))를 `xtask/data/p6/intended.txt`에 이유와 함께 두어 `p6 anchor` PASS. 사실은 `m8-facts.md`.
- 성능(release, 평균/p99 ms, M7 → M8, 리스너 없음): 1명 0.018/0.26 → 0.017/0.32, 20명 0.189/0.66 → 0.186/0.63, 100명 1.77/3.97 → 1.58/3.79, 아이템 1,000 0.173/0.23 → 0.153/0.27, 아이템 10,000 2.20/2.79 → 1.23/1.59, 작물 0.040/0.075 → 0.027/0.043, 조명 켬 0.72/1.83 → 0.72/1.78. 빈 리스너 비용은 노이즈 수준. 남은 것: 1명 p99(조명 일의 블록 읽기, 16바이트 표현), movement·food 추가 조회(100명 약 7 µs).
- 남은 위험과 검증 필요(M9): 공식 클라이언트에서만 보이는 것(창 열기 지연 2틱, 겹상자 짝 표시, 같은 차원 다른 월드 텔레포트 때 옛 청크, 커스텀 블록 렌더링·마찰·`"none"` 카테고리, 커스텀 엔티티 `rid`·`hasspawnegg`, 보스바 색 갱신, CommandRequest 버전), BDS 대조가 남은 것(AddPainting 위치·방향, 폭죽 메타데이터, 크리스털 `BlockTarget`, 삼지창 튕김, 주민 직업 번호, `%gameMode.*` 번역), 부하가 클 때만 흔들리는 시험(`tick::task::crash::forced_kills_keep_user_data_with_its_tick`, `auth` 키 갱신 시간 한도, 전체 실행 중 NetherNet 시그널링 연결 거부) — 혼자 돌리면 통과, M9에서 원인을 본다. M7 결정성 시험이 디버그 148초로 늘었다.

#### M8 진행 상태
- 설계를 적음(2026-10-04).
- 약속 만듦: `event.rs` 틀(리스너·모니터·`listeners!` 표, 영역 파일 6개), `tick/dispatch.rs`(heard·listened·call·watch·defer·perform, 틱당 실행 16,384), `action.rs`, `player.rs`(`Id`), `world::Id`, `tick::Setup`(listeners·tasks·commands·hooks), `Player` 공개, `send` → `send_packet`, 새 패킷 분기(Text, CommandRequest), 수신 훅 호출, `player::sync`. 단계 A(A1~A5) 띄움.
- A5 합침: `cargo xtask table [--check]`(verify에 들어감, 0.44초). 처음 10,217행 중 안 함 650, 빈칸 9,567. alias 규칙: 짧은 이름이 겹칠 때만 `pocketmine\` 뒤 경로, 멤버는 선언한 클래스로, 상수·enum case는 행이 아님, 같은 alias 여러 곳 허용(위치 모두 적음). status.txt는 공백으로 나눈 `<패턴> <상태> <나머지>`, `*`는 `\`·`:`를 넘지 않고 `**`는 모두. 기존 alias 오류 5개(`Facing` 3개는 pocketmine/math, `ItemTagToIdMap` internal, `WorldManager::save` 없음)는 단계 D에서 고친다. A6(엔티티·월드 API) 띄움.
- A3 합침: `pack::Pack`(zip 직접 읽기, 매니페스트 PMMP 규칙, 팩 256 MiB·64개·합계 512 MiB·manifest 256 KiB, 조각 128 KiB·200ms는 gophertunnel, 거절은 PMMP 문구·이유 58, 중복 조각 요청은 끊음(PMMP 코드)), `ServerBuilder::resource_pack`, `start_game` 훅(엔진 필드는 훅 뒤 되돌림), `on_receive`(직렬, Context)·`on_send`(병렬, `Fn + Sync`, 공용 스폰 배치·로그인·팩 단계 제외). 스폰 마감은 팩 요청 때 30초 + 조각마다 2초 늘림(최저 64 KiB/s). 팩 경로는 설정 파일 폴더 기준(`Config.dir`). 남은 것: CRC-32가 sdp.rs와 중복, TLS 경로 기준이 다름, 공식 클라이언트의 조각 요청 방식은 검증 필요(M9).
- A2 합침: `command`(`Command`, `Param` 15종 + optional, `Args`, `Sender`, `Output`, `Error`; 줄 512자·2,048바이트, 오버로드 64, 인자 32; 플레이어당 틱 2요청(PMMP), @s·@a·@p·@r(@r는 엔진 난수), `~` 상대 좌표, 선택자 인자 없음), 처리기는 `Result<Output, Error>`(성공 출력을 보내려고 10.8의 `Result<()>`에서 바꿈), `permission`(`Permission::new`, `Default`, 기본 권한 56개는 PMMP 이름의 `pocketmine` → `anchor`), 실행 중 덧붙임은 `Commands` 안 세션별 표(`Context::has_permission(Sender, ..)`), op·밴·화이트리스트는 redb `access` 표(저장 형식 그대로, 표만 더함, 손상이면 시작 거부), 밴 기간은 엔진 틱(재생 결정성, 꺼진 시간은 안 셈), `set_whitelist_enabled`는 저장 안 함(설정 파일을 쓰지 않음). 입장 정책 `command::admit`(꽉 참 → 화이트리스트 → 이름 밴 → IP 밴, PMMP 문구)을 `spawn::join`에 연결, `Identity.address`, 입력 로그 형식 5(주소, 콘솔 줄; 4 읽음), 콘솔 스레드 `anchor-console`(표준 입력은 끊을 수 없어 detach). 스폰 창에 SetCommandsEnabled(SetDifficulty 뒤)와 AvailableCommands(CraftingData 뒤)를 넣고 유예표 두 줄을 지움(P6 대조는 C5). AvailableCommands는 처리기가 `Sync`가 아니라 출력 단계 앞 직렬 부분에서 만든다.
- C2(폼·스코어보드·보스바) 띄움.
- A4 합침: 이벤트 97개(block 18, entity 24, inventory 8, player 39, server 1, world 7), 보조 타입(DamageCause 17, DamageModifier 11 고정 배열, DamageSource, CombustSource, RegainReason, Window, SlotChange, Particle, Sound, Actor, EnchantOption), alias 662. `List`는 `Option<Box<..>>`(리스너 표가 커져 스택 배열 lint), 실수 세터는 NaN·Inf를 무시하고 범위 값은 자른다(PMMP는 예외). 기반 클래스 메서드는 대표 이벤트 하나에 alias(`CancellableTrait::*` → PlayerDuplicateLogin 등). 안 만든 것: 플러그인 2, PHP·운영 7, 레드스톤 1, 패킷 훅 3, 월드 고정 3, BlockExplode·PreExplode(다른 차원에서만), BlockItemPickup(PMMP도 안 쏨), M7에 동작 없음 4(EntityFrostWalker, PlayerEditBook, PlayerEntityPick, PlayerRespawnAnchorUse), 직접 API만 2(DisplayNameChange 2개). 단계 B1(interact)·B2(inventory·block_entity·trade + 인벤토리 API) 띄움.
- A1 합침: `player::Message`(Raw, Chat, Translation, Popup, Tip, JukeboxPopup, System, Whisper, Announcement, Title, Subtitle, ActionBar, TitleTimes, ClearTitle, ResetTitle, Toast; 클라이언트 길이 상한을 넘으면 `Err`), `player::Error`, `Player` 게터·세터(신원, 위치, `teleport`(틱 끝 `sync`가 보냄), `send`, `kick`(Disconnect 55, PMMP는 0: 검증 필요(M9)), `transfer`, 게임 모드, 체력·배고픔·경험치·효과·불·비행·침대·스폰 지점·이름표 등), 채팅(PMMP `Player::chat`: 틱당 2줄, 512자·2,048바이트, `chat.type.text`), `Context`(`players`, `player`, `player_mut`, `player_exact`, `player_by_prefix`, `broadcast`, 서버 설정 게터, `set_max_players`(해시에 넣음)). 옛 `Context::add_experience`·`add_effect`·`teleport`는 `Player` 메서드로 옮김. 얻을 수 없는 값: locale·ping(세션 쪽), first/lastPlayed(저장 안 함), TPS(벽시계), `set_motd`(서버 목록은 시작 값). 스폰 창 순서 시험에 명령 패킷 2개를 넣음. B3(생존·채팅 이벤트) 띄움.
- A6 합침: `Entity`(틱 엔티티 구조체 공개)와 `entity::Mut`(위치를 바꾸면 청크 색인·저장 표시도 같이 바꿔야 해서 `&mut` 대신 가드), `entity::{Type, ArrowPickup, Projectile, Skin}`, `Context`의 엔티티 조회(`entity`, `entity_mut`, `find_entity`, `entities`, `chunk_entities`, `entities_in`(청크 색인 x 범위), `nearest_entity`)와 생성(`drop_item`, `drop_experience`, `spawn_experience_orb`, `spawn_falling_block`, `spawn_tnt`, `spawn_projectile`, `spawn_human`), 월드(`world(id)`, `world_mut(id)`, `worlds()`, `players_in`, `set_time`, `add_sound`(사운드 101), `add_particle`(입자 36), `broadcast_packet`, `explode`), `World::{id, is_spawn_chunk, is_in_world, loaded_chunks, biome, set_biome}`, `set_spawn`이 붙든 스폰 청크를 옮김(10.8 검증 필요 처리: 새 청크를 먼저 잡고 옛 것을 놓음). 소리·입자는 큐 없이 바로 보내되 받는 이의 outbox에 1,024칸 넘게 남았을 때만. 엔티티 저장 키 `CustomName`·`CustomNameVisible`(PMMP)과 `Scale`·`Invisible`·`NoGravity`·`CustomNameAlwaysVisible`. 옛 `Context::world`·`world_mut`는 `default_world`·`default_world_mut`로 이름을 바꿈(`world(id)`와 겹쳐서), `WorldManager::save` alias를 지움. 입자 번호는 gophertunnel 값(PMMP는 9·18을 끼워 9번부터 1~2 다름, 검증 필요(M8 P6)). 안 함: 시간 정지(시계와 월드 레코드 형식이 바뀜), 월드별 난이도(서버 설정 하나), 월드 로드·언로드·생성(빌드 때 고정), 청크 로더, FloatingTextParticle(이름표 NPC로 대신). B4(엔티티 이벤트) 띄움.
- C2 합침: `form`(`Form` = Modal·Menu·Custom, `Button`·`Image`, 원소 label·header·divider·input·toggle·slider·dropdown·step_slider, `Response`, `Value`, `Error`), `display`(`Scoreboard`(슬롯마다 objective `anchor:<slot>`, 바뀐 줄만), `BossBar`(엔티티 ID는 플레이어 자신의 runtime ID)). API는 `Context::send_form`·`close_forms`·`set_scoreboard`·`set_boss_bar` 등이다(처리기가 `&mut Context`를 받고 폼 표가 `State.forms`에 있어 `&mut Player`로 닿지 않음 → 10.8의 `Player::send_form`에서 바꿈). 대기 16개, ID는 u32 소진 시 거부, 답이 오면 맞든 틀리든 그 폼을 지움(두 번 = 한 번), 64 KiB, 슬라이더는 step 0.001 오차(클라이언트 f32), `close_forms`는 대기 폼도 지움(PMMP는 남겨 상한을 막음 → 고침). 상한: 원소·선택지 256, JSON 256 KiB, 사이드바 15줄·그 밖 1,024줄. JSON은 dragonfly v0.11.5(1.26.50). 검증 필요(M9): 메뉴 `elements` 형식, 보스바 색 갱신(공식 클라이언트·BDS 스크립트 팩). B5(블록 갱신 이벤트) 띄움.
- B1 합침: BlockBreak(`destroy_with`를 계획·적용으로 나눔, 리스너가 바꾼 드롭·경험치, `insta_break`면 부수기 시간 검사 생략 → 리스너가 있으면 이벤트를 시간 검사 전에 부름), BlockPlace(여러 블록), PlayerInteract(좌·우 클릭, 취소하면 블록·아이템 둘 다 안 씀), PlayerItemUse, PlayerBucketEmpty·Fill, SignChange(편집·염료), PlayerEntityInteract, BlockTeleport(용 알). 행동 `AttackBlock`·`BreakBlock`·`UseItemOn`·`UseItem`·`InteractEntity`. 리스너 뒤 다시 검사(블록, 손 아이템, 위치, 계획), 무작위 리스너 보존 시험(시드 3). PMMP와 다름: 실패할 이벤트를 미리 취소된 채로 보내지 않고 만들지 않음, 표지판 양면이 그대로면 아무것도 안 함(PMMP와 같게 바뀜). 다른 모듈의 이벤트가 상호작용 흐름 안에서 날 때 손 아이템 다시 검사를 B2·B3·B4에 전함, B4에 `reaches` 공개를 요청함. C1(기본 명령) 띄움.
- B3 합침: EntityDamage(플레이어, 수정자 11, 원인 17과 1:1이 되게 내부 `Damage`에 Melee·BlockExplosion·FallingBlock·Suicide·Custom을 더함), EntityRegainHealth, PlayerExhaust(걷기는 0으로 알림, PMMP), PlayerExperienceChange, PlayerItemConsume, EntityEffectAdd·Remove, EntityCombust·Extinguish, PlayerDeath(메시지는 빈 값으로 시작: M7 출력 유지), PlayerRespawn, PlayerGameModeChange(하드코어 관전), PlayerMove(틱마다, 취소는 되돌림), 토글 5·Jump·MissSwing, PlayerBedEnter·Leave, PlayerChat(`/` 줄은 PMMP처럼 명령), PlayerKick·Transfer·Quit, PlayerEmote(6틱 간격), PlayerChangeSkin(4 MiB, 1초 간격). 행동 `Damage`, `SetGameMode`, `Chat`, `Kick`, `Transfer`. 리스너가 넣은 값 검사(넉백 ±16, 드롭 41칸 등). Quit은 틱 안의 떠남 자리(입력 Leave, 넘친 outbox, 중복 로그인, 종료)에서만 내고 `deliver`의 떠남(틱 단계 밖)에서는 내지 않는다. Emote·PlayerSkin 분기를 더함.
- B2 합침: InventoryTransaction(모든 요청, 바뀐 칸 앞뒤), CraftItem, PlayerItemEnchant, Trade, PlayerDropItem, InventoryOpen·Close, PlayerItemHeld, PlayerBlockPick, PlayerEnchantingOptionsRequest(이름 64바이트·인챈트 16개), TradeOpen, FurnaceBurn·Smelt(`Furnace::step`을 나눔, 결과가 출력 칸에 안 들어가면 취소: PMMP는 덮어써 잃음 → 고침), BrewingFuelUse·BrewItem, CampfireCook, ChestPair. 요청의 다시 검사 = 지금 상태로 요청을 다시 실행해 리스너가 본 것과 같아야 함. 무작위 리스너 보존 시험 2 × 5만 요청. 공개 `inventory`(`Kind`, `Inventory`(읽기), `Editor`(쓰기, 칸마다 새 스택 ID와 보는 이에게 전송), `Error`), `Player::{inventory, inventory_mut, held_slot, set_held_slot, held_item, window}`, `Context::{block_inventory, block_inventory_mut, close_window}`. 남은 정리(단계 D): `Context::give`가 `Editor::add`와 겹침, `event::Inventory`와 `inventory::Kind`가 겹침.
- C3(예약·비동기·사용자 데이터)와 C5(봇 기능·P6 명령) 띄움.
- B5 합침: 공통 도우미 `tick/update/raise.rs`(`ask`·`tell`, PMMP `BlockEventHelper`처럼 grow·spread·form·melt·die). BlockGrow(작물·줄기·사탕수수·선인장·코코아·네더 사마귀·열매·횃불꽃·피처·동굴 덩굴·꽃잎·자수정), StructureGrow(`Tx::apply` 한 곳), BlockSpread(잔디·균사체·나일리움·불·발광 이끼·액체), BlockBurn, BlockForm, BlockMelt, BlockDeath(산호), LeavesDecay(실제로 썩을 때만: PMMP는 원목 검사 전에 매번 냄 → 고침), FarmlandHydrationChange, BlockUpdate(이웃 큐), EntityTrampleFarmland(`soil::trample` 한 곳, 다른 모듈은 내지 않음). `fertilize`·`trample`에 행위자 인자. 세터 없는 new_state·블록 목록은 PMMP처럼 못 바꿈. 비용: 밀밭 41.3 µs → BlockGrow 리스너 있음 41.9 µs(+1.5%). 남은 확인: 블록 갱신 벤치가 청크 49개만 틱함(문서는 318).
- C4(커스텀 블록·아이템·레시피·거래 API) 띄움.
- B4 합침: EntitySpawn·ItemSpawn은 `create`·청크 로드 때 큐에 넣고 엔티티 틱이 알림(물리 전·제거 직전, PMMP `onFirstUpdate`처럼: 다른 규칙 중간·직접 API에서 리스너가 돌지 않게 → 10.8의 "적용 자리에서"를 이렇게 구체화), EntityDespawn, ItemDespawn(취소면 나이 6,000으로), ItemMerge, EntityItemPickup(아이템·화살; 경험치 구슬은 PMMP처럼 안 냄), EntityDamage(`hurt`를 `plan_hit`·`take_hit`으로), EntityDeath, EntityMotion, EntityCombust·Extinguish, EntityRegainHealth, EntityPreExplode·EntityExplode(블록 목록 8,192 상한), ProjectileLaunch, EntityShootBow, ProjectileHitBlock·Entity, EntityTeleport(엔더 진주), AreaEffectCloudApply, EntityBlockChange. 행동 `Attack`, `Explode`(직접 `Context::explode`는 `Raise::Quiet`). 리스너가 물리 적용 중 엔티티를 바꾸면 새 상태에서 다시 계산. `entity::reaches`를 상호작용이 PlayerEntityInteract 전에 부름. 남은 정리: `room`(줍기 공간 계산)이 인벤토리 규칙을 한 번 더 적음, `Context::remove_entity`가 `flag_for_despawn`과 겹침.
- C6a(메뉴 창·가짜 블록) 띄움.
- C1 합침: 기본 명령 37개(ban, ban-ip, banlist, pardon, pardon-ip, op, deop, whitelist, kick, clear, effect, enchant, gamemode, give, kill, spawnpoint, tp, xp, title, tell, transferserver, help, list, me, say, seed, setworldspawn, defaultgamemode, difficulty, time, particle, save-all, save-off, save-on, status, stop, version), 뺌 4(dumpmemory, gc, plugins, timings). 명령은 권한 여러 개 중 하나(최대 16). `Context::{can_use, shutdown, set_auto_save, auto_save_enabled, set_difficulty, set_default_game_mode}`. 결정: `commands.*`는 번역 키로, `pocketmine.*`은 영어로 보냄, 대상은 Bedrock 식(@s·@a·@p·@r·정확한 이름, PMMP 접두 일치 아님), status는 틱 가동 시간·플레이어·청크·엔티티(벽시계 없음), save-off는 주기 저장만 끔(저장 안 함), particle 명령당 1,024개, `stop`은 틱 루프가 `stop_requested`를 보고 끝냄. 아직 안 됨: effect infinite·hideParticles, tp 회전, give NBT(time start/stop은 M9 FIX-D에서 구현). 검증 필요(C5): CommandOutput 안 `%gameMode.*`·`%potion.*` 번역.
- C6b(커스텀 엔티티) 띄움.
- C3 합침: `task`(`Handle`, `Error`, 작업 65,536·비동기 1,024·결과 1 MiB), `Context::{schedule, schedule_repeating, schedule_delayed_repeating, cancel_task, is_task_scheduled, cancel_all_tasks, current_task, spawn_blocking}`(순서: `every` → 예정 틱 → 만든 순서, 작업 단계에서 예약한 것은 다음 틱부터), 블로킹 풀은 tokio(세마포어로 사용자 작업 동시 4개, `MAX_BLOCKING_THREADS` 4 → 8), `work` 패닉은 `Error::Panicked`로 `done`에(사용자 작업은 틱·저장 스레드가 아님: 4절 6번에 닿지 않음), 결과는 `InputKind::Job`으로 다음 틱 입력, 입력 로그 형식 6(4·5 읽음, 재생은 기록된 결과). `data`: `Player::{data, data_mut, extension, extension_mut, insert_extension, remove_extension}`(Extensions 64종), `ServerBuilder::{player_data, server_data}`, `Context::{data, data_mut}`. 사용자 값은 `Send + Sync`(출력 단계가 플레이어를 병렬로 읽음). 플레이어 값은 extra NBT `anchor:data`, 서버 값은 redb `user` 표 → 저장 형식은 3 그대로(10.8의 "형식 4"에서 바꿈). 상한(플레이어 64 KiB, 서버 1 MiB)을 넘으면 마지막 저장 값을 남기고 경고, 플레이어 값 역직렬화 실패는 `None`(원본 유지), 서버 값 실패는 시작 거부. 남음: PlayerDataSave 이벤트를 내는 곳이 없음, 예약 작업은 재시작하면 사라짐(PMMP와 같음).
- D1(대응표 상태 규칙) 띄움. 그때 표: 구현 1,136, 병합 115, 안 함 662, 빈칸 8,304.
- C5 합침: 봇 기능(`xtask/go/bot/api.go`: 채팅, 명령(CommandRequest), CommandOutput·AvailableCommands 요약, 폼 답, 창 칸 클릭, 표시 기록), 봇 시나리오 `commands`·`particles`·`kick`·`forms`·`script`(`-script` 파일의 동작을 차례로: 예제 시험용), `cargo xtask p6 api bds|anchor|compare`(BDS는 `allow-cheats`, 폼은 스크립트 팩 `xtask/data/p6/forms-pack`, @minecraft/server-ui 2.0.0, 실험 토글 없음), 유예표 Camera 세 줄은 `-`(PMMP에 카메라 API 없음), 사실은 `xtask/data/p6/m8-facts.md`. `p6 bds` PASS. 닫은 검증 필요: 메뉴 `elements` 형식(BDS와 같음), kick 이유 55(BDS와 같음), 입자 번호(14 아이템 깨짐·15 눈덩이는 PMMP 번호가 맞음: gophertunnel은 9번 뒤 하나 작음 → PMMP 값으로 고칠 것, 19 이상은 검증 필요(M9)). BDS와 다른 Anchor 값(고칠 것): 모르는 명령 메시지(`commands.generic.unknown` [이름]), 채팅 메아리(BDS: TextType 1, SourceName `이름§r`, 원문, 번역 없음), message 인자 형(68 MessageRoot), op 뒤 UpdateAbilities 권한, gamemode는 UpdatePlayerGameType, 폼 `"image":null`·`"icon":null`·슬라이더 `"timeout":100`, 눈덩이·알 명중 LevelEvent 2009 ×6. PMMP식으로 둔 차이: `/say`(BDS는 Text 8만), op 뒤 AvailableCommands 재전송.
- D2(예제 pvp·minigame, 예제 시험 틀) 띄움.
- C6a 합침: 가짜 블록 `Context::{send_fake_block, fake_block, clear_fake_block, clear_fake_blocks}`(플레이어당 4,096, 틱당 4,096, 출력 단계에서 보내 마지막 상태가 이김, 서브청크 답 뒤 다시 보냄, 덮은 자리의 실제 변경은 그 플레이어에게 안 보냄, 서버 판정은 실제 블록), `menu`(`Menu`(Chest 27, DoubleChest 54, Hopper 5), `Click`, `Action`, `Response` = Keep·Close(Take는 보존을 보장할 수 없어 뺌), `Id`, `Error`), `Context::{open_menu, menu, close_menu, set_menu_item}`. 메뉴 블록은 플레이어 뒤(InvMenu식), 창은 블록 2틱 뒤에 염, 메뉴 칸을 건드리는 요청은 통째로 거부하고 첫 동작이 Click 하나, 처리기는 State에 따로(Player는 출력 단계에서 공유돼 `Sync` 필요). 남은 것: `interact::resync`가 가짜 블록을 고려하게(E 단계), 이벤트 `Window`에 메뉴가 없음, 검증 필요(M9): 공식 클라이언트의 창 열기 지연·겹상자 짝 표시.
- E1(P6 차이 고치기) 띄움.
- C6b 합침: `entity::Custom`(식별자, 크기, 중력, 저항, 최대 체력, 이름표, 틱 주기, 소환·알·임시), `entity::Behaviour: Send`(`on_tick`, `on_interact`; `on_damage`·`on_death`는 규칙 중간 `&mut` 콜백이 위험해 빼고 이벤트의 `custom_type()`으로), `ServerBuilder::custom_entity`, `Context::spawn_custom_entity`, `Entity::{custom_type, custom_data}`(4,096바이트, 저장 키 `AnchorData`). 커스텀은 `Kind::Npc`의 `Form::Custom`(피해·넉백·죽음 규칙을 그대로 받음, 기본 무적 아님). 타입 1,024, 식별자 128바이트. AvailableActorIdentifiers에 `rid` = 바닐라 최대 + 1부터(커스텀 없으면 바이트 같음). 등록 안 된 타입은 로드 때 NBT를 두고 만들지 않음. 검증 필요(M9): 공식 클라이언트의 `rid`·`hasspawnegg` 사용.
- D2 합침: `examples/pvp.rs`(키트 명령·쿨다운·권한, 낙하 피해 없음, 킬 연속, 드롭 없음, 영속 킬·데스, 사이드바), `examples/minigame.rs`(임시 월드, 카운트다운 타이틀·보스바, 텔레포트, 구역 밖 부수기 취소, 모달 폼, `schedule`, `spawn_blocking` 요약, 수신 훅), `xtask/tests/examples.rs`(예제 빌드 → 임시 폴더 `anchor.toml` → 자식 프로세스 → 봇 `script`), 봇 동작 `expect-item`·`expect-position`. pvp 시험은 PlayerJoin이 아직 나지 않아 실패(E2에서 고침). API 빈틈: PlayerDeath에 킬러·원인이 없고 `last_damage_cause` 없음, 모니터가 `&Context`라 통계를 못 씀, 비영속 서버 전역 사용자 상태 없음, 아이템 만들기가 김(상수 없음), 스폰 발 위치 도우미 없음.
- 빠뜨린 연결(E2로): PlayerLogin, PlayerJoin, PlayerResourcePackOffer, PlayerDataSave, PlayerPostChunkSend, PlayerViewDistanceChange, PlayerDuplicateLogin 모니터, 월드 이벤트(ChunkLoad·Unload, WorldSave, SpawnChange, WorldDifficultyChange, WorldSound·Particle), CommandEvent 확인.
- D3(lobby 예제) 띄움.
- C4 합침: `custom`(`Item`, `Block`(속성 bool·int 범위·enum, `Material`, `RenderMethod`, `Drops`, `Behaviour`: `on_use`·`on_place`·`on_break`), `Recipe`(shaped·shapeless·furnace), `Ingredient`, `FurnaceType`, `ItemRef`, `Creative`, `Category`, `Error`, `Offer`), `ServerBuilder::{custom_item, custom_block, recipe}`(정의는 `&'static`), `Context::{custom_item, custom_block, item_of}`, 거래 `Context::{trade_offers, set_trade_offers, add_trade_offer, ..}`. 표현: 전역 레지스트리 없이 `block::State`·`item::Type`을 enum으로(2 → 16바이트: 커스텀 값이 `&'static` 정의와 번호를 들고 다님, 바닐라 경로는 분기 하나, 커스텀 블록의 `kind()`는 `InfoUpdate`라 바닐라 규칙이 반응하지 않음). 번호는 등록 순서(런타임 ID 884부터, block_id 10000부터, 레시피 ID는 PMMP 뒤), 해시 충돌이면 빌드 거부. 블록 해시 함수를 런타임 `block/hash.rs`로 옮김(10.8 검증 필요 처리, 골든 22,091 + dragonfly 커스텀 3). 디스크는 이름으로, 틱이 청크를 넣을 때 등록된 블록이면 커스텀으로 바꾸고 아니면 원본 유지. `Shared::new(settings, custom, custom_entities)` 하나로 합침(커스텀 없으면 바이트 같음). 남은 일(E3): 저장된 커스텀 아이템을 읽는 5곳이 레지스트리 없이 읽어 재시작 때 사라짐(데이터 손실), 커스텀 화로 레시피를 화로·모닥불이 안 씀, `spawn_villager`를 `&[Offer]`로, `/give` 커스텀 아이템, 커스텀 블록 랜덤 틱 훅, permutation. 검증 필요(M9): 마찰 1 − factor, `"none"` 카테고리, 속성 상한, 공식 클라이언트 렌더링. 성능: 팔레트 항목이 커짐(재야 함).
- E2(남은 세션·월드 이벤트)와 D4(custom 예제) 띄움.
- D1 합침: 표 도구에 `// pmmp-class: Name`(rustdoc이 항목 이름과 같은 alias를 거부해서: 항목 바로 위 주석으로 클래스 행을 덮음), `// pmmp:`는 시험 속성 위(사이에 `///`·주석·속성 허용, 빈 줄 거부), 새 상태 `open <메모>`(아직 없고 거부하지도 않은 기능: 빈칸으로 남고 넓은 규칙이 가져가지 못함). 상태 규칙 약 1,200줄(VanillaBlocks·VanillaItems·블록 클래스 속성 → `Block`(= `block::State`) 병합, 블록·아이템 동작 훅·`data`·`network`·레드스톤·차원·바닐라 생성 → 안 함 등). 합친 뒤 표: 구현 1,150, 병합 498, 안 함 4,826, 빈칸 3,743(시험 대기 약 2,100, 대상 alias 대기 약 980, open 399, 미결정 약 285). 다른 alias가 생겨 쓸모없어진 상태 줄 4개를 지움.
- F0(빠진 alias·클래스 주석·시험) 띄움.
- D3 합침: `examples/lobby.rs`(나침반·`/games` 메뉴 폼 → Arena 텔레포트(영속 두 번째 월드)·Shop 메뉴 창, `/spawn`, 사이드바, 보스바, 로비에서 배고픔·피해 취소), 봇 스크립트, 시험 `the_lobby_example_opens_the_menu_shop_and_arena`(약 9초). PlayerJoin 대신 1초 `every`가 새 플레이어를 찾음(E2 뒤 바꿀 수 있음). 관찰: 같은 차원 다른 월드 텔레포트에 ChangeDimension이 없음(검증 필요(M9): 공식 클라이언트에 옛 청크가 남는지), 아이템 이름 도우미 없음, 봇 `expect-window`가 창 내용까지 기다리지 않음(스크립트에 대기).
- E3(커스텀 아이템 저장 손실·커스텀 화로·`spawn_villager` 타입·중복 API 정리·resync·눈덩이 입자) 띄움.
- E1 합침: BDS와 같게 고침 — 모르는 명령 `commands.generic.unknown`·권한 없음 `commands.generic.error.permissions`, 채팅 메아리(PlayerChat 형식이 기본일 때만 BDS 모양, 리스너 형식은 그대로, 이름 256바이트 넘으면 번역으로), message 인자 68, op·deop 뒤 UpdateAbilities(1·2·255 / 0·1·63), gamemode는 본인에게 UpdatePlayerGameType과 `gameMode.changed`, 폼 `image`·`icon` null·슬라이더 `timeout` 100, give·tp 받는 쪽 메시지(raw text 10), tp 좌표 소수 둘째 자리. P6 api: kick·forms·particles·script SAME, commands는 남은 차이 4개(이유와 함께 m8-facts.md 9절: `/say` PMMP식, give 아이템 표시 이름 표 없음, gamemode 뒤 UpdateAbilities(PMMP식, API 값), 문법 오류 문장(파서 위치 필요, 검증 필요(M9))). 스폰 P6 SAME.
- F1(플레이어 쪽 대응표 빈칸) 띄움.
- D4 합침: `examples/custom.rs`(커스텀 아이템 `example:ruby`·블록 `example:ruby_block`(속성, 드롭)·레시피 shaped·shapeless·furnace·엔티티 `example:wisp`(행동, 저장 데이터)·주민 상점(타입 있는 오퍼)·코드로 만든 리소스팩(PNG·무압축 zip을 예제가 직접 씀)), 봇 동작 `expect-packs`·`expect-entity`·`interact`·`craft`·`place`·`use`·`break`·`expect-block`·`expect-drop`·`trade`, 시험 `the_custom_example_serves_its_pack_items_blocks_and_entities`(14~20초). 예제 4개가 모두 들어옴(pvp는 PlayerJoin 대기). API 빈틈(G 단계): 시작 때 한 번 도는 훅 없음, `pack::Error` → `anchor::Error` 변환 없음, 커스텀 타입으로 엔티티 찾기 없음, 주민 직업 enum 없음(7을 씀, BDS 미확인), PlayerDeath에 킬러·원인 없음.
- E2 합침: PlayerLogin(중복 로그인 뒤, Anchor 거절 사유를 모두 플래그로, 리스너가 가득 참을 지워도 밴은 막힘, `port()`는 0), PlayerJoin(Spawned 때, 기본 메시지 `§e%multiplayer.player.joined`를 다른 플레이어에게만: BDS 캡처와 같음, PMMP는 본인에게도), PlayerDuplicateLogin 모니터, PlayerViewDistanceChange, PlayerPostChunkSend·ChunkLoad(`is_new`)·SpawnChange(뷰 단계 끝 `view::announce`), ChunkUnload(리스너가 있으면 뷰 단계까지 미룸, 취소면 600틱 유지, 최대 1,024청크), WorldSave(커밋마다, 종료 커밋 제외), PlayerDataSave(스냅샷 직전, 취소는 무시: 한 플레이어만 빼면 원자 저장이 깨짐). 문서로: PlayerResourcePackOffer → `resource_pack` 병합(팩 교환은 틱 전 세션에서 끝남), WorldDifficultyChange(직접 API뿐), WorldSound·Particle(한 곳으로 모이지 않음, 송신 훅으로 거름). op면 스폰 창부터 op 능력, StartGame `player_permissions` 2. 뷰 단계 뒤에도 defer를 돌림. CommandEvent와 BDS식 퇴장 메시지는 F1에 맡김.
- F2(월드·제작 대응표 빈칸) 띄움.
- E3 합침: 저장된 아이템을 읽는 곳(인벤토리, 블록 엔티티 전부, 엔티티, 거래)이 `Stack::from_nbt_with`(커스텀 레지스트리)로 읽음 → 재시작 뒤 커스텀 아이템 유지(실제 redb 저장·재시작 시험; 생존 저장 데이터는 F1), 화로 계열·모닥불이 커스텀 화로 레시피를 씀, `spawn_villager(pos, profession, &[custom::Offer])`(잘못된 오퍼면 `None`), `Context::give`·`remove_entity` 삭제(같은 일을 하는 `Editor::add`·`flag_for_despawn`이 남음), `interact::resync`가 가짜 블록을 다시 보냄, 눈덩이·알 명중 LevelEvent 2009 ×6(BDS). 남음: 커스텀 블록 랜덤 틱 훅(`Behaviour::on_random_tick` + 플래그), 떨어지는 블록의 커스텀 상태, `/give` 커스텀 아이템.
- F0 합침: alias(`Block` = `block::State`, `Item::*` 10개, 생성기로 `Effect`·`VanillaEffects`·`VanillaEnchantments`), `// pmmp-class:` 주석(GameMode, Skin, Event, Inventory, Particle, Sound, Permission, BanEntry, Server, Player, Entity, World, Chunk, Generator, Flat 등), 잘못된 alias 4개 삭제(`Facing` 3: pocketmine/math, `ItemTagToIdMap`: internal), PMMP 덤프 대조 시험(효과·물약 전부, 아이템 1,613종의 도구·갑옷·음식·연료·사용 값, 블록 상태 11,420개의 부수기·불·플래그·마찰·지지·충돌 수). 표: 구현 1,297, 병합 3,316, 안 함 4,825, 빈칸 779.
- F3(블록 엔티티 API)·F4(엔티티·효과·서버·크리에이티브 행) 띄움.
- F1 합침: `Player`에 공기·움직임 플래그·이동 속도(기본 속도만: PMMP처럼 효과 섞인 값을 덮으면 떠돎)·오르기·자동 점프·`set_motion`·경험치 나머지·배고픔 끄기·갑옷 점수·`last_damage_cause`(리스폰 뒤에도 남음, PMMP)·`emote`·`set_skin`·사망 위치·`ip`·시야·화면 줄 높이, `Context`에 아이템 쿨다운·`drop_from`·시선·`target_block`·`sleep_on`·권한 목록. CommandEvent(`command::typed` 하나를 요청·콘솔·채팅이 같이 씀, `dispatch_command`는 안 냄), 기본 퇴장 메시지는 BDS처럼 다른 플레이어에게만 `§e%multiplayer.player.left`(`player::LEFT`, 중복 로그인 퇴장도), 생존 저장 데이터도 커스텀 레지스트리로 읽음. `Stack::from_nbt`(바닐라만)는 시험 전용으로 묶음(저장 데이터는 `from_nbt_with`만). 표: 구현 1,446, 병합 3,371, 안 함 4,849, 빈칸 551.
- 전체 시험 통과(합친 상태, `--no-fail-fast`), 퇴장 메시지 변경으로 바뀐 기대값 하나를 고침(리스너가 입장 중 킥하면 입장 줄 없이 보통 퇴장 줄).
- F2 합침: `Context::{crafting_result, furnace_result, brewing_result, chunk_viewers}`, `World::{safe_spawn, collision_boxes}`(한 축 32블록 상한), `FurnaceType::cook_ticks`, 생성기에 아이템 이름 색인 3개(화로·양조 조회가 선형 검색 대신 색인: 2.2). 레시피 객체·목록은 공개하지 않음(아이템으로 묻는 조회만, `custom::Ingredient`와 겹치지 않게). 안 함: 청크 즉시 로드(틱 밖), `setChunk`, `setDisplayName`(이름은 저장 키), 양조·석재 절단기 레시피 등록. 표: 구현 1,457, 병합 3,405, 안 함 4,891, 빈칸 464.
- G1(작은 남은 일: `World::chunk`, 예약 블록 갱신 API, 커스텀 블록 랜덤 틱, `set_view_distance`, 표지판 편집 창, 별칭, `From<pack::Error>` 등) 띄움.
- F3 합침: `tile`(`Tile` 읽기, `tile::Mut` 쓰기 = `Context::block_entity`·`block_entity_mut`·`chunk_block_entities`; 컨테이너 이름·잠금·상자 짝·셜커 방향·모닥불·신호기·표지판 글·밀랍·침대 색(반쪽마다, PMMP)·배너·화분·액자·주크박스·독서대·책장·머리·가마솥; 쓰기는 실패하면 아무것도 안 바꾸고 성공하면 저장 표시와 BlockActorData, 이벤트 없음, 이름 1,024·표지판 1,000바이트·무늬 16 상한), alias 101. 안 함: Bell·MonsterSpawner·SporeBlossom(저장 상태 없음·몹 AI), `BaseSign::update*Text`(플레이어 편집은 SignChange). 남음: Note 블록 엔티티(Anchor에 없음, 만들면 새 저장 종류 → open), 표지판 상한이 `interact::sign::MAX_BYTES`와 겹침. 표: 구현 1,560, 병합 3,465, 안 함 4,920, 빈칸 272.
- F4 합침: `effect::Instance` 빌더(`with_duration` 등)·`effect::mix`(색 섞기 한 곳)·`Type::instance`, `entity::Animation`(PMMP 클래스 11)·`Context::play_animation`, 엔티티 게터·세터(중력·스텝 높이 읽기, 주인·대상·스택 크기·물약, 구름 값), `Context::spawn_area_effect_cloud`, 플레이어 숨기기 `Context::{hide_player, show_player, can_see}`(보는 이마다 1,024, 나가면 지움), `Context::player_by_uuid`, 경험치 구슬이 `can_attract_xp_orbs`를 봄. 안 함: 오프라인 플레이어 데이터(저장 스레드 소유, 틱은 블로킹 I/O 안 함), 방송 채널(broadcast·send로 합침), 중력·스텝 높이 세터(종류별 상수), 크리에이티브 인벤토리 변경(빌드 때 고정). 표: 구현 1,625, 병합 3,480, 안 함 4,958, 빈칸 154.
- 남은 빈칸 154의 분류: G1이 맡은 것(getChunk·getSide·예약 갱신·시야·표지판 편집·별칭 등), PMMP에 있으나 Anchor에 없는 엔티티(Painting·PaintingMotive·Trident·FireworkRocket·EndCrystal·IceBomb 약 60행: 1절 "PMMP가 구현한 것은 구현" → F5에서 구현), 소리 표(Note·Record·GoatHorn), 상태 결정만 남은 것(이벤트 게터, Command·Console 계열, 서버 IP·포트 alias, TaskHandler 게터 등 → G2).
- F5(빠진 PMMP 엔티티) 띄움.
- G1 합침: `World::chunk` 공개(읽기), `BlockPos::step` 공개, `Context::schedule_block_update`(기존 큐, 위치마다 하나), 커스텀 블록 랜덤 틱(`custom::Behaviour::on_random_tick`, `custom::Block::random_ticks()`, 바닐라와 같은 순서·스트림, `disable-block-ticking`은 커스텀에 안 걸림), `Context::set_view_distance`(1~`view-distance`, 직접 호출이라 이벤트 없음), `Context::open_sign_editor`, `GameMode::from_name`, `Permission::__construct` alias, `Error::Pack`(`From<pack::Error>`), 리스폰 지점이 `World::safe_spawn`을 씀(규칙 한 곳), `NetworkSession::getPort`는 안 함(포트를 보관하지 않음: 넣으면 입력 로그 형식이 바뀜). 표: 구현 1,634, 병합 3,483, 안 함 4,959, 빈칸 141.
- G2(나머지 빈칸·정리) 띄움.
- G2 합침: 서버 IP·포트·온라인 모드 alias(설정 필드), `block::Face::HORIZONTAL`, `world::Sound::{Note, Record, GoatHorn}`·`NoteInstrument`, `Player::window_item`(인챈트대 칸), `inventory::HOTBAR_SIZE`, 셜커 상자 안 셜커 상자 거부, `Context::{sender_name, send_to, broadcast_command_message, task_next_run, task_period}`, 경험치 계산 함수 공개(넘침 없음). 정리: `open_sign_editor`가 `tile::Side`를 받음(bool 인자 금지), 기본 명령이 `GameMode::from_name`, `ticks_randomly`가 커스텀 설정을 봄, 무한 효과 유지와 `effect` 상수·규칙 한 곳, 표지판 상한 한 곳(`tile::MAX_SIGN_TEXT_BYTES`), CRC-32 한 곳(`crypto/crc32.rs`), `BlockPos::containing(Vec3)`. 안 함: Element(덤프에 데이터 없음), Note 블록 엔티티(PMMP도 소리를 내지 않음, 저장 `Music`은 그대로 둠), 레시피 객체, AsyncTask 취소 등.
- F5 합침: 그림(벽에 맞는 가장 큰 것 중 엔진 난수, 겹침 거부, 받침이 사라지면 떨어짐, `AddPainting`), 엔드 크리스털(흑요석·기반암 위, 타격 다음 틱 크기 6 폭발, 연쇄), 폭죽 로켓(`Flight`로 비행 시간, 반경 5 피해, 저장 안 함, 공중 부스트는 PMMP에 없음), 던진 삼지창(14틱 이상, 피해 8, 자리가 있을 때만 줍기: PMMP는 꽉 찬 인벤토리에 넣어 잃음 → 고침), 얼음 폭탄(3×3×3 물 → 얼음), `entity::PaintingMotive`, `Context::{spawn_painting, can_hang_painting, spawn_end_crystal, spawn_firework_rocket, spawn_trident}`. 검증 필요(M9): BDS의 AddPainting 위치·방향, 폭죽 메타데이터, 크리스털 `BlockTarget` 기본값, 삼지창 튕김.
- 대응표 빈칸 0: 구현 1,691, 병합 3,518, 안 함 5,008(전체 10,217행).
- H1 합침: 시험 `busy_listeners_tasks_and_jobs_give_the_same_ticks_on_one_and_many_threads_and_replay`(모든 이벤트에 빈 리스너·모니터 + 바쁜 리스너 약 20개, 예약 작업, 폼, `spawn_blocking` 결과 입력, 채팅·명령: 스레드 1과 max(L,4)의 틱별 해시·출력·로그 같음, 재생 같음, 디버그 75.7초), `idle_listeners_and_monitors_on_every_event_leave_the_m7_ticks_unchanged`(리스너 없음 = 빈 리스너, 해시·출력·로그 바이트), 재진입 시험 2개. 찾아 고친 결함: `despawn_heard`가 해시에 들어감, 청크 해제 시점이 ChunkUnload 리스너 유무로 달라짐 → 늘 뷰 단계에서 해제(PMMP도 월드 틱에서, 리스너 없을 때도 M7과 비트 단위로 다름), `Context::explode`가 플레이어 피해 이벤트를 냄 → `survival::damage_quiet`. 성능(release, 평균/p99 ms, M7 재측정 → M8): 1명 0.015/0.27 → 0.019~0.020/0.36~0.38, 20명 0.238/0.77 → 0.26~0.30/0.80, 100명 1.87/4.08 → 2.13~2.31/4.5~4.7, 아이템 1,000 0.183/0.31 → 0.21~0.23/0.36~0.40, 아이템 10,000 2.54/3.46 → 3.3~3.7/4.0~5.5, 작물 랜덤 틱 0.023 → 0.031, 조명 2.02/1.24 → 2.15~2.31/1.32~1.39. 빈 리스너 비용은 노이즈 수준(아이템에서 +3~10%). M8이 10~40% 느려짐 → P1에서 원인을 나눠 고친다. 벤치는 측정마다 두 경우를 나란히(한 프로세스 안 순서 효과 약 1.5배).
- P1(성능 회귀), V1(P6 재실행), R1(체크리스트 감사)을 병렬로 띄움(사용자 지시: 벤치 최소화, 병렬 최대).
- 합친 상태(H1까지) `cargo xtask verify` PASS. R1 감사: 높음 7(스킨 방송 4 MiB × 받는 이 복사, 명령 출력 길이 상한 없음, InventoryClose 뒤 다시 확인 없음, 발생하지 않는 이벤트 4개, 커스텀 데이터 상한 전 할당, zip 오프셋 덧셈, 점수판이 로딩 중 플레이어를 받음), 성능 5, 일관성·단순함 여러 건 → P1 뒤 파일 단위 수정 3개(K1 명령·채팅, K2 이벤트·인벤토리·폼, K3 생존·엔티티·커스텀·팩·작업).
- V1: `p6 bds` PASS, `p6 anchor`는 api commands의 문서화한 4가지 차이뿐(새 차이 없음) → 비교 표에 의도된 차이로 올려 PASS로 끝나게 한다(K1).
- P1 합침: 원인 ① 엔티티 물리 입력을 틱 스레드에서 직렬로 만들고 결과를 두 번째 Vec에 묶음(아이템 10,000에서 약 1.3 ms) → 풀 작업이 자기 엔티티를 읽어 입력을 만듦, ② `block::State`·`item::Type` 16바이트 enum이 되며 작은 게터가 모듈 밖에서 인라인되지 않음(블록 읽기 약 2배) → 핫 게터 `#[inline]`, 바닐라 `value`·`with`는 표를 바로 읽음, ③ `player::sync`가 틱마다 플레이어마다 찾음·`track_players`가 쌍마다 찾고 AddPlayer를 매 틱 만듦 → 표시된 플레이어만, 새 시청자가 있을 때만. 표현은 16바이트 enum 유지(u32 + 서버 표는 1.9~2.1 ns로 조금 빠르나 커스텀 상태 메서드에 레지스트리가 필요해 API 변경 또는 전역 상태(4.1 금지)). 벤치(M7 → M8 뒤, 평균/p99 ms): 1명 0.018/0.26 → 0.017/0.32, 20명 0.189/0.66 → 0.186/0.63, 100명 1.77/3.97 → 1.58/3.79, 아이템 1,000 0.173/0.23 → 0.153/0.27, 아이템 10,000 2.20/2.79 → 1.23/1.59, 작물 0.040/0.075 → 0.027/0.043, 조명 켬 0.72/1.83 → 0.72/1.78, 끔 0.044/0.64 → 0.042/0.08. 남은 것: 1명 p99 약 20%(조명 일의 블록 읽기 약 0.5 ns), 아무도 듣지 않을 때 movement·food의 추가 조회(100명에서 약 7 µs).
- K2 합침: InventoryClose 뒤 같은 창일 때만 닫음(리스너가 연 메뉴 유지), 발생하지 않던 이벤트 4개(WorldDifficultyChange, WorldSound, WorldParticle, PlayerResourcePackOffer)와 `event::Particle`·`event::Sound` 삭제(대응표는 안 함·병합), 점수판이 로딩 중 플레이어 거부, 메뉴 단계는 `state.menus` 키만, bool 인자 → enum, `action::Error::Full`·`action::Result`·행동은 비공개 필드 + `new`, `Tile::position`, `event::Inventory`가 `inventory::Kind`를 씀, `event::Command::sender`는 `command::Sender`, 공개 색 타입은 `protocol::Rgba` 하나(`tile::from_argb`·`to_argb`), 메뉴 ID도 소진 때 거부, `close_menu` 삭제(`close_window`가 메뉴도 닫음). 공통 dispatch 도우미(감사 24)는 지점마다 모양이 달라 하지 않음. 이벤트는 93개.
- K1 합침: 명령 출력 상한 한 곳(`tick::command::shown`: 64줄, 줄당 인자 32, 합계 `MAX_TEXT`, 문자 경계에서 자름), `/banlist`·`/whitelist list`는 처음 100개 + "and N more", AvailableCommands는 `finish()`에서 한 번 만든 `available::Catalog`(처리기 없음, `Sync`)에서 출력 단계가 플레이어별로 고름, `is_op` 등 이름 조회에 할당 없음, 채팅과 명령이 틱당 2개 하나의 카운터(PMMP), 권한 덧붙임은 떠날 때 `command::left`가 지움(틱마다 `retain` 없음), `register`의 bool → `Origin`, ban 게터 alias를 `bans()`·`ip_bans()`로, 이름 검사 한 곳(`login::check_user_name`), 게임 모드 이름 `GameMode::ALIASES`, 난이도 이름 표 하나, `sonicexplosion`을 이름 목록에 넣음(PMMP에 있음). P6: 검토한 의도된 차이 파일 `xtask/data/p6/intended.txt`(12개, 시나리오 안 정확한 글자로만 맞춤, 다른 차이는 여전히 실패) → `cargo xtask p6 anchor` PASS.
- K3 합침: 스킨 방송은 `Arc` 하나를 함께 씀(`Message::SharedPacket`, 송신 훅이 있을 때만 복사) + 틱당 스킨 4 MiB 예산, 커스텀 엔티티 데이터는 한도에서 멈추는 `Writer::nbt_within`(기존 4 MiB `nbt`도 같은 방식), zip 오프셋 checked, 커스텀 화로 레시피·크리에이티브 항목은 색인, `player_exact`는 소문자 이름 색인 먼저, 스냅샷의 사용자 데이터 인코딩은 틱에서(스냅샷 바이트 상한은 한 틱 경계의 일부만 저장해 원자성을 깨므로 값마다 크기 제한: `performance:` 이유를 코드에), 뷰 버퍼 용량 유지, 커스텀 빌더 `with_*`, 같은 사실 한 곳(`MAX_SKIN_BYTES`, NBT 태그, `Face::index`, `world::Id::of`, `task::MAX_RUNNING_JOBS`), `set_current_total_xp`도 범위 밖이면 `Err`, `spawn_villager(world, pos, profession, offers) -> Result`, `extinguish()` 삭제(`set_fire_ticks(0)`), `scheduleRepeatingTask` alias는 `schedule_repeating`에만(`every`는 빌드 때 형태), 커스텀 블록 드롭 개수가 스택 크기를 넘으면 빌드 거부.
- 끝남(2026-10-04): K3까지 합침, 모든 worktree·브랜치 정리, `cargo xtask verify` PASS.

### 10.9 M9 세부 설계 (2026-10-04)
- 목표: CLAUDE.md 2절 고정 목표 전부와 M9 통과 기준(체크리스트 지적 0, 검증 필요 0, 막힘 0, P6 전체 통과). "검증 필요 0"은 아래 "실제 접속 확인 대기"(공식 클라이언트로만 볼 수 있는 것)와 "Linux 검증 대기"(12절 8번, 사용자 결정)를 뺀 나머지로 판정한다.
- 순서: 0 빌드 속도 → 1 설계(이 절) → 2 검토(영역별 읽기 전용 서브에이전트 9개: R1 네트워크·프로토콜, R2 세션·로그인·수명, R3 틱·결정성, R4 월드·저장, R5 게임 A(상호작용·블록 갱신·엔티티), R6 게임 B(인벤토리·작업대·생존·거래), R7 API·이벤트·명령, R8 vendor 사본, R9 xtask·시험·게이트) → 반박(다른 서브에이전트) → 살아남은 것만 수정(수정마다 고치기 전 실패·뒤 통과 시험) → 3 운영 기능·M8 남은 위험·대응표·검증 필요 구현(worktree 서브에이전트, 동시 빌드 5개 이하) → 4 P6 전체, verify, 체크리스트 감사.
- 검토 규칙(사용자 지시): 결함 = 실제로 닿는 경로에서 잘못된 결과를 내는 코드(파일:줄 + 재현 시나리오). 근본 원인만 고친다(증상 가드, 이미 보장된 것의 재검사, 일어날 수 없는 상태 처리 금지, 지워서 고칠 수 있으면 지움). 성능·일관성·가독성을 깎아야 하면 2절 우선순위와 이유. 사용자 결정과 부딪치면 고치지 않고 보고. 공개 API 변경은 이유를 보고.

#### 빌드 속도 (0단계)
- 방법: 변경마다 전후 `cargo xtask verify` 전체 시간(Bash, `src/lib.rs`를 건드린 뒤의 따뜻한 실행)을 재서 빨라지고 PASS일 때만 남긴다. `panic = "unwind"`, release `overflow-checks`는 그대로.
- 후보: (1) `.cargo/config.toml` `[target.x86_64-pc-windows-msvc] linker = "rust-lld.exe"`, (2) `[profile.dev] debug = "line-tables-only"`, `[profile.dev.package."*"] debug = false`, (3) `[profile.dev.package."*"] opt-level`, (4) sccache(`RUSTC_WRAPPER`, worktree 의존성 빌드 공유).
- 결과(따뜻한 실행 = `src/lib.rs`를 건드린 뒤, Bash, 같은 기계 28스레드; 측정 중 다른 서브에이전트가 돌아 노이즈 있음):
  - 기준: 290초·271초(시험 단계 177·156초). 두 번 모두 `forced_kills_keep_user_data_with_its_tick` 실패(원래 흔들리던 시험, 원인은 검토 R3-K5).
  - (1) rust-lld: 차가운 518초 PASS, 따뜻한 265초 → 남김.
  - (2) 디버그 정보(`line-tables-only`, 의존성 `debug = false`): 차가운 334초, 따뜻한 156초(시험 단계 100초) → 남김. 패닉·백트레이스의 파일과 줄은 남는다.
  - (3) 의존성 `opt-level = 1`: 차가운 472초 PASS, 따뜻한 208초에 시험 9개 실패(인증·시그널링 시간 시험, 그때 부하) → 느려서 되돌림.
  - (4) sccache 0.18.0(`.cache/tools`, zip sha256 확인; 사용자 지시: 효과가 있으면 모든 worktree 빌드에 적용): 차가운 `cargo build --workspace --all-targets`가 sccache 없이 83초, 새 target 폴더 둘에서 채움 95초·다시 89초(적중 0%: target 경로가 캐시 키에 들어감). `SCCACHE_BASEDIRS`로 worktree 두 개를 정규화하면 적중 43%지만 채움 127초·다시 135초로 빨라지지 않음(proc-macro·bin 크레이트, 빌드 스크립트(aws-lc C 빌드), 증분 컴파일 크레이트, 링크는 캐시되지 않음). 효과가 없어 적용하지 않고 도구와 캐시를 지웠다.

#### M9로 미룬 항목과 검증 필요 (처리 방법)
처리: 구현 = 이번에 만듦, P6 = BDS 대조로 닫음, 대기 = 실제 접속 확인 대기(공식 클라이언트), Linux = Linux 검증 대기(12절 8번).

| # | 출처 | 항목 | 처리 |
|---|---|---|---|
| 1 | 10.5, 10.6 | 재생 도구(크래시 로그를 읽어 재생) | 닫음(`Server::replay`) |
| 2 | 10.6 검증 필요(M9) | 커밋 뒤 로그만으로 재생하려면 구간 시작의 실행 상태 체크포인트 | 닫음(형식 8 체크포인트) |
| 3 | 10.6 | 입력 로그 64 MiB가 한 저장 주기 안에 차면 그 구간이 잘림 | 닫음(1/4에서 스냅샷) |
| 4 | 12절 5번, 10.6 | Bedrock(LevelDB) 월드 가져오기: LevelDB 리더, 디스크 서브청크, 옛 블록 상태 업그레이드 | 구현 |
| 5 | 10 표 M9 | 운영: 정상 종료(Ctrl+C, 콘솔 stop) 확인·시험, 백업, 남은 콘솔 기능 | 구현 |
| 6 | 10.6 검증 필요(M9) | 저장 시작 비용이 파일 크기에 비례(1 GB 이상) | 닫음(실측, BACKUP) |
| 7 | 10.8 | 부하가 클 때만 흔들리는 시험 3개(`forced_kills_keep_user_data_with_its_tick`, `auth` 키 갱신 시간 한도, 전체 실행 중 시그널링 연결 거부) | 구현(근본 원인) |
| 8 | 10.7, 10.8 | M7 결정성 시험 디버그 148초 | 구현(시간 줄임) |
| 9 | 10.8 P1 | 1명 p99 약 20%(블록 값 16바이트) | 전역 상태 없이 다시 찾음, 없으면 양보 기록 |
| 10 | 10.8 | 대응표 "안 함" 5,029행 재검토, 8.4 범위인데 빠진 API | 구현 |
| 11 | 10.7 D | 벽걸이 간판 충돌 상자(PMMP 뒤집힘) | P6 |
| 12 | 10.7 K, 10.8 | ParticleType 19 이후 번호, `UnWaxOxidised` 0xfa0a, `EntityDataFlagCount` 131 | P6 |
| 13 | 10.7 | 갑옷·책 인챈트 선택지 `Slot` 값 | P6 |
| 14 | 10.7 | MovePlayer 대 MoveActorDelta(봇 둘) | P6 |
| 15 | 10.7 | 등급 상승 때 UpdateTrade 재전송, demand≠0 가격 표시 | P6 |
| 16 | 10.7 | 신호기·모루·대장장이 요청 모양 | P6 |
| 17 | 10.7 | 메타데이터 키 136~140 타입, 구름 메타데이터 | P6 |
| 18 | 10.7 | 사람 NPC AddPlayer 필드, 다른 플레이어 AddPlayer 72키 | P6 |
| 19 | 10.8 C1, C5 | CommandOutput 안 `%gameMode.*`·`%potion.*` 번역, 명령 문법 오류 문장 | P6 |
| 20 | 10.8 F5 | AddPainting 위치·방향, 폭죽 메타데이터, 크리스털 `BlockTarget`, 삼지창 튕김 | P6 |
| 21 | 10.8 D4 | 주민 직업 번호 | P6 |
| 22 | 9.1 | dimpl 레코드 순서(#169) 릴리스 확인: dimpl 0.7.5(2026-10-03)·str0m 0.24.1(2026-10-03) 나옴, redb·sctp-proto는 새 판 없음 | 구현(7절 절차로 새 판 검토, vendor 규칙) |
| 23 | 10.2, 10.3 | Join 전 대기 32, 엄격한 ClientData JSON, 핑 모드 뒤 숫자 | 대기 |
| 24 | 2.2, 10.2 | 공식 클라이언트가 RakNet으로 들어오는지(BDS 1.26.52는 "NetherNet 없이 접속 불가") | 대기 |
| 25 | 3 | Snappy "AVX2 없는 기기에서 크래시" 보고 | 대기 |
| 26 | 10.6 | 스폰 PlayStatus 시점(BDS 95개 뒤), 틱마다 Publisher를 안 보냄, BiomeDefinitionList 순서 | 대기 |
| 27 | 10.4, 10.7 | 교육판 아이템의 크리에이티브 표시, 레시피 ID·`deprecated` 블록 이름의 레시피 책 영향, 레시피 책 재료 목록 | 대기 |
| 28 | 10.8 | 창 열기 지연 2틱, 겹상자 짝 표시, 같은 차원 다른 월드 텔레포트 때 옛 청크 | 대기 |
| 29 | 10.8 C4, C6b | 커스텀 블록 렌더링·마찰 1 − factor·`"none"` 카테고리·속성 상한, 커스텀 엔티티 `rid`·`hasspawnegg` | 대기 |
| 30 | 10.8 | 보스바 색 갱신, CommandRequest 버전, 리소스팩 조각 요청 방식 | 대기 |
| 31 | 2.1 | 비신뢰 데이터 채널 내용 형식 | 대기 |
| 32 | 10.3 | `identity.key` rename 뒤 폴더 fsync(Linux 크래시) | Linux |
| 33 | 사용자 실제 접속(2026-10-04) | 스폰 중 `disconnectionScreen.missingStructureData`로 끊김: 10.6에서 JigsawStructureData를 안 보내기로 한 결정이 틀림. BDS가 스폰 때 보내는데 Anchor가 안 보내는 패킷 전부 다시 대조 | 구현(가장 먼저) |
| 34 | 33번 대조 | 다른 플레이어가 있을 때 BDS가 스폰 창에서 보내는 PlayerLocation·LocatorBar(위치 표시줄). Anchor는 `locatorbar` 게임 규칙을 true로 보내면서 웨이포인트를 안 보냄 | P6로 BDS 값 확인 후 구현 |
| 35 | 사용자 실제 접속(2026-10-04, 재접속 성공 뒤) | 물 밖에서도 공기 방울 표시가 계속 뜸(피해·움직임 문제 없음): Breathing 플래그·AirSupply·MaxAir와 물속 전환을 BDS P6 기록과 대조 | 닫음(Breathing 플래그를 안 보냈음, 10.7 M9 공기). 남은 P6: 다른 플레이어 AddPlayer의 Breathing, 리스폰 데이터, 익사 피해 ActorEvent 데이터(BDS 9, Anchor 0) |
| 36 | 사용자 보고(2026-10-04) | `[patch.crates-io]`는 가장 바깥 프로젝트에서만 적용되어, anchor를 path·git 의존성으로 쓰는 사용자 프로젝트에는 vendor 사본이 아니라 crates.io 원본(결함판)이 들어감. 설정 없이 사본을 쓰게 바꾸고, 바깥 임시 프로젝트의 Cargo.lock으로 시험 | 구현 |
| 37 | 가져오기(M9) 실측 | PMMP가 읽는 기준 상태 중 4,884개(PMMP의 dummy·unused 속성: 호박 `cardinal_direction`, 매단 간판, 압력판 신호, 유약 테라코타 방향 0/1 등)에 Anchor 상태가 없어, 가져온 그런 블록이 알 수 없는 블록이 됨(실측에서는 호박) | 구현(덤프·gen이 PMMP 역직렬화 결과 표를 만들고 상태 조회가 그 표를 봄) |
| 38 | FIELDCAPS(M9) | Mojang 상한상 PlayerList 1,000개·Text 매개변수 4개 등: Anchor가 보내는 쪽에서 넘으면(설정 `max-players`는 10,000까지) 클라이언트가 거부할 수 있음. 문자열 상한 단위(바이트/글자) | 닫음(보내는 쪽 상한), 단위는 P6V |
| 39 | PMMP-D(M9) | `Stack::tag`가 타입의 고정 NBT로 스택의 `Fireworks`를 덮어써, 크리에이티브에서 꺼낸 폭죽이 폭발 정보를 잃음 | 닫음(PMMPB) |
| 40 | 재개 전 관찰 | `tests/server.rs::a_shutdown_from_another_thread_ends_run`의 "5초 안에 끝남" 벽시계 판정이 부하 때 1.2~12.5초로 흔들려 실패 | 구현(근본 원인) |
| 41 | 사용자 결정(2026-10-04, 7번은 2026-10-05에, 6번은 M9 후속에 뺌) | 범위 수정: 부수기 시간 강제 설정, 주민 소환 빈 오퍼, status 명령, 기본 콘솔 로그와 수준 설정, 날씨·번개 API, 숫돌. 7번 사용자 정의 효과·인챈트는 안 함(사용자 결정: 클라이언트가 새 효과·인챈트를 표시할 수 없어 뺌). 6번 숫돌은 안 함(사용자 결정 2026-10-05) | 구현 |
| 42 | 사용자 결정(2026-10-04) | 설정 스위치(끄면 코드가 돌지 않음)와 새 설정 키(8.5 정본) | 구현 |
| 43 | 사용자 결정(2026-10-04) | 저작권: 다른 서버의 문장·형식·주석·코드 덩어리를 Anchor 고유로 바꿈, 바꾼 목록 | 구현 |

#### 운영 기능 설계
- 재생 도구(1, 2번): 입력 로그 형식 7은 구간을 지킨다. 구간 머리 = (시작 틱, 체크포인트). 로그 머리에 설정 지문(`tick::Settings`와 월드 명세)과 등록 지문(명령, 커스텀 아이템·블록·엔티티 이름, 이벤트별 리스너 수, 작업 주기)을 `build()` 때 한 번 계산해 넣는다. 체크포인트는 스냅샷을 뜰 때(`log.snapshot` 직후)와 프로세스 시작 때 만들고, 지형을 뺀 `State` 전부를 담는다(지형은 커밋된 DB나 생성기가 준다). `tick/checkpoint.rs`의 `State::checkpoint`·`restore`는 `State::hash`처럼 모든 필드를 이름으로 분해한다(새 필드는 컴파일 실패). 조명 니블은 COW `Arc`로 바꿔 틱에서는 복제만 하고 크래시 로그를 쓸 때 인코드한다(성능은 실측, 비싸면 저장 틱에 풀에서). 런타임 클로저(예약 작업, 작업 끝 처리기, 폼·메뉴 처리기, defer)와 타입 없는 Extensions는 인코드할 수 없어 개수를 세고, 0이 아니면 그 구간은 "재생 불가"로 표시해 재생이 거부한다(추측하지 않음). 커밋된 구간은 64 MiB 상한이 자리를 요구할 때만 오래된 것부터 버린다. 재생은 `data/world.redb`와 `data/backups/*`를 읽기 전용으로 열어 meta 틱이 구간 시작과 같은 DB를 고른다. 공개 API `Server::replay(log) -> Result<Replay>`(`replay::End::{Completed, Truncated, Panicked { tick, message }}`), 같은 `ServerBuilder` 등록을 쓴다. 런타임을 만들지 않고(블로킹 작업은 기록된 결과를 넣음), 커밋하지 않고, 파일을 쓰지 않는다. 클로저 안 상태는 재생에서 처음부터 시작하고 외부 효과는 다시 일어난다(rustdoc).
- 로그 상한(3번): 커밋되지 않은 구간의 입력 바이트가 상한의 1/4에 닿으면 자동 저장이 꺼져 있어도 스냅샷을 요청한다(메모리 상한과 같은 규칙, `/save-off` 문서). 나머지 3/4가 커밋 시간(실측 24~802 ms)을 덮는다.
- 백업(5번): 시작 백업은 임시 파일 → 동기화 → rename으로 쓰고(잘린 백업이 백업 3개에 세어져 좋은 백업을 지우던 결함), 시작 때 남은 `.tmp`를 지운다. 실행 중 백업 `Context::backup()`과 기본 명령 `backup`(op): 다음 스냅샷을 커밋한 뒤 저장 스레드가 읽기 트랜잭션 하나로 표를 8 MiB 단계씩 `backups/world-<초>.redb.tmp`에 복사하고(단계 사이에 요청을 처리) 다 복사하면 지속 커밋 한 번·닫기·동기화·rename, 3개 유지. 단계 커밋은 동기화하지 않는다(`Durability::None`, M9 후속 R2: 단계마다 동기화하면 바쁜 디스크에서 한 단계가 최대 37초 막혔다). 단계의 동기화는 redb가 파일을 늘릴 때(크기 두 배, 4 GiB 영역마다)뿐이고, 복사가 끊기면 `.tmp`라 백업으로 세지 않는다(시험 `an_online_backup_step_syncs_only_to_grow_its_file`: 고치기 전 7단계 13번, 뒤 성장 6번만). 실패하면 `.tmp`를 지우고 error, 서버는 계속(틱 입력 아님). PMMP에는 백업 명령이 없어 이름은 Anchor가 정한다.
- 저장 시작 비용(6번): 256 MiB·1 GiB·4 GiB 파일로 복사·`check_integrity`·`compact`·실행 중 백업을 재는 `#[ignore]` release 시험을 두고 결과를 적는다. 너무 느리면 1 MiB 복사 버퍼, 지난 종료가 compact했으면 시작 compact 생략(무결성 검사는 생략하지 않음).
- 정상 종료(5번): 신호는 Ctrl+C에 더해 Unix SIGTERM·SIGHUP, Windows Ctrl+Break·창 닫기·로그오프·종료(창 닫기는 약 5초만 허락: rustdoc)를 모두 같은 `ShutdownHandle::shutdown()`으로. 두 번째 인터럽트는 PMMP처럼 즉시 종료(error 기록 뒤 종료 코드 130, 강제 종료 시험이 안전을 보임). 종료 메시지: 아직 Join하지 않은 세션과 틱이 처리하지 않은 Join의 세션도 세션 층이 Disconnect(종료 메시지)를 보낸다. RakNet 구동부는 닫기 표시 전에 큐의 명령을 먼저 처리하고, NetherNet은 종료 때 명령을 처리한 뒤 피어마다 `CLOSE_LINGER`까지 남긴다(두 전송을 동시에 멈춤). 틱 패닉 뒤 저장 스레드는 큐에 남은 스냅샷을 커밋하지 않고 compact하지 않는다(4절 6번: 정상 종료는 `Request::Finish`로 알림). 앞 커밋이 실패한 뒤의 커밋은 거부한다(원자성). 종료 직전 quit 리스너의 defer도 마지막 스냅샷 전에 돈다. Windows Ctrl+C는 `unsafe` FFI 없이 시험할 수 없어 시험은 핸들과 콘솔 `stop`으로 한다.
- 콘솔(5번): PMMP처럼 줄마다 터미널 이스케이프와 제어 문자를 지운다. 그 밖의 PMMP 콘솔 기능은 이미 있다.
- 로그인 뒤 Join 전 세션 상한: 로그인 검증을 마친 세션은 로그인 전 자리(64·IP별 4)를 돌려주므로 그 뒤 Join까지는 상한이 없었다. 세션 층에 "로그인했고 아직 Join하지 않은" 세션의 세마포어(크기 = 시작 때 `max-players`)를 두고, 모자라면 PlayStatus 7(서버 가득 참)로 끊는다(PMMP도 로그인 때 판정). 실행 중 `set_max_players`는 틱 상태라 이 크기에 반영되지 않는다(4.2).

#### 가져오기 설계 (4번)
- LevelDB 리더는 직접 쓴다(읽기 전용, 약 1,100줄). rusty-leveldb는 열 때 원본 폴더에 쓰고(LOCK, 로그 재생, compaction), 디스크 입력 경로에 `unsafe`와 snap(9.2에서 거절)이 있어 6절 3번을 못 지킨다. 범위: CURRENT → MANIFEST 버전 편집 → 살아 있는 표(발자국, 색인, 데이터 블록, 재시작 지점, 압축 바이트, 마스크 CRC-32C)와 MANIFEST 로그 번호 이상의 로그(32 KiB 블록, 쓰기 묶음), 내부 키(시퀀스·종류). 압축 0 없음, 1 Snappy, 2 zlib, 4 raw zlib, 3(zstd)은 오류. 상한: CURRENT 256 B, MANIFEST·로그 64 MiB, 파일 65,536개, 블록 해제 16 MiB, 키 1 KiB. 가져오는 동안 원본 파일(이름, 길이, 수정 시각)이 바뀌면 실패.
- 월드 형식: 오버월드 키만(네더·엔드는 1절 범위 밖, 세어 보고), 태그 Version, SubChunkPrefix(v0·v2~7 레거시 id+meta, v1, v8, v9), LegacyTerrain(0~2판, PMMP도 읽음), Data3D·Data2D(바이옴 번호 그대로), BlockEntity, Entity와 `digp`/`actorprefix`, PendingTicks(Anchor 예약 갱신 형식으로, x/y/z는 월드 절대 좌표: BDS 1.26.52 픽스처와 dragonfly mcdb로 확인), level.dat(시드, 시간, 스폰; SpawnY 32767이면 그 열 최고 블록 + 1). 1.18 이전 월드의 y<0은 공기(PMMP와 같음). 플레이어는 가져오지 않는다(XUID가 없음, PMMP도 플레이어를 따로 둠).
- 블록 상태 업그레이드: pmmp/BedrockBlockUpgradeSchema 5.3.0(CC0, PMMP의 5.2.0에는 1.26.30 변경이 빠짐)을 `cargo xtask dump`가 `xtask/data/upgrade/`로 옮기고 `gen`이 표와 해석기를 만든다. 아이템 이름도 PMMP처럼 아이템 업그레이드 스키마로 올린다. 스키마 결함 검사: 루트에 있는 `0351_1.26.40_to_1.26.50.json`을 따로 읽고 손으로 쓴 `xtask/data/block_upgrade.json`과 같은지 본 뒤 손 파일을 지움(같은 사실 한 곳), 0321부터 출력 버전이 같아 최신 상태도 다시 통과하므로 기준 목록 22,091개가 그대로 돌아오는지 gen이 확인, 같은 id가 이름 바꾸기와 평탄화에 함께 있으면 gen 실패, 순서에 따른 부분 일치는 PMMP PHP 골든으로 덮음, 모르는 레거시 값은 meta 0으로 바꾸지 않고 원본을 알 수 없는 블록으로 보존.
- 저장: 청크는 `save::to_record`·`seal`을 그대로 거쳐 Anchor가 쓰는 바이트와 같다. 대상 월드는 서버가 선언한 영구 월드이고 `worlds` 표에 아직 없어야 한다(두 번째 실행은 거부, 아무것도 안 바꿈). 쓰기 트랜잭션 하나로 커밋, 청크 하나라도 디코드에 실패하면 전체 중단, 원본은 쓰지 않는다. 큰 트랜잭션의 메모리는 10만 청크로 실측.
- API: `Server::import_world(dir, world_name) -> Result<import::Report>`(`run()` 전, 틱 밖, 자체 CPU 풀을 만들고 끝에 기다림, alias `FormatConverter::execute`), `Error::Import`.
- 시험: 리더는 무작위 쓰기·지우기 모델 대조(고정 시드 1,000회), Go 도구(df-mc/goleveldb, 개발 때만)와 해시 대조, BDS가 만든 월드를 가져와 BDS가 보낸 서브청크와 블록 단위 비교(P6 `import`), 모든 입력 종류의 변형 1만 개 패닉 0, 장애 주입 시 파일 그대로, 원본 해시 그대로, 풀 1·N 같은 바이트.

#### 대응표 재검토 결과 (10번)
"안 함" 5,029행 중 약 210행이 잘못 빠졌다(PMMP에 있고 1절 범위 안). 구현한다: 실행 중 월드 로드·언로드·생성·기본 월드 바꾸기와 WorldInit·Load·Unload 이벤트(4.1은 등록만 고정한다, M8 A6의 "안 함"은 묻지 않은 범위 축소였음), 시간 정지, 청크 붙들기(청크 로더), 로그인·세션 값(로케일, 입력 방식, 기기, 버전, 핑, 포트: 로그에 든 입력으로), 플레이 기록(처음 접속 여부, 처음·마지막 접속 시각), 리스폰 앵커 폭발(PMMP는 어느 월드에서나 폭발)과 BlockPreExplode·BlockExplode·PlayerRespawnAnchorUse, 서리 보행과 EntityFrostWalker, 책 편집과 PlayerEditBook, 엔티티 집기와 PlayerEntityPick, WorldSound·WorldParticle 이벤트(규칙 코드의 소리·입자를 한 곳으로), 양조·석재 절단기·대장장이 레시피 등록, 크리에이티브 인벤토리 바꾸기(빌드 때), AI 없는 좀비와 아기 표시(PMMP 좀비는 AI 없음), 네트워크 제어(서버 이름, IP 차단, 연결 수), enum 데이터 메서드 7개, 인벤토리를 보는 사람, 월드별 난이도, TPS·MSPT(틱 루프가 잰 값을 로그에 든 입력으로), 나무 키우기 API. status.txt의 틀린 이유 글도 고친다.

#### 검토 결과 요약 (2단계)
검토 9개 영역에서 D(결함) 37, K(체크리스트) 33. 반박 뒤 살아남은 것만 고친다(기각: R6-K4 거래 오퍼 게터). 영역별 판정과 수정 방향의 정본은 진행 상태에 적는다. 주요: 제작 칸 바꿔치기로 아이템 복제(R6-D1), 배고픔 소모 무한 루프(R6-D3), 인챈트 입력 바꿔치기(R6-D4), 셜커 닫기 리스너 복제(R5-D3), 엔티티 크기 × 배율 상한 없음(R5-D2), 한 청크 레코드 16 MiB 초과 시 저장 실패로 서버 종료(R4-D1, R3-D5), 로그인 뒤 Join 전 세션 무제한(R2-D1), RakNet 조각 비용 미계산(R1-D2), SCTP 원격 스트림 수 무제한으로 구동 작업 정지(R8-D2), 크래시 로그가 패킷 입력을 적용 뒤에 기록(R3-D1), 실패한 커밋 뒤 커밋(R3-D4), P6 비교가 차이를 가림(R9-D1·D2), 흔들리던 시험 3개의 원인(R3-K5 시험이 청크 로드 전에 셈, R3-K6 벽시계 창, R3-K7 시그널링 리스너보다 핑 응답이 먼저).
- R1-K2(필드별 목록 상한, 3절·10.1의 약속이 구현되지 않음): 설계대로 Mojang bedrock-protocol-docs의 `maxItems`·`maxLength`를 생성기가 읽어 만든다.
- R9-K2(vendor 사본 자체 시험): redb `--tests`, str0m(Anchor 패치 조합으로), dimpl(openssl-src 빌드에 Strawberry Perl을 개발 도구로 `.cache/tools`에) 시험을 verify에 더한다.

#### 범위 수정과 설정 추가 설계 (사용자 결정, 2026-10-04)
- 원칙: 설정에 없는 동작은 가장 성능적이고 안전한, 어떤 서버에서도 맞는 기본값이다. 끌 수 있는 기능은 끄면 그 코드가 아예 돌지 않는다(틱 작업, 이벤트, 패킷, 메모리 0)이고, 끈 상태에서 코드가 실행되지 않음을 시험으로 확인한다(규칙 함수가 호출되지 않음을 세는 시험 훅이 아니라, 상태·출력·이벤트가 생기지 않음과 그 기능의 표가 비어 있음으로). 새 키 목록과 기본값은 8.5 정본.
- 기능 스위치의 뜻: `experience`(경험치를 얻는 것 자체: 점수·레벨·구슬. 끄면 구슬도 없고 경험치 속성·이벤트를 보내지 않음, 인챈트대·모루 비용은 레벨 0으로 보아 레벨이 드는 일은 거부), `experience-orbs`(옛 `experience-bottles`를 대신함, M9 후속 U6: 경험치 구슬과 경험치 병 던지기. 끄면 구슬만 없어짐: 떨굴 경험치를 셈하지 않고(블록·화로·좀비) 구슬을 만들거나 저장된 구슬을 읽지 않으며(`drop_orbs` 한 곳과 청크 로드) 병 던지기를 거부; 구슬 대신 경험치를 바로 주지 않음(결정: 단순한 쪽), 점수·레벨·인챈트대·모루·`/xp`는 `experience`만 따름. 규칙은 `tick::Settings::has_orbs` = `experience` 그리고 `experience-orbs`), `fall-damage`, `drowning`(공기·익사·질식: 끄면 공기 값을 세지 않고 Breathing은 늘 켬), `armor`(방어 계산과 갑옷 내구도 소모·가시), `durability`(M9 후속 U7: 끄면 도구·무기·갑옷·활·삼지창 등 모든 아이템의 내구도가 줄지 않음. 내구도를 깎는 모든 규칙이 지나는 `interact::breaking::damage` 한 곳에서 막아 계산·난수 뽑기가 돌지 않음; `armor`가 켜져 있으면 갑옷은 막아 주되 닳지 않고, `armor`를 끄면 방어·갑옷 소모·가시가 모두 없음), `beds`(수면과 침대 스폰 지점: 끄면 침대는 장식 블록), `daylight-cycle`(시간이 흐르지 않음, 클라이언트 시계도 멈춤), `recipe-book`(끄면 게임 규칙 `recipesunlock` 등으로 레시피 책을 비우고 레시피 책 빠른 제작(CraftRecipeAuto)을 거부, 제작 칸 제작은 됨 — BDS P6로 확인), `vanilla-recipes`(끄면 바닐라 레시피를 표와 CraftingData에 넣지 않음), `break-time-check`(끄면 클라이언트의 부수기 완료를 시간 검사 없이 받음; 도달 거리·블록 종류 검사는 그대로), `movement-check`(15블록 검사), `backups`(시작 백업과 backup 명령), `hardcore`(이미 있음).
- `lighting`: "dynamic"(지금), "on-load"(청크 로드 때 계산, 블록 변경 뒤 갱신 없음: 변경 큐·BFS가 돌지 않음), "fixed"(계산 없음, 하늘빛 `fixed-light-level`·블록빛 0). 옛 bool 값도 받는다.
- 월드별 값: `[worlds.<이름>]` 표가 그 월드의 `view-distance`·`chunks-per-tick`·`spawn-radius`·`tick-radius`를 덮는다(빌드 때 선언된 월드 이름만, 아니면 시작 거부). 같은 값을 `world::Options`와 `ServerBuilder::world` 옵션으로도 준다.
- `slow-empty-worlds`: 플레이어가 없는 월드는 100틱에 1틱만 블록 갱신·엔티티·블록 엔티티·시간을 돌린다(예약 작업과 이벤트 콜백은 서버 단위라 그대로). `item-despawn-ticks`, `item-pickup-interval`, `sprint-critical-hits`(PMMP는 달리는 중 치명타 없음 = 기본 false), `hook-movement-packets`·`hook-player-input`(PMMP DataPacketSend/ReceiveEvent에 해당하는 훅을 이 패킷만 건너뜀; 기본은 지금처럼 거침).
- `enable-encryption = false`: RakNet 로그인에서 ServerToClientHandshake와 암호화를 하지 않는다(NetherNet은 원래 Minecraft 암호화가 없음). 위험은 설정 주석에 적음.
- 로그(4번): Anchor가 자기 콘솔 출력기를 둔다. 전역 상태 없이(CLAUDE.md 4.1) `tracing` 디스패치를 Anchor가 만든 스레드(틱, 저장, 런타임 작업자, 콘솔, CPU 풀, `run`을 부른 스레드의 실행 동안)에만 건다. 수준은 `log-level`, 사용자 바이너리는 `ServerBuilder`로 끄고 자기 구독자를 쓴다. 기본 로그 항목: 시작·종료, 여는 주소와 포트, 월드 로드·언로드, 접속·퇴장, 경고·오류. 문장과 형식은 Anchor 고유. 새 크레이트 없이 쓴다(필요하면 6·7절 절차).
- status(3번): 가동 시간, TPS·MSPT(평균·최근), 프로세스 메모리, 스레드 수(틱·저장·네트워크·CPU 풀), 접속자, 월드별 로드된 청크·엔티티·블록 엔티티 수. Anchor 고유 형식.
- 날씨와 번개(5번): 월드별 날씨 상태(맑음, 비·눈, 천둥; 지속 시간 없음, 자연 순환 없음)를 `World`/`Context` API로 바꾸고, 바뀌면 그 월드의 플레이어에게, 접속·월드 이동 때 그 플레이어에게 BDS와 같은 패킷(LevelEvent 등, P6)으로 보낸다. 번개는 `Context::strike_lightning(world, pos, options)`(보이는 것과 소리는 늘, 피해·불은 선택)로 BDS의 번개 엔티티 패킷 모양. BDS처럼 피해는 모든 난이도, 불은 normal·hard에서만(P6 `boltpeaceful`·`bolteasy`·`bolthard`). 모든 날씨·번개 패킷은 송신 훅을 거친다. 저장: 날씨 상태는 월드 기록(형식 올림).
- 숫돌(6번): 안 함(사용자 결정 2026-10-05, M9 후속 R7: PMMP에 없는 블록이라 다른 PMMP에 없는 블록과 같게 둠. 구현했던 블록, 아이템, 갈기 규칙, 시험, P6 시나리오, 생성기의 사용자 예외 파일 `xtask/data/exceptions.json`과 그 경로를 지움).
- 사용자 정의 효과·인챈트(7번): 안 함(사용자 결정 2026-10-05: 클라이언트가 새 효과·인챈트를 표시할 수 없어 뺌; 다른 Claude 세션이 사용자 대신 전함).
- 주민(2번): 생성 알·명령·API 소환 기본 오퍼는 빈 목록(바닐라 거래 템플릿 없음).
- 저작권: 위 8.4. 감사로 목록을 만들고(읽기 전용) 고친 뒤 바꾼 목록을 보고에 적는다.

#### M9 판정식
- 빌드 속도: 남긴 변경마다 전후 verify 시간과 PASS.
- 검토: 영역 9개 보고, 발견마다 반박 결과, 살아남은 수정마다 고치기 전 실패·뒤 통과 시험. 남은 지적 0.
- 운영: 재생 도구가 강제 종료한 자식의 크래시 로그(커밋 뒤 구간)를 커밋된 DB와 체크포인트로 재생해 크래시 전 기록 해시와 같다. 로그 상한 근처에서 커밋이 요청되어 구간이 잘리지 않는다. 가져오기: BDS가 만든 월드를 가져와 블록이 BDS가 보내는 서브청크와 같고, 깨진 입력 변형(고정 시드)에서 패닉 0. 정상 종료: 콘솔 `stop`·`ShutdownHandle`에서 모든 플레이어가 종료 메시지를 받고 마지막 커밋 뒤 모든 스레드가 끝난다.
- M8 남은 위험: 흔들리던 시험 3개의 근본 원인과 수정(재시도·sleep·시간 늘리기 없음), 결정성 시험 시간 전후, 1명 p99 전후 또는 양보 이유.
- 대응표: `table --check` 빈칸 0, 잘못 빠진 API 구현.
- 검증 필요: 위 표의 P6 항목을 모두 닫고 결과를 적음. 대기·Linux 목록만 남음.
- 마지막: `cargo xtask p6 bds`·`p6 anchor`·`p6 api` 전체 PASS, `cargo xtask verify` PASS, 3절 체크리스트 감사 지적 0.

#### M9 진행 상태
- 설계를 적음(2026-10-04). 검토 R1~R9, 설계 조사(가져오기, 운영), 대응표 감사를 읽기 전용으로 띄움. 기준 verify 시간 측정 중.
- 검토 R1~R9 끝, 영역마다 반박 서브에이전트가 확인(기각 1: R6-K4). 판정과 수정 방향은 서브에이전트 작업 파일에 두고 수정 단계에서 쓴다.
- 사용자 실제 접속 보고(2026-10-04): `missingStructureData`로 끊김 → 스폰 창에 JigsawStructureData·SyncActorProperty 13개·카메라 3개·CurrentStructureFeature를 BDS 위치·내용으로 보냄(10.6 바뀐 결정), `p6 bds`·`p6 anchor` PASS, release lobby 예제를 다시 빌드해 사용자에게 알림. 남은 일: 다른 플레이어가 있을 때 BDS가 보내는 PlayerLocation·LocatorBar(위치 표시줄, `locatorbar` 규칙은 true로 보냄), 사용자 재접속 결과.
- 수정 1차(worktree): TICK, GAMEA, GAMEB, NET, SESS, API. GAMEB 합침(R6-D1·D2·D3·D4·D5·K1 일부·K2·K3, 빌드·clippy·영역 시험 258개 통과).
- TICK 합침(R3-K5 시험 경쟁 수정: 스폰 청크 로드 뒤에만 셈·커밋 이벤트로 죽이는 시점, R3-D1 적용 전 기록(`Cow`), D2 모든 떠남에 PlayerQuit, D3 종료 defer, D4 실패 뒤 커밋 거부, D6 PlayerList `Arc` 공유, K1 설정 해시, K2, K3 이름·UUID 색인, K4 step 단계 표시로 bench 사본 삭제, R4-D3 묶음 읽기 실패는 다시 열어 3번 재시도 뒤 저장 없이 멈춤·입력 로그 형식 7 `LoadFailed`). NET 합침(R1-D2 `PART_COST`, D3 다른 피어 주소 빼앗기 거부·`Connection::addr`는 받아들인 DTLS 출발지, K1 UDP 구동 공통부 한 곳·`clock::token_now`, R2-D2 RakNet 닫기 전 명령 처리, D3 NetherNet 종료 linger, R8-D1·D2(받는 스트림 16)·D3·K1). GAMEA 합침(R5 전부, R6-K1 연결, R7-D3; R5-D5의 아이템 돌려주기는 판정의 규칙별 거부 대신 `return_items` 한 곳에서 발밑 spill로: 모든 경로를 한 규칙으로). 합친 상태 lib 시험 1,316개 통과(흔들리던 `forced_kills_keep_user_data_with_its_tick` 포함).
- 사용자 두 번째 보고(재접속 성공, 물 밖 공기 방울 표시) → 수정 서브에이전트. 수정 2차: SESS, API, WORLD, TOOLS.
- API 합침(R7-D1 같은 enum 이름에 다른 값이면 빌드 거부 `ConflictingEnum`, D2 기본 명령 kick·ban·whitelist·gamemode·tp·transferserver가 `perform`으로 이벤트를 냄(새 `action::Teleport`; 성공 줄은 리스너보다 먼저), K1 단위 접미사 `_ticks`, K2 PlayerDataSave 취소는 무시하는 손 메서드·대응표 "다름" 3행, K3 `SignChange::side`). SESS 합침(R2-D1 로그인 뒤 Join 전 세션 세마포어·PlayStatus 7, D4 Join 전 세션에도 종료 메시지, D5, D6 백업 임시 파일·남은 `.tmp` 정리, K1, K2 `fs::write_atomic`(폴더는 만들지 않음: 설정 파일 경로 오류 동작 유지), K3 인증 문서 세 요청이 마감 하나, R3-K6 시험, R3-K7 NetherNet을 RakNet보다 먼저 엶, 모든 종료 신호, 두 번째 신호는 종료 코드 130, `Request::Finish`, 콘솔 이스케이프·제어 문자 제거(PMMP가 줄 끝 `~`까지 지우는 결함은 고침), `tests/shutdown.rs`). WORLD 합침(R4-D1 저장 형식 4(6절), D2 이름 길이 `Error::NameTooLong`, K1 저장 스레드가 커스텀 블록을 풀에서 해석, K2, K3, K4 하늘 높이 증분·훑은 칸만큼 예산; R4-D1의 항목 하나 4 MiB는 설계가 필요해 따로). TOOLS 합침(R9-D2, K1 `xtask/tests/go.rs`, K2 vendor 자체 시험(redb `--tests`, dimpl, str0m은 사용자 결정으로 lib만), K3 금지 목록 56줄, K4). DOWNSTREAM 합침(vendor path 의존성, `[patch]` 삭제, 바꾸지 않은 사본 3개, `xtask/tests/downstream.rs`, verify `fmt --check`는 `--all` 없이: `--all`은 path 의존성인 vendor 사본까지 포맷함). 공기 방울 수정(메인에서, 10.7 M9 공기) → 표 35번 닫음, release lobby 다시 빌드.
- 사용자 결정(2026-10-04): str0m 시험 캡처 7개(5.4 MB)는 커밋하지 않는다. verify는 str0m lib 시험만, 통합 시험은 사본을 바꿀 때만 돌리는 xtask 명령(상류 95e5038c에서 `.cache`로 받고 sha256 확인)으로 두고 M9에서 한 번 돌려 9.1에 적는다. 캡처는 `.cache/str0m-test-data`로 옮겼다.
- 가져오기 합침(IMPORT-R 리더·`level.dat`·청크 디코드·Go 대조 도구·BDS 픽스처 `xtask/data/import/flat`, IMPORT-U 업그레이드 스키마·gen·해석기·`Server::import_world`·`Storage::begin_import`·`p6 import`): 시험 84개, `p6 import` 67개 서브청크 블록 단위 다른 블록 0(알 수 없는 블록만 차이, 그중 호박은 표 37번), 10만 청크 release 실측 변환·쓰기 19.7초·커밋 3.9초·compact 13초·최대 메모리 98 MiB(큰 트랜잭션이 메모리에 쌓이지 않음 → 검증 필요 닫음). BDS 실측으로 닫은 형식 사실: 오버월드 키 x·z LE i32 + 태그 + 서브청크 y, 표 블록 압축 4, MANIFEST 태그 1~7·9(5 포함), 청크 버전 42·서브청크 v9, `level.dat` 버전 10. 검증 필요로 남음: 네더·엔드 키 배치(픽스처에 없음, 범위 밖이라 세기만), 압축 1·2 블록(BDS가 안 씀, 인코더로만 시험), 레거시 형식 v0~v8·LegacyTerrain·태그 52(BDS 1.26.52로는 못 만듦, PMMP 코드·chunkutils2 골든 근거).
- NORMALIZE 합침(표 37 닫음): 덤프가 기준 상태 22,091개를 PMMP 역직렬화기로 읽어 `xtask/data/pmmp/deserialized.json`(다르게 읽는 4,884, 거부 8,211)에 쓰고, gen이 별칭 표(4,884행, 이분 탐색)를 만든다. `State::read`는 정확한 조회 뒤 별칭 표를 본다. `save::find_state`가 쓰므로 가져오기·디스크 팔레트·아이템 블록 NBT·낙하 블록·화분이 덮인다. 보내는 상태는 그대로. gen은 PMMP가 읽는 상태에 Anchor 상태가 없으면 실패. `p6 import` PASS(호박 7개를 별칭으로 셈, 다른 블록 0), intended.txt의 호박 줄 삭제.
- REPLAY 합침(표 1·2·3 닫음): `Server::replay(log) -> replay::Result<Replay>`(`anchor::replay`: `Replay`, `End::{Completed, Truncated, Panicked}`, 오류 `NoMatchingWorld`·`SettingsDiffer`·`RegistrationsDiffer`·`Opaque`·`Diverged`·`Format`·`Storage`·`Io`). 입력 로그 형식 8(구간마다 시작 틱·커밋 표시·체크포인트, 머리에 설정·등록 지문 SHA-256, 끝에 CRC-32C; 4~7도 읽음). `tick/checkpoint.rs`는 모든 필드를 이름으로 나열(새 필드는 컴파일 실패). 설계와 다르게 지형도 체크포인트에 넣음: DB에서 읽으면 팔레트가 압축되어 실행 중 청크와 바이트가 달라져 재생이 갈라졌다(실측 tick 27). 로드된 청크와 조명 니블은 `Arc`로 공유해 두었다가 크래시 로그를 쓸 때 인코드(조명 release 벤치 손해 없음: 켬 0.708/1.825 → 0.537/1.636 ms). 클로저·Extensions·커스텀 쿨다운 태그가 있으면 그 구간은 재생 불가로 표시하고 거부. 커밋된 구간은 상한이 자리를 요구할 때만 버림, 커밋 뒤 입력이 상한의 1/4이면 자동 저장이 꺼져 있어도 스냅샷(재생 쪽은 기록된 구간 시작을 따름). 재생은 `world.redb`와 백업을 새것부터 읽기 전용으로 열고 meta 틱이 구간 시작과 같은 파일을 고름, 런타임·네트워크 없이, 파일을 쓰지 않음.
- SMALL 합침: `cargo xtask vendor-test str0m`(상류 커밋은 `.cargo_vcs_info.json`, 캡처는 `.cache/str0m-test-data`에 없으면 curl로 받고 늘 sha256 확인(`xtask/data/vendor/str0m-test-data.sha256`), `tests/data`에 복사해 돌린 뒤 지움), M9 실행 1,078개 통과·실패 0·바이너리 67개·약 220초(빈 target 빌드 포함). 표 38 닫음: 보내는 쪽 Mojang 상한은 생성기 `SEND_LIMITS`가 `types.rs` 상수로 만들고(표에서 사라지면 gen 실패), PlayerList는 1,000개씩 나눠 보냄(1,000 이하는 예전과 같은 패킷 하나, P6 같음), 보스바 제목 256바이트 넘으면 `display::Error::TooLong`, 명령 설명 1,000바이트 넘으면 빌드 거부(`command::Error::DescriptionTooLong`). 나머지 보내는 필드는 이미 상한 안. 남은 위험: 공개 `protocol`로 사용자가 직접 만든 패킷은 상한 검사를 거치지 않음.
- PMMP-D 합침: 레시피 종류 `Recipe::{stonecutter, smithing_transform, brewing, brewing_container, shapeless_type}`(석재 절단기·대장장이는 PMMP 레시피 뒤 번호로 O(1), 양조는 PMMP 순서: PMMP 용기 → 서버 용기 → PMMP 물약 → 서버 물약, 재료 이름 색인; smithing_trim은 PMMP에 클래스가 없어 안 함), 크리에이티브 `custom::{CreativeInventory, CreativeGroup, CreativeItem}`·`ServerBuilder::{creative_inventory, alternative_creative_inventory}`·`Context::{set_creative_inventory, creative_inventory, creative_items}`(빌드 때 고정·시작 때 한 번 인코드, 바꾸지 않은 기본 인벤토리는 바닐라 패킷과 같음; 커스텀 묶음이 이제 PMMP `CreativeInventoryCache` 규칙 그대로), enum 데이터 `DyeColour::rgb`·`block::{CopperOxidation, BrewingStandSlot, ChiseledBookshelfSlot}`(`RecordType::getSoundId`는 PMMP에서 폐기, 늘 0 → 안 함), `Context::block_inventory_viewers`, status.txt 이유 고침. 표: 구현 1,690, 다름 3, 병합 3,539, 안 함 4,985, 빈칸 0. 체크포인트에 `Inventory.creative`를 더함(병합 때). 찾은 결함(표 39): `Stack::tag`가 타입의 고정 NBT로 스택의 `Fireworks`를 덮어써 크리에이티브 폭죽 31종이 폭발 정보를 잃음.
- PMMP-A 합침(대응표 감사 1~3·월드별 난이도): `Context::{create_world, load_world, unload_world, force_unload_world, is_world_generated, is_world_loaded, set_default_world}`, `world::Options`, `ServerBuilder::generator`(이름으로 등록, flat·void 기본), 이벤트 `WorldInit`·`WorldLoad`·`WorldUnload`(뷰 단계, 언로드 취소 가능, 강제면 무시; PMMP 언로드 콜백은 `WorldUnload`에 병합), 언로드 월드는 인덱스·Id 유지(상한 1,024), 언로드 때 플레이어는 기본 월드 안전 스폰으로·청크는 보통 경로로 내리고 같은 커밋에 원자적으로, 기본 월드 언로드는 강제여도 거부(PMMP는 기본 월드 없는 상태가 됨 → 고침), 월드 기록은 시작 때 한 번 읽어 틱 메모리(체크포인트 포함)에. 시간 정지 `Context::{stop_time, start_time}`·`World::is_time_stopped`(클라이언트 시계도 paused). 청크 붙들기 `Context::{hold_chunks, release_chunks, chunk_tickets}`·`world::Ticket`(정사각형 반경 8 이하, 티켓 1,024, 합계 4,096청크). 월드별 난이도 `World::difficulty`·`Context::set_world_difficulty`(배고픔·불·StartGame·스폰 창이 씀, 월드 이동 때 SetDifficulty, `/difficulty`는 PMMP처럼 개별 난이도를 지움). 저장 형식 5(월드 기록에 시간 정지·난이도, 4 이하는 흐름·없음으로 읽음). 공개 API 바뀜: `Context::worlds()`가 `ExactSizeIterator` → `Iterator`(언로드 월드를 빼야 해서), `world(id)`·`world_named`는 로드된 월드만, `world::Id::DEFAULT`는 "설정 월드". 10.8 A6의 "안 함"(시간 정지, 월드별 난이도, 월드 로드·언로드·생성, 청크 로더)은 이로써 구현함.
- CRASHTEST 합침: 아이템 흐름 강제 종료 시험과 블록 강제 종료 통합 시험도 벽시계 대신 커밋 이벤트·틱으로 죽이는 시점을 정하고, 아이템 시험은 짝수 실행을 정한 단계에서 커밋하게 해 각 장소 관찰이 구조로 보장됨(고치기 전 부하에서 `[50, 50, 9]`·`only 34 of 50 runs committed` 실패, 고친 뒤 부하에서 3번씩 통과, 일부러 망가뜨린 커밋은 두 시험 모두 잡음).
- ENTRY 합침(R4-D1 나머지, 6절 항목 하나 상한, 사용자가 상한을 승인함; 쪽을 바이트만으로 자르면 바닐라만으로도 다시 못 읽는 청크가 생기던 결함도 고침). 공개 API: `Stack::with_extra`가 `Result`, `item::Error`·`item::Result`·`item::MAX_NBT_BYTES`, `inventory::Error::NotAccepted`.
- 병합 확인: 합친 상태 lib 1,327개 통과(SESS까지). WORLD·DOWNSTREAM 뒤 `tests/server.rs` 병합 오류 1곳(시험 도우미 이름) 고침.
- 강제 종료 뒤 재개(2026-10-04): PC가 꺼져 세션과 서브에이전트가 모두 끊김. 사용자 지시대로 "합침"으로 적힌 것만 끝난 것으로 본다(PMMP-A는 끊기기 전에 합치고 확인까지 마쳐 위에 "합침"으로 적혀 있어 끝난 것으로 둠). 끊길 때 합치던 P6V(검증 필요 닫기)는 적히지 않았으므로 작업 트리에서 되돌림(그 패치를 거꾸로 적용, 손으로 합친 4곳과 체크포인트 필드 3개도 되돌림, 생성 파일 다시 생성). PMMP-B·PMMP-C·BACKUP은 합치기 전에 끊겨 작업 트리에 없음. 점검: 바뀐 파일 371개에 빈 파일·NUL·잘린 소스 없음(복사한 상류 JSON의 끝 줄바꿈 없음은 원본 그대로), 빌드 통과, 전체 verify 실행.
- 재개 뒤 verify: dimpl 시험만 실패(정전·정리로 `.cache`의 개발 도구가 사라져 OpenSSL 빌드에 쓸 Strawberry Perl이 없음). 나머지 검사는 모두 통과. `.cache` 복구: Strawberry Perl 5.42.3.1·PM5 PHP(php-build-524)를 다시 받음(zip sha256이 9.1 기록과 같음), Mojang protocol docs(475bd72)·BedrockBlockUpgradeSchema(4e874a16)·BedrockItemUpgradeSchema(7f551f62)를 다시 받음, `.git`을 잃은 pmmp·dragonfly·cloudburst-data는 그 자리에서 고정 커밋으로 되살림(파일 차이 0), pmmp `vendor`는 `composer.lock`대로 다시 설치. 그 뒤 dimpl 시험 537개 통과, `cargo xtask dump --check` 통과(복구한 입력이 커밋된 데이터를 바이트 그대로 다시 만듦) → 재개 시점 verify의 모든 검사 통과.
- 재개 뒤 서브에이전트 1차(worktree, 작업 지시는 `target/m9work/`): P6V, PMMPB, PMMPC, BACKUP, PERF. 다음: SHUTTEST(표 40), VENDOR(표 22), 그 뒤 M9 새 코드 검토·반박.
- 남은 일(재개 뒤 설계): (1) P6V 다시: 표 11~21·34와 공기 후속(다른 플레이어 AddPlayer·리스폰 데이터의 Breathing, 익사 ActorEvent 데이터), 문자열 상한 단위(Text·CommandRequest), 물약 색표. (2) PMMP-B 다시: 로그인·세션 값, 플레이 기록, 네트워크 제어, TPS·MSPT, 표 39 폭죽 NBT. (3) PMMP-C 다시: 리스폰 앵커 폭발, 서리 보행, 책 편집, 엔티티 집기, 소리·입자 이벤트, 좀비·아기, 나무 키우기. (4) BACKUP 다시: 실행 중 백업, 시작 비용 실측(표 5·6). (5) vendor 새 판(표 22). (6) 성능(표 8·9). (7) 표 40: `a_shutdown_from_another_thread_ends_run`의 5초 벽시계 판정이 부하 때 흔들림(재개 전 두 에이전트가 관찰). (8) M9에서 새로 쓴 코드(가져오기, 재생, 저장 형식 4·5, 월드 수명주기 등)의 검토와 반박, 3절 체크리스트 감사. (9) 마지막 P6 전체, verify, 실제 접속 확인 대기 목록.
- 사용자 범위 수정과 설정 추가(2026-10-04, 표 41~43): 8.4 범위, 8.5 정본(새 키), 10.9 설계, 대응표 로그 행 이유를 먼저 고침. 저작권 감사 두 개(문장·형식, 주석·코드)를 읽기 전용으로 띄움. 구현 순서: 지금 단계(P6V, PMMPB, PMMPC, BACKUP, PERF)를 합친 뒤 CONFIG(설정 키·검증·월드별 값) → SWITCH-A(생존 스위치), SWITCH-B(월드·프로토콜·세션 스위치), LOGSTATUS(콘솔 로그·status), WEATHER(날씨·번개), STATIONS(숫돌·주민 빈 오퍼·사용자 효과·인챈트), 저작권 수정, SHUTTEST, VENDOR → M9 새 코드 검토·반박 → P6 전체·verify. 8.5 정본이 바뀌어 CONFIG를 합칠 때까지 설정 기본 파일 시험은 실패한다(의도).
- PMMPB 합침(재개 뒤 다시 함): 로그인·세션 값 `Player::{locale, input_mode, device_model, device_id, game_version, server_address, port, ping}`(`Identity.client`로 Join 입력에), 핑(RakNet connected pong, NetherNet은 SCTP RTT를 밖에서 얻을 수 없어 ICE consent RTT; 세션이 5초마다 바뀌면 `Measure::Ping` 입력), 플레이 기록 `Player::{first_played, last_played, has_played_before}`(PMMP `firstPlayed`/`lastPlayed` 밀리초, 조인 벽시계는 세션이 Join에 담고 저장 시각은 조인 시각 + 틱 × 50ms; 고친 결함: 아무것도 안 바꾼 플레이어가 떠날 때 저장되지 않던 것 → PMMP처럼 늘 저장), 네트워크 제어 `Context::{set_motd, block_address, unblock_address, connection_count, valid_connection_count}`(틱당 256, accept 작업이 차단 4,096개를 지님, 같은 주소 다시 차단은 끝 시각을 바꿈(PMMP와 같음), 연결 수는 1초마다 바뀌면 `Measure::Connections`; `Context::motd`는 지금 값), TPS·MSPT `Context::{tps, average_tps, mspt, average_mspt}`(틱 루프가 잰 시간을 다음 틱 `Measure::TickTime`으로, PMMP 식), 입력 로그 형식 9(4~8 읽음). 표 39 닫음: `Stack::tag`는 타입 NBT를 스택에 없는 키에만 넣음. 에이전트가 시작할 때 `.cache`가 비어 PMMP 동작을 다른 복사본으로 봤으므로, 쓴 사실(TPS·사용률 식, `firstPlayed`·`lastPlayed` 키, `blockAddress` 덮어쓰기)을 고정 커밋 6a7cc02에서 리드가 다시 확인함(같음). 실제 접속 확인 대기: 공식 클라이언트 NetherNet에서 ICE consent RTT가 들어오는지.
- BACKUP 합침(재개 뒤 다시 함, 표 5 백업 부분·6): `Context::backup`·기본 명령 `backup`(op, `anchor.command.backup`; PMMP에 없어 이름은 Anchor). 다음 스냅샷에 백업 표시 → 커밋 뒤 저장 스레드가 읽기 트랜잭션 하나로 모든 표를 8 MiB 단계(단계마다 커밋)로 `backups/world-<초>.redb.tmp`에 복사, 밀린 요청은 단계 사이에 먼저 처리, 닫기·동기화·rename, 시작 백업과 합쳐 3개. 하나씩만(겹치면 거부·warn), 실패나 파일 다시 열기는 `.tmp` 삭제·error, 정상 종료는 백업을 끝낸 뒤 닫음, 틱 사망은 버림, 결과는 로그(틱 입력 아님). 설계와 다르게 백업 `compact`는 하지 않음(한 번 3.7~4.1초 막힘, 크기는 10~13% 큼, 되돌린 백업은 그 시작이 compact). 시작 비용 실측(다른 빌드 부하 중): 190 MiB 0.9~5초, 754 MiB 9.6~25초, 3,060 MiB 65~99초(복사 41~73초, `check_integrity` 4.7~5.5초, 다시 compact 2.8~3.5초) → 표 6 닫음: 시간의 약 65%가 디스크 속도에 묶인 복사, 1 MiB 버퍼는 이득이 일관되지 않고 compact 생략은 약 4%라 넣지 않음, 무결성 검사는 생략하지 않음. 남은 위험: 단계마다 커밋·동기화라 디스크가 바쁘면 한 단계가 길어짐(부하 중 최대 37초) → 최종 검토에서 다룸.
- CONFIG 합침: 8.5 정본 키 63개, `config::{Lighting, LogLevel, WorldSettings}`, `[worlds]`는 build에서 이름 검사(`config::Error::UnknownWorld`), 월드별 값은 `Config::view_limits` 한 곳에서 계산해 `Settings::view(world)`로 읽음(반경 요청·틱당 청크·스폰 반경·틱 반경·StartGame), 다른 월드로 갈 때 새 월드 시야가 작으면 반경을 줄이고 알림. `backups`·`log-level`은 아직 `Config`에만. 아직 읽히지 않는 설정 필드에는 필드마다 `expect(dead_code)`(스위치 작업이 읽으면 지움). 공개 API: `Developer::lighting`이 bool → `Lighting`, 새 공개 필드(General 15, Developer 7, `Config::worlds`). 남은 일(SWITCH-B로): 작은 월드에서 큰 월드로 돌아올 때 반경이 다시 커지지 않음, `world::Options`·`ServerBuilder::world`의 월드별 값, `fixed-light-level`과 "on-load"의 실제 동작.
- PMMPC 합침(재개 뒤 다시 함): 리스폰 앵커 폭발(어느 월드, 반경 5·불, `PlayerRespawnAnchorUse`·`BlockPreExplode`·`BlockExplode`), 서리 보행(`EntityFrostWalker`, 반경 상한 16: PMMP는 상한 없음, 정지 물도 얼림), 책 편집(`PlayerEditBook`, 50쪽은 할당 전에 검사: PMMP는 패킷이 말한 쪽까지 먼저 늘림), 엔티티 집기(`PlayerEntityPick`, 블록 집기와 "손에 넣기" 함수 하나), 소리·입자 한 길 `tick/emit.rs`(`WorldSound`·`WorldParticle`은 들을 때만, 직접 호출은 이벤트 없음; 약 60곳이 타입 있는 소리로 바뀌어 패킷 값이 PMMP를 따름 → P6로 다시 대조), AI 없는 좀비·아기(`IsBaby`)·좀비·주민 스폰 알(NPC 죽음 드롭과 EntityDeath는 다음 틱: API kill도 드롭), `Context::grow_tree`·`world::TreeType`. 고친 결함: 발 위치를 PMMP처럼 소수 4자리로 반올림(f32 눈 위치 때문에 한 칸 아래가 됨), 블록 폭발에도 폭발 보호, 엔티티 없는 폭발은 블록 폭발 피해(용 알 입자 부호 비트를 바꾼 것은 결함이었음: 3차 검토 V8-D1에서 PMMP 배치로 되돌림). 실제 접속 확인 대기: 아기 주민·좀비 그림, 새 소리 패킷.
- PERF 합침(표 8·9): 원인 ① 청크를 넣을 때마다 `Registry::default()`가 바닐라 크리에이티브 목록(1,633개)을 새로 만들던 틱 결함(M9 크리에이티브 작업에서 생김; release 1명 p99 11~13 ms → 0.29 ms) → 월드에 커스텀 레지스트리가 없을 때만 만듦, ② 시험 해시가 빈 칸마다 NBT를 인코드 → 빈 스택은 단어 하나, ③ 시드를 차례로 돌림 → 자기 풀에서 나란히(같은 시드·틱·단언), ④ 강제 종료 시험은 20 TPS 실시간 → 50번을 같은 시점·조준의 두 사슬로 나란히. 디버그 결정성 120 → 17초, busy 61 → 16초, 아이템 강제 종료 86~94 → 41~59초, lib 전체 202 → 93~135초. 팔레트가 항목마다 조명 값 3바이트를 함께 둠(`paletted::Derive`, 전역 상태·공개 API 변경 없음): 블록 읽기 평지 2.6 → 1.1 ns, 빽빽함 6.8 → 1.3 ns, 조명 켬 평균은 M7과 같음. 1명 p99는 M7보다 약 15% 높음(0.25 대 0.29 ms)이나 조명 일 양(64,892)·청크 수가 같고 16바이트 표현을 지운 경로에서도 남아 원인은 표현이 아님 → 표 9: 표현 문제는 닫음, 남은 15%의 원인은 최종 검토에서 다시 잼(검증 필요).
- LOGSTATUS 합침(표 41 3·4번): `console::log`(직접 쓴 `tracing` 구독자, stderr, UTC 시각·수준·문장·필드, 제어 문자 이스케이프·§코드 제거로 로그 위조 방지, 전역 구독자 없이 `run`·`import_world`·`replay`를 부른 스레드와 Anchor가 만드는 모든 스레드(틱, 저장, tokio 작업자·블로킹, CPU 풀, 콘솔)에만, `log-level = "off"`면 설치 안 함, `ServerBuilder::without_console_log()`로 끄면 사건이 프로그램의 구독자로 감; 찾아 고친 결함: `message`라는 필드가 본문에 붙던 것), 기본 로그 줄(시작 버전·프로토콜, 여는 주소마다 한 줄, 월드 로드·언로드와 종류; 세션 층의 접속 줄은 debug로), `/status` Anchor 고유 문장(벽시계 가동 시간·틱, TPS·틱 시간 지금·평균, 상주 메모리, 스레드, 접속자, 월드별 청크·엔티티·블록 엔티티; 메모리·가동 시간은 틱 밖에서 1초마다 `Measure::Process` 입력, 형식 9 안의 새 태그). 새 의존성 memory-stats(9.1).
- SWITCH-A 합침(표 42 생존 스위치): `hunger`(점검 중 결함 고침: 꺼져 있어도 금사과·케이크·포화 효과가 포화를 채우던 것), `experience`(구슬은 `drop_orbs` 한 곳에서 막음, 저장된 구슬은 청크를 읽을 때 버리고 그 청크는 구슬 없이 다시 저장(FIX-C), 화로·블록·좀비·사망 경험치 0, 인챈트대·모루는 레벨 0, `/xp` 실패), `experience-bottles`, `fall-damage`(엔더 진주 낙하 포함 면제), `drowning`(공기·질식 틱 없음, Breathing 늘 켬, 저장된 Air 안 읽음), `armor`(방어·보호 인챈트·투구·소모·가시 없음), `beds`(침대는 일반 블록, `sleep_on` false), `daylight-cycle`(시간·보정 없음, 클라이언트 시계 paused), `sprint-critical-hits`, `movement-check`(15블록 검사만). 게임 규칙 doDayLightCycle·drowningDamage·fallDamage는 설정값. 꺼진 시스템의 속성은 스폰·리스폰 UpdateAttributes에서 뺌. 공개 API: `player::Error::Disabled`(꺼진 시스템을 바꾸는 API는 이 오류나 무동작; hunger API를 막는 것은 PMMP와 다름). 검증 필요(P6): doDayLightCycle false일 때 BDS의 StartGame 시간 고정 값과 시계, fallDamage·drowningDamage false 규칙. 실제 접속 확인 대기: 속성을 빼면 클라이언트가 기본값을 보이는지, 처음부터 Breathing, 멈춘 하늘.
- SWITCH-B 합침(표 42 월드·프로토콜·세션 스위치): `break-time-check`(끄면 진행도 안 셈, 도달 검사는 그대로), `item-despawn-ticks`(38,767 넘는 값은 Anchor 저장 키 `DespawnDelay`), `item-pickup-interval`(`(틱 + 세션) % N`으로 엇갈림), `slow-empty-worlds`(`State::is_awake`: 시간·블록 갱신·블록 엔티티·조명), `lighting` 세 모드(`world::LightMode`, on-load는 블록 변경에 일 없음, fixed는 지정 하늘빛), `hook-movement-packets`(MovePlayer·MoveActorDelta·MoveActorAbsolute·SetActorMotion)·`hook-player-input`, `enable-encryption`(RakNet은 핸드셰이크 없이 LoginSuccess, NetherNet 그대로, P5로 확인), `vanilla-recipes`·`recipe-book`(레시피 레지스트리가 가짐, 책을 끄면 모든 레시피를 BDS의 잠긴 레시피처럼 문맥 0·재료 없음으로 보내고 CraftRecipeAuto 거부, 사실은 m9-facts SWITCH-B), 원하는 반경 `asked_radius`(월드를 옮길 때 min(원한 값, 월드 한도)), 월드별 값 API(`world::Options::with_settings`, `ServerBuilder::world_settings`). `backups = false`는 사용자 답(2026-10-05, "임시 복사본 유지")대로 리드가 고침: 시작 때 사본을 만들어 손상·새 형식 거부 때 되돌리고, 열기에 성공하면 바로 지움(`StartBackup::Transient`, 시험 `a_start_without_backups_keeps_no_copy`, 손상·새 형식 거부 시험은 두 방식 모두). 백업 명령·`Context::backup`(이제 `bool`)은 거부. 합칠 때 `// pmmp: StatusCommand` 시험 주석(별칭 없음, 대응표 검사 실패)을 지움. 공개 API: `Context::backup() -> bool`, `world::Error::InvalidSettings`, `world::Options::{settings, with_settings}`, `ServerBuilder::world_settings`. 남은 위험: 체크포인트 배치가 바뀌었지만 입력 로그 형식은 그대로(PMMPB와 같은 방식), 런타임 월드의 `Options` 값은 재시작 뒤 남지 않음(문서). 실제 접속 확인 대기: 레시피 책이 비고 제작 칸 제작은 되는지, 암호화 없는 RakNet 로그인.
- 재시작(2026-10-05): 끊긴 P6V, VENDOR, WEATHER의 worktree·브랜치·패치를 지우고 처음부터 다시 띄움(사용자 지시). 같은 단계에 STATIONS, SHUTTEST를 함께 띄움(빌드 5개). 사용자 결정(2026-10-05): 동시에 빌드하는 서브에이전트 상한을 5개에서 7개로 올림. 사용자가 답할 수 없는 동안 결정은 추천안으로 하고 이유를 여기와 보고에 적으며, 되돌리기 어려운 것(데이터 삭제, 큰 공개 API 변경, 사용자 결정 뒤집기)은 하지 않고 보고에 남김. 상한이 7이 되어 COPYRIGHT(생성기 문서와 문장, P6V·STATIONS와 겹치는 파일은 문장 literal만 고치게 함)와 FLAKY(부하에서 흔들리는 `auth::tests::keys_refresh_on_schedule…`의 원인, 다른 벽시계 의존 시험 목록)를 함께 띄움. 같은 때 바뀌지 않는 합친 영역(V1 가져오기, V2 재생·입력 로그, V3 저장, V4 설정·로그·status·빌더, V5 스위치)의 읽기 전용 최종 검토를 먼저 띄움(빌드하지 않아 상한에 들지 않음). 지금 작업이 바꾸는 영역은 그 작업을 합친 뒤 검토함.
- 2차 최종 검토(V1~V5, 읽기 전용, 반박 거침, 결과 `target/m9work/review2/`): 남은 것 22개. V1 가져오기(`backups=false`인데 import가 백업을 남김(V4-D1과 같음), PMMP `PMMPDataVersion` 키가 업그레이드를 막음, 단계 바이트 상한, 죽은 필드, 주석, 블록 상태 버전 세 곳), V2 재생(재생 첫 구간 체크포인트를 틱마다 만들고 버림, 크래시 로그 크기와 재생 상한 불일치, 읽기 미리 할당, 형식 4~7 죽은 분기, 같은 틱 다른 커밋 구분 못 함), V3 저장(리스너가 만든 월드의 스폰 청크가 손상으로 격리됨, 임시 사본이 강제 종료 뒤 남음, 문서), V4 설정·로그(틱 스레드의 동기 stderr 쓰기, `/status` 접속자 수, `/time start|stop` 누락, 문서), V5 스위치(꺼진 경험치의 저장된 구슬, 빈 월드가 같은 틱에 깸, 잠든 월드 블록 엔티티 훑기, 느린 월드의 주기 블록 엔티티가 안 돎). 검토가 확인 못 한 'PendingTicks 좌표'는 반박 단계에서 닫음(절대 좌표). 결정(추천안, 사용자 부재): V2-D2는 커밋 식별값(기록된 입력의 이어진 해시)을 meta와 구간 머리에 넣어 저장 형식과 입력 로그 형식을 한 번씩 올림(이전 저장 형식은 계속 읽음, 되돌리기 어려운 결정이 아님). V4-K1은 로그 스레드 하나와 상한 큐(가득 차면 버리고 버린 수를 적음; 막는 쪽은 멈춤을 되살리므로), 새 크레이트 없음, `.clippy.toml`에 stderr·stdout 금지. 수정 묶음: FIX-A(저장·백업·가져오기), FIX-C(월드 생성·스위치)는 빌드 자리가 나면 바로, FIX-B(재생·로그 형식)는 WEATHER를 합친 뒤(저장 형식이 겹침), FIX-D(로그 스레드·status·time)는 SHUTTEST·COPYRIGHT를 합친 뒤(같은 파일).
- SHUTTEST 합침(표 40 닫음): 종료 단계를 실측함(부하 중 최대: 파일 생성 3.9초, 시작 compact 1.6초, 종료 compact 4.0초, 마지막 커밋 0.35초, 나머지 종료 단계 0.25초 이하, 부하 없을 때 전체 0.43초). 원인은 시험의 벽시계 구간에 서버 시작이 들어 있던 것과 fsync에 묶인 커밋·compact이고, 쓸데없는 타이머 대기는 없음(틱 마지막 sleep은 1틱 한도, 연결이 없으면 linger·grace 안 기다림). 제품 코드는 그대로, 시험은 틱이 돈 뒤 `run`이 Ok이고 `anchor-*` 스레드의 WARN/ERROR가 0건인지로 판정(타이머 만료는 warn이라 잡힘, 결함을 넣어 확인). 부하 중 5회 연속 통과. 남은 일: 새로 만든 빈 파일의 시작 compact 생략은 FIX-A에 넣음. 종료 compact는 PLAN 6절 설계대로 둠.
- VENDOR 합침(표 22 닫음): dimpl 0.7.5, str0m 0.24.1로 올림(7절 분석·권고 없음·라이선스 허용). 상류가 고친 사본 고침(str0m ①②, dimpl ④)은 빼고 남은 것(str0m ③④⑤, dimpl ①②③)만 사본에 둠. 중간 사본 셋은 같은 판이라 그대로. 사본 시험(저장소 밖 같은 명령): str0m lib 757, dimpl 560, sctp-proto 261, redb 통과, vendor-test str0m 1,086. Cargo.toml str0m `=0.24.1`, xtask dimpl `=0.7.5`. NetherNet 피어 시험에 `Fault::ReverseFirst`와 회복 시험 추가.
- FLAKY 합침(표 7의 auth 부분): 원인은 키 갱신 시험이 캐시 원자 쓰기(`sync_all`, 디스크가 바쁘면 3.9초 넘음)를 벽시계 5초 timeout으로 기다린 것(HTTP는 4~50ms). 시험을 시간이 멈춘 런타임(`start_paused`, blocking 작업 중에는 시계가 안 감)으로 바꾸고 요청 목록을 정확히 비교. 고치기 전 디스크·CPU 부하에서 20번 중 9번 `Elapsed`, 고친 뒤 20번 모두 통과, 결함을 넣으면 바로 실패. 제품 코드는 그대로. 표 7의 '시그널링 연결 거부'는 재현 안 됨(NetherNet이 RakNet보다 먼저 열림). 부하에 기대는 다른 시험 5개(spawn.rs 2초 창, tick.rs `sleep(TICK*3)`, job.rs 50ms, session.rs 3초, nethernet 1초 유예)는 TESTS 작업으로 넘김.
- COPYRIGHT 합침(표 43): 생성기가 Go 주석을 읽지 않고 자기 문서를 씀(IR `Doc` 지움, 문서 2,211개 교체; 패킷은 이름·ID·방향, 필드는 이름·와이어 형식·enum 링크·Mojang 상한). 베낀 주석 고침, `liquid.rs` 쇠퇴 계산을 `settle()`로 다시 짬(장면 시험 통과). oracle canonWriter와 스킨 JSON 사본 지움(10.1). 명령 문장: 기본 명령 설명은 BDS처럼 `commands.*.description` 키(BDS P6 2026-10-05: 82개 중 79개가 키), 키가 없는 것은 Anchor 고유 문장, 오류·연결 문장은 클라이언트 키나 Anchor 문장, 디코드 오류 이름표는 Anchor 타입 이름. 결함 2개 고침: `commands.whitelist.*` → `commands.allowlist.*`, `commands.time.query` → `/time query <daytime|gametime|day>`. vendor/is, str0m-aws-lc-rs, str0m-proto에 str0m 저장소의 LICENSE-MIT.txt(저장소에 Apache 파일 없음). 공개 API(저작권 규칙 때문): 콘솔 이름 "Console", PlayerJoin 기본 메시지·PlayerDuplicateLogin 기본 이유가 키, `command::Error::line`이 키, `/time query`는 인자 필수(BDS 모양), `/kick`은 이유 그대로. 실제 접속 확인 대기: Disconnect 안 키 번역(notAllowed, loggedinOtherLocation, disconnect.kicked), CommandOutput의 설명 키 한 줄, `%death.attack.player` 뒤에 붙인 문장(pvp 예제).
- WEATHER 합침(표 41 5번): `src/tick/weather.rs`(BDS가 자리 잡은 날씨에서 보내는 변화표, 접속 때 StartGame 비·번개 수준과 SetCommandsEnabled 바로 뒤 LevelEvent 3001/3002, 월드 이동 때 두 월드 날씨의 차이), API `world::Weather`(맑음·비·천둥, 기본 맑음, 자연 순환 없음), `World::weather`, `Context::set_weather`, `Context::strike_lightning(world, pos, world::Lightning { damage, fire })`. 번개는 엔티티가 아니라 AddActor `minecraft:lightning_bolt`·천둥·폭발 소리와 20틱 뒤 RemoveActor 큐(상한 1024), `damage`는 주변(옆·아래 3, 위 9) 플레이어·생물에 5 피해(`Custom` 원인, 이벤트 없음)와 8초 불, `fire`는 맞은 블록과 주변 4곳에 불(dragonfly `lightning.go` 참고, 퍼짐 위치를 누적하지 않는 결함은 고쳐 매번 맞은 블록에서 고름). 날씨·번개 패킷은 송신 훅을 거치고 start_game 훅이 수준을 바꿔도 되돌림. 저장 형식 6(월드 기록 끝에 날씨 바이트, 5 이하는 맑음). 체크포인트·해시에 날씨와 번개 큐. P6 `api weather/joinrain/joinstorm` SAME(사실은 m9-facts WEATHER). 공개 API: `world::{Weather, Lightning}`, `World::weather`, `Context::{set_weather, strike_lightning}`. 결정(추천안): 비가 불붙은 엔티티를 끄는 효과는 넣지 않음(PMMP·dragonfly 모두 없음, 1절 경계), 자연 번개 없음(사용자 결정), 번개 수명 20틱(BDS 20~40). 남은 차이: 번개 피해의 ActorEvent hurt 데이터(BDS 24, Anchor 0)는 P6V의 피해 원인 번호 작업에서 맞춤. 검증 필요(P6): normal이 아닌 난이도에서 BDS 번개 불·피해. 실제 접속 확인 대기: 비·눈·천둥 하늘, 줄인 메타데이터의 번개 모습·소리.
- FIX-C 합침(2차 검토 V3-D1, V5-D1·K1·K2·D2): 리스너가 만든 월드는 `view::update`가 청크 묶음 직전에 저장 스레드에 알림(`announce` 리스너가 만든 월드는 다음 시야 단계나 스냅샷 때), 경험치가 꺼지면 저장된 구슬은 청크를 읽을 때 버림, `slow-empty-worlds`의 월드 w는 (틱+w)%100==0에 깸(엇갈림), 잠든 월드의 블록 엔티티는 한 번에 건너뜀, 주기형 블록 엔티티(모닥불·신호기)는 느려진 월드에서 깰 때마다 돎(결정: 깬 횟수를 세면 체크포인트·해시에 새 값이 필요해 이 규칙을 고름; 모닥불은 느려진 월드에서 화로보다 상대적으로 빨리 익음). 공개 API 없음. 합칠 때 WEATHER의 번개 큐 필드와 겹친 `Entities` 줄을 손으로 합침.
- 두 번째 강제 종료 뒤 재개(2026-10-05, CLI 백그라운드 세션): VS Code가 부하로 꺼져 세션과 서브에이전트가 끊김. 점검: FIX-A 패치는 작업 트리에 전혀 적용되지 않았음(정방향 `git apply --check` 통과), `.rej`·부분 적용 없음, SWITCH-B는 위에 합침으로 적혀 있고 다시 적용할 것 없음. 남은 worktree 7개와 `worktree-agent-*` 브랜치를 지움. PC 안정(사용자 결정): 이 세션의 프로세스를 Windows BelowNormal로 낮춰 모든 빌드·시험이 물려받게 함, 서브에이전트는 `CARGO_BUILD_JOBS=6`, 부하 재현은 횟수 상한·시험 하나씩(같은 실행 파일 여러 복사본 동시 실행 금지), 멈춤 파일로만 끝나는 무한 반복 금지, 동시 빌드 7개 이하(`target/m9work/impl.md`). 다시 띄움: P6V, STATIONS, FIX-A, FIX-B(WEATHER 합침 뒤라 바로), 읽기 전용 3차 검토 V6 월드 수명주기, V7 세션 값·네트워크 제어, V8 PMMPC·PMMP-D 기능, V9 날씨·PERF·생성기(`target/m9work/review3/`). 다음: FIX-D, TESTS, 3차 검토 반박과 수정, 합친 작업의 최종 검토, P6 전체·verify.
- FIX-A 합침(2차 검토 V1-D1·D2·K1~K4, V3-K1·K2, V4-D1, SHUTTEST 후속): `import_world`는 `run`과 같은 `backups` 설정으로 연다(`StartBackup::from_backups` 한 곳, 결정: storage.rs에 두어 server.rs 변경을 호출 두 곳으로), `backups = false`의 시작 사본은 `.tmp` 이름으로 두고 성공하면 지우며 죽은 시작이 남긴 것은 다음 시작이 지움, 시작 compact는 있던 파일(시작 사본을 만든 파일)만, 가져오기 한 단계는 256청크나 256 MiB 중 먼저, PMMP `PMMPDataVersion` 키는 PMMP처럼 버림, 블록 상태 버전은 생성 상수 `BLOCK_STATE_VERSION` 하나(gen이 blocks.json과 마지막 스키마 버전이 같은지 검사), level.dat 죽은 필드 삭제, 문서. PendingTicks 좌표는 절대 좌표로 닫음(픽스처 청크 (0,-1)의 (3,-58,-1), 시험 `an_imported_pending_tick_runs_at_its_world_position`). 공개 API 없음. 합친 뒤 clippy, lib import·storage·save 210개, tests import·config·server·replay·crash, xtask upgrade 통과. 남은 위험: world 파일이 사라진 채 다시 시작하면 남은 `.tmp`를 지우지 않음(`backups = true`의 `.tmp`와 같은 원래 한계).
- FIX-B 합침(2차 검토 V2-K1·K2·K3·D1·D2): 입력 로그 형식 10(구간 머리에 커밋 식별자), 저장 형식 7(meta 값에 (틱, 커밋 식별자), 6 이하는 식별자 모름으로 읽음). 커밋 식별자 = 틱이 적용한 모든 입력의 자리·종류·패킷 id·측정값을 이은 FNV-1a 64(결정: 패킷 본문은 넣지 않음 — 틱에서 모든 패킷을 인코드하는 비용을 로그 설계가 피하고, 두 실행은 매 틱 기록되는 틱 시간으로 갈림; 해시는 기존 `tick::Fnv`를 씀). 재생은 틱과 식별자가 모두 같은 파일만 고르고, 어느 쪽이든 모르면 틱만 봄. 로그는 형식 8 이상만 읽음(4~7 코드 삭제, 변형 시험은 형식 10에서 CRC를 다시 계산). 크래시 로그는 `MAX_FILE_BYTES`(1 GiB, 읽는 쪽과 같은 상수) 이하로 씀: 넘으면 가장 새 커밋 구간부터, 그래도 넘으면 쓰지 않고 error. 재생 로그는 첫 구간에 복원한 체크포인트를 가짐(틱마다 다시 뜨지 않음). 두 리더의 미리 할당은 도우미 `capacity` 하나가 남은 바이트로 묶음. 공개 API 없음. 합친 뒤 clippy, lib 286개, tests replay·crash·import·server·shutdown·world·log·config 통과. 남은 위험: 모든 틱의 패킷 id·세션·순번·µs 틱 시간까지 같은 두 실행은 식별자가 같을 수 있음(닿지 않는다고 봄, 모듈 문서), 가장 새 구간의 체크포인트 하나가 1 GiB를 넘으면 크래시 로그 없음(error).
- 3차 최종 검토(V6~V9, 읽기 전용, 반박 거침, `target/m9work/review3/`): 남은 것 V6 7개(공개 API 인자 `i32::MIN`으로 틱 패닉, 콘솔 명령이 기본 월드 대신 설정 월드, 하드코어인데 다른 난이도 월드, 언로드 요청 뒤 load_world가 Ok인데 언로드됨, 티켓 선형 합, 문서; Joined 단계 플레이어가 시간 정지·난이도 변화를 못 받음), V7 3개(제어 큐 flush 시점이 재생을 가를 수 있음, 틱 길이 네 곳, 문서), V8 5개(용 알 입자 부호 비트가 PMMP와 다름, 죽는 중 저장된 NPC가 살아나 드롭 반복, 서리 보행 선형 검색, 구리 단계 세 곳, 이중 검사), V9 2개(Joined 플레이어가 날씨 변화를 못 받음, 생성 문서 오류). 수정: FIX-D(V6-D2 추가), FIX-E(월드·날씨 보내기), FIX-F(게임·네트워크·생성기).
- 4차 검토(이번 세션에 합친 FIX-A·B·D·E·F의 결과, 읽기 전용, 반박 거침, `target/m9work/review4/`): 남은 것 8개. R4a: `backups = false`에서 시작이 실패해 남긴 사본을 다음 시작이 `.tmp`로 보고 지움(원본 손실), FNV-1a 64가 두 곳(`random::tag`, `tick::Fnv`), 백업 모듈 문서. R4b: 두 번째 종료 신호의 error 줄이 로그 스레드에 닿기 전에 프로세스가 끝남(FIX-D 퇴행), `set_random_tick_speed`가 Joined를 건너뜀(FIX-E와 같은 원인), 로그 줄 길이 상한 없음, 태그 없는 lint 이유 5곳(verify가 태그를 검사하지 않음). 수정: FIX-H(verify에 태그 검사 추가 포함). 합치면서 모은 작은 후속(월드 이동 난이도, `props::renamed`·EPSILON 중복, `Error::Start` 문장)은 FIX-G.
- FIX-H 합침(4차 R4a-K1·K2·K3, R4b-D1·D2·K1·K2): `backups = false`에서 실패한 시작이 남기는 사본은 `backup::keep`으로 백업 이름을 받아 다음 시작이 지우지 않음(운영자가 지울 때까지), FNV-1a 64는 `random::Fnv` 하나(`tag`·상태 해시·커밋 식별자, 출력 같음), 두 번째 종료 신호는 error 줄 뒤 콘솔 로그 큐가 비기를 최대 500 ms(`EXIT_LOG_WAIT`) 기다리고 130으로 종료(`log::Drained`), `set_random_tick_speed`도 Joined에게, 콘솔 로그 한 줄의 메시지·필드는 4 KiB까지(문자 경계, `…(cut)`, 큐 상한 약 16 MiB), 태그 없는 lint 이유 5곳 고침과 verify의 'lint reason tags' 검사(`xtask/src/reasons.rs`, 저장소 410개 오류 0). 공개 API 없음(`Server::run` 문서 한 문장).
- 5차 검토(STATIONS, FIX-G·H, TESTS의 결과, 읽기 전용, 반박 거침, `target/m9work/review5/`): 결함 0, 남은 것 8개(실패한 시작마다 사본이 쌓임 — 손상 아닌 실패에서만 정리, verify 태그 검사가 `examples`·`xtask/tests`·raw 문자열을 못 봄, 사건 대기에 고장 대비 마감 없음, job 시험이 결과 버림 경로를 확인 못 함, spawn 시험의 2초 판정 창, root Linux에서 깨지는 저장 시험, 봇 시나리오 목록 문서, 숫돌 dragonfly 차이 기록). 수정: FIX-I(저장 실패 분기 함수화·정리), FIX-J(시험·verify), 봇·m9-facts 문서는 P6V를 합친 뒤, 9.1 기록은 리드가 고침.
- 마지막 체크리스트 감사(저장소 전체, 읽기 전용, 반박 거침, `target/m9work/review5/AUDIT.md`·`audit-refute.md`): 결함 0, 남은 것 10개 — 작은 파일 상한 읽기가 일곱 곳(replay 읽기는 metadata 뒤 상한 없는 읽기), 들인 의존성의 블로킹·상한 없는 할당·난수 진입점 금지 빠짐(tokio oneshot·broadcast·Mutex·RwLock·io-util, ureq, aws-lc-rs), 가져오기 보고의 모르는 이름 맵 상한 없음, 거짓 문서 셋(Player::ip, PLAN 4.x 참조, Lighting::Fixed 결과), 파일 전체 `#![expect]`, 시험 파일 `//!` 빠짐, 틱 단위 이름 접미사 섞임(공개 이름 약 20개), 저장 주기의 틱 길이 중복. PLAN 1·6절의 옛 설정 문장은 리드가 고침. 수정: FIX-K(읽기 도우미·금지 목록·가져오기 상한·문서·expect·저장 주기), FIX-L(이름 접미사, xtask 시험 포트 경합의 재시작 가림, session 시험 마감). 결정(추천안, 사용자 부재): 공개 이름 바꾸기는 CLAUDE.md 5.1과 R7-K1 선례를 따르고, 이름만 바뀌며 alias가 남아 되돌리기 쉬운 변경으로 봄.
- P6V 합침(P6V-MERGE로 새 기준 위에 다시 올림; 표 11~21·34·35 후속·38 닫음): 벽걸이 간판 상자는 생성기가 PMMP의 뒤집힌 상자를 BDS 막대로 바꿈, 입자 번호는 BDS가 쓰는 PMMP 번호(14·15, 물약 구름 35; `UnWaxOxidised`는 BDS가 옛 블록 ID를 보내고 Anchor는 보내지 않음), 인챈트 선택지 `Slot`(레깅스 8, 부츠 4, 책 −1), 다른 플레이어 움직임은 BDS도 MovePlayer, 등급 상승 때 BDS는 UpdateTrade를 안 보냄 → Anchor도 안 보냄, 신호기·모루·대장장이 요청 모양은 둘 다 받음, 메타데이터 키 138 int32·127/133 타입 바로잡음·물약 구름 데이터 BDS와 같음, 다른 플레이어 AddPlayer 79키·현재 플래그·기기 ID·플랫폼, 명령 문법 오류는 `commands.generic.syntax`(파서가 틀린 낱말을 찾음, `command::Error::Syntax`)·`/effect clear`는 ID 순서, 폭죽 데이터·폭발 이벤트, 주민 직업 번호 같음, 위치 표시줄 PlayerLocation·LocatorBar(죽으면 숨김·리스폰 때 보임), 피해 ActorEvent에 BDS 피해 원인 번호(익사 9, 공격 2, 발사체 3, 번개 24), 문자열 상한 단위는 BDS도 바이트(채팅 512바이트), 물약 색 12개 BDS 값, `daylight-cycle = false`는 BDS `doDayLightCycle` false처럼(StartGame 잠긴 시간, 멈추지 않은 시계, 주기 시계 갱신). P6 영역: 봇 시나리오 목록을 시나리오 표에서 만듦, m9-facts 숫돌 차이, 봇 `-signaling-addr` 문서, P6 도구의 Anchor는 포트 0과 `listening` 로그 줄로 실제 포트를 읽음(BDS는 server.properties 고정 포트 — 포트를 못 잡으면 시작하지 않아 `wait_started`가 실패, 다른 서버에 붙지 않음; 검증 필요: 재현 안 함). 의도한 차이 11개를 intended.txt에(PMMP에 없는 삼지창 인챈트·느린 낙하, `/xp`·`/gamemode` PMMP 모양 등). 결정: 긴 명령은 BDS가 508바이트부터 연결을 끊지만 Anchor는 PMMP 상한(512글자·2,048바이트)과 '너무 김' 답을 둠, 폭죽 `Flight`는 표시 전용이라 체크포인트·해시에서 뺌. 공개 API: `command::Error::Syntax`(enum이 non_exhaustive가 아니라 깨지는 변경), 입자 번호·효과 색·채팅 상한. `p6 bds`·`p6 anchor`·`p6 import` PASS(P6V-MERGE). 남은 차이(범위 밖): 엔티티 공통 데이터 키 약 70개 대 PMMP 짧은 묶음, 엔티티 없는 소리의 엔티티 타입 "" 대 ":", 삼지창 던지기 소리, 구리 긁기 LevelEvent 대 소리, 투척 물약 지속 시간 식. 실제 접속 확인 대기: `EntityDataFlagCount` 131, 위치 표시줄 그림, 새 물약·효과 색.
- 사용자 결정(2026-10-05, 이 세션에 직접): 새 검토 라운드는 더 돌리지 않음. P6V-MERGE 합침, 문서 링크 경고 정리, 마지막 verify·P6 전체 뒤 M9 완료. 고치지 않은 지적은 고치지 않고 보고서 '남은 지적'에 적음(데이터 손실·복제 결함만 예외).
- FIX-N 합침: 비공개 문서의 끊긴 intra-doc 링크 22개와 중복 링크 6개 수정, `cargo xtask verify`에 `cargo doc --document-private-items` 검사 추가(따뜻한 상태 약 13~25초). 문서만.
- 마지막 검사(2026-10-05): `cargo xtask verify` PASS(gen·table·lint 태그·fmt·clippy·시험 전체·vendor 사본 시험 넷·doc 둘; 첫 실행은 FIX-N 뒤 table.md 줄 번호가 낡아 `table --check`만 실패 → 다시 만들고 PASS). P6: `p6 bds`·`p6 game bds`·`p6 game anchor`·`p6 api bds`·`p6 api anchor`·`p6 import` PASS. `p6 anchor`는 FAIL: 첫 실행은 `p6 api bds`보다 먼저 돌아 옛 BDS 기록과 비교(tradeup, 다시 돌리면 SAME), 둘째·셋째는 시나리오 격리 문제 — 앞 시나리오의 화로가 꺼지는 블록 갱신이 grindstone 단계에 섞이고(두 번 다른 단계), 앞 단계에서 놓은 구리 창살이 랜덤 틱으로 산화해 blocks 비교에 섞임. 같은 비교인 `p6 game anchor`는 통과, 차이가 실행마다 다른 단계에 나와 제품 동작 차이가 아니라 시험 도구의 타이밍 문제로 판단. 데이터 손실·복제 결함이 아니므로 사용자 결정대로 고치지 않고 남은 지적에 둠.
- M9 완료(사용자 결정에 따라): 남은 지적·검증 필요·실제 접속 확인 대기 목록은 `.cache/m9-report.md`.
- FIX-M 합침(6차 R6a-D1·K1·K2, R6b-K1·K2·D1): redb의 `Io(InvalidData)` 열기 오류(매직 불일치, 읽기 전용으로 연 빈 파일)는 손상(`DamagedFile`)으로 봄 → redb가 아닌 world 파일로 시작하면 원본 그대로 거부하고 백업을 정리하지 않음(Make·Transient 시험), 정리 문서·이유, `Lighting::Fixed`의 서리 얼음, 이름 `fuse_ticks`·`set_fuse_ticks`·`task_period_ticks`(alias 그대로), 비공개 문서 링크 둘, spawn 시험의 시작 마감 실패 때 START를 쥔 채 서버를 멈추고 끝나기를 제한 시간만큼 기다린 뒤 실패. 공개 API: 이름 셋, redb가 아닌 파일의 오류 종류가 `WorldDamaged`로.
- P6V 끝(리드 합치기 전): BDS로 닫을 수 있는 검증 필요(표 11~21·34·35 후속·38, 물약 색, SWITCH-A 낮 주기)를 봇 시나리오로 BDS에서 먼저 확인하고 Anchor를 맞춤, `p6 bds`·`p6 anchor` PASS. 오래된 기준에서 작업해 9개 파일이 겹쳐, 새 기준 위로 다시 올리는 P6V-MERGE를 띄움(봇 문서·시나리오 목록, m9-facts 숫돌 차이, P6 도구 `free_ports` 경합, 쓰이지 않는 Firework 필드 포함).
- 6차 검토(FIX-I~L의 결과, 읽기 전용, 반박 거침, `target/m9work/review6/`): 결함 2(첫 바이트가 redb 매직이 아닌 파일은 redb가 `InvalidData` Io 오류로 알려 손상으로 분류되지 않음 → FIX-I의 정리가 재시작 3번에 정상 백업을 모두 지울 수 있음(높음), spawn 시험 시작 마감 실패 때 서버가 남음), 그 밖 5(정리 문서, `Lighting::Fixed` 서리 얼음, 이름 접미사 빠짐 `fuse`·`task_period`, 비공개 문서 링크 둘, 봇 문서). 수정: FIX-M, 봇 문서는 P6V-MERGE.
- FIX-L 합침(감사 K9, 후속): 틱·초 단위 공개 이름에 접미사(`effect::Instance::duration_ticks`·`with_duration_ticks`, `effect::Type::default_duration_ticks`, `item::Type::fuel_time_ticks`, 엔티티 `pickup_delay_ticks`·`despawn_delay_ticks`·`cloud_age_ticks`·`cloud_max_age_ticks`·`cloud_max_age_change_on_use_ticks`·`cloud_reapplication_delay_ticks`와 세터, 블록 엔티티 `cooking_time_ticks`, `Context::item_use_duration_ticks`, `World::time_ticks`, `BreakInfo`·`State::break_time_secs`; alias 그대로; `BrewingFuelUse::fuel_time`(양조 횟수)·`Cooldown::ticks`·설정 필드는 그대로). xtask spawn·examples 시험은 포트 0과 서버 시작 로그 줄(여는 주소마다 한 줄)로 실제 포트를 읽음, 다시 시작하는 반복 삭제(spawn은 콘솔 로그를 끄고 시험 구독자가 `listening` 줄을 모음, 서버 시작은 한 번에 하나), 봇 `-signaling-addr`(없으면 `-addr`), session 시험 `join_status`에 10초 마감. xtask 개발 의존성 `tracing =0.1.44`(이미 들인 것). 공개 API: 위 이름 바꿈(CLAUDE.md 5.1, R7-K1 선례).
- FIX-K 합침(감사 K1~K8·K10): 작은 파일 상한 읽기를 `fs::read_bounded` 하나로(일곱 곳, replay의 metadata 뒤 읽기 구멍 닫음, auth 호출자 검사 삭제), 금지 목록 보강(9.1 tokio·ureq·aws-lc-rs 행, 경로 27개 모두 걸림을 임시 코드로 확인), 가져오기 보고의 모르는 이름은 4,096개·1 MiB까지(넘는 수는 `Report::unlisted_unknown_blocks`), 문서(Player::ip, CLAUDE.md 4.x 참조, `Lighting::Fixed` 결과), 함수 단위 expect, 시험 파일 `//!`, 저장 주기는 `TICK`에서. 공개 API: `import::Report::unlisted_unknown_blocks() -> u64` 추가. 합칠 때 FIX-D 시험의 `// pmmp:` 주석(별칭 아닌 문장, `table --check` 실패)을 보통 주석으로 고침. 남은 것(선택): std `Read::read_to_end` 금지는 다른 사용처(LevelDB·zip)를 본 뒤.
- FIX-J 합침(5차 R5b-K2·K3·K4·K5): verify의 lint 이유 태그 검사가 `examples`·`xtask/tests`와 raw 문자열 이유도 봄, 사건 대기(tick.rs 크래시 로그 시험의 첫 플레이어·틱 스레드 끝, session.rs 연결 수 보고)에 고장 대비 마감(실패는 assert), `Runner::run`이 작업 `JoinHandle`을 돌려주고 job 시험이 그것을 기다려 `Ok(Ok(()))`를 확인(옛 시험은 작업이 패닉해도 통과했음), spawn 시험의 `/v1/join` 확인은 60초 마감까지. 공개 API 없음. 합칠 때 FIX-I의 session.rs 시험 변경 위에 올림.
- FIX-I 합침(후속, 5차 R5b-K1·K6): `tests/shutdown.rs`가 부하에서 9번 중 1번 실패한 원인 = 시험이 UDP 빈 포트만 고르고 놓았는데 서버는 같은 포트에 UDP와 TCP(시그널링)를 둘 다 열어, TCP가 쓰이고 있거나 그 사이에 빼앗기면 `AddrInUse`로 자식이 보고 없이 죽음(탐침으로 재현) → 시험 자식·통합 시험(server·replay·log·import·crash)은 포트 0, session 시험은 포트 0 뒤 실제 주소를 읽음, 부하 아래 20회 통과. `MOTION_THRESHOLD` 한 곳, `renamed` 버퍼 = `block::MAX_PROPERTIES`. `Storage::open`의 실패 처리를 `failed_start`로 빼고 주입한 `Error::Io`로 시험(root Linux에서도 같은 길), 손상 아닌 실패 시작도 백업을 KEPT개로 정리(손상 파일을 되쓰지 못한 갈래는 정리 안 함: 옛 정상 백업 보존). xtask spawn·examples 시험의 `free_port`도 같은 경합이었고 5번까지 다시 시작해 가려졌음 → FIX-L에서 고침.
- STATIONS 합침(표 41 2·6번; 7번은 사용자 결정으로 M9 밖): 숫돌은 PMMP 데이터에 없어 사용자 예외 파일 `xtask/data/exceptions.json`(블록 상태 16·아이템·크리에이티브·모양 레시피 4)을 gen이 PMMP 덤프 뒤에 더함(상태·해시가 기준 목록과 같아야 통과; 결정: 덤프가 아닌 gen 입력 — PMMP에 없는 블록이라 덤프가 낼 것이 없고 `dump --check`가 그대로). 놓기(바닥·천장은 놓는 사람 쪽, 옆은 누른 면 쪽)·받침이 없으면 부서져 하나 떨굼·창 26은 BDS P6 SAME(`p6 game grindstone`). 갈기(저주 아닌 인챈트 제거, 둘이면 남은 내구도 합 + 최대의 5%·저주 합침, RepairCost 지움, 인챈트가 다 지워진 인챈트 책은 책, 경험치 구슬은 눈 위치, `experience = false`면 없음)는 dragonfly를 바탕으로 결함을 고친 규칙(9.1 dragonfly 숫돌 행): BDS가 봇이 만든 모든 갈기 요청을 상태 9(InvalidCraftResult)로 거부해 대조 못 함(m9-facts STATIONS). 주민: 알·API 소환은 이미 빈 목록(바닐라 템플릿 코드 없음), 시험 추가. 찾아 고친 P6 결함: 봇 판정이 이름 목록 밖 게임 시나리오(weather·joinrain·joinstorm·grindstone)를 스크립트 실패에도 통과시킴 → `isGame`으로 판정, 날씨 시나리오 BDS·Anchor 다시 SAME. 공개 API: `block::Kind::Grindstone`, `block::Class::Grindstone`(블록·아이템 번호 밀림, 저장은 이름이라 영향 없음; 모양 레시피 924 뒤 네트워크 ID가 4씩 밀림). 실제 접속 확인 대기: 공식 클라이언트의 숫돌 갈기 요청 모양과 BDS 결과·경험치, 숫돌 그림.
- FIX-G 합침: `switch_world` 난이도 동기화를 Loading이 아닌 모든 플레이어로(스폰 창 뒤 Spawned 전 텔레포트), `props::renamed`·EPSILON 중복 제거, `Error::Start` 문장을 모든 시작 실패 경로에 맞춤(공개 API는 문장만). 합친 뒤 clippy, lib 1,627개 통과. 실제 접속 확인 대기: 그 텔레포트에서 공식 클라이언트가 SetDifficulty를 반영하는지.
- TESTS 합침(표 7 나머지 닫음): 부하에 기대던 시험 5개를 사건으로 기다리게 바꿈 — spawn.rs NetherNet 세션 종료는 `PlayerQuit` 5번, tick.rs 크래시 로그 시험은 첫 join의 출력, job.rs는 작업의 끝(`num_alive_tasks`), session.rs는 첫 연결 수 보고, NetherNet 종료 시험은 1초 유예 대신 드라이버가 끝난 뒤 소켓에 남은 데이터그램을 모두 읽음. tick·job의 옛 시험은 부하에서 경로를 조용히 건너뛰었음(멈춤 probe 3/3 실패). 제품 코드 그대로. 부하 아래 각 20회 통과, `cargo test --workspace` 2회 통과, '시그널링 연결 거부'는 재현 안 됨. 결정: spawn 시험은 quit 뒤 남은 서버 안 메시지 한 번에 드러나는 신호가 없어 status 확인을 남김(FIX-J에서 100ms 간격, 60초 고장 대비 마감으로 바꿈).
- FIX-D 합침(2차 V4-K1·K2·K3·K4, 3차 V6-D2): 콘솔 로그는 로그 스레드 `anchor-log` 하나가 stderr에 씀, 이벤트 스레드는 줄을 만들어 상한 큐(tokio mpsc 4,096줄)에 `try_send`만(가득 차면 버리고 원자 카운터로 셈, 버린 수는 다음 줄 앞과 끝에 Anchor 문장), `Server::console_log`가 만들어 run·import_world·replay가 모두 씀, 가드 drop은 강한 sender 하나를 놓고 남은 줄을 쓴 뒤 join(결정: 반박안의 '가드가 receiver를 닫음'은 async 문맥의 `blocking_send` 패닉에 닿아, 구독자는 약한 sender만 갖게 함; 분리된 콘솔 스레드가 끝을 막지 않음). `.clippy.toml`에 `std::io::stderr`·`stdout` 금지(시험 자식 보고 3곳과 xtask `run`은 예외). `/status`는 스레드 줄에 log, 접속자 수는 `player_count`. `/time start|stop`(권한 time.start/stop, 모든 월드, Anchor 문장). 콘솔의 월드(`/seed`·`/setworldspawn`·`/time query`·`/particle`)는 `set_default_world`를 따름. 공개 API 없음. 실제 접속 확인 대기: `/time start|stop` 오버로드가 클라이언트 명령 목록에 어떻게 보이는지(BDS time에는 없음).
- FIX-E 합침(3차 V6-D1·D3·D4·K1·K2·X1, V9-D1): 월드 단위 전송(시간 정지, 월드 난이도, 날씨, `set_time`, 서버 난이도)은 `world_api::send_to_world` 하나로 StartGame을 받은 모든 플레이어(Joined 포함)에게 감(스폰 창이 같은 상태를 다시 보내도 해 없음), `switch_world` 날씨도 같음. 하드코어 서버는 모든 월드를 Hard로(`difficulty_of` 한 곳, setter 거절은 문서대로 둠), 같은 콜백에서 unload 뒤 `load_world`는 unload를 취소, `hold_chunks` 범위 검사 `unsigned_abs`(i32::MIN 패닉), 청크 티켓은 (청크, 티켓) 색인으로 찾고 셈(결정: 숫자와 맵 둘 대신 집합 하나 — 같은 사실 한 곳; 티켓에서 만들어 체크포인트에 넣지 않고 복원 때 다시 만듦), 문서. clock.rs의 주기 동기화는 Spawned만(합치면 BDS 스폰 창 순서가 깨짐). 공개 API 모양 그대로, 동작 셋이 바뀜(위). 합친 뒤 clippy, lib 271개, tests world·config 통과. 실제 접속 확인 대기: 청크를 기다리는 동안 바꾼 날씨·난이도·시간 정지가 화면에 맞는지.
- FIX-F 합침(3차 V7-K1·K2·K3, V8-D1·D2·K1·K2·K3, V9-K1): 용 알 입자 부호 비트를 PMMP 배치로 되돌림(x 24, y 25, z 26), 죽는 중 저장된 NPC(체력 0)는 죽는 중으로 읽음(PMMP처럼 두 번째 드롭 없음), 서리 보행은 움직임마다 상자 목록 한 번(`entity::boxes_in`), 엔드 수정구 검사도 `anything_in`, 구리 이름 파서·빌더를 block.rs 하나로(`CopperOxidation::prefix`), `spawn_zombie` 이중 검사 삭제, 네트워크 제어 flush는 `boundary` 끝(퇴장 리스너의 제어도 같은 틱, 재생과 같음), 틱 길이는 `tick::TICK` 한 곳, 생성 문서의 틀린 'a list without a count' 삭제, `block_address` 문서. 공개 API 없음. 실제 접속 확인 대기: 용 알 순간이동 꼬리 방향(PMMP·dragonfly 같은 값).
- `.cache` 사고와 복구(2026-10-05): 남은 worktree를 `git worktree remove --force`로 지울 때 그 안의 `.cache` junction을 따라가 메인 `.cache`가 지워짐(리드 실수, 앞선 재개 때 `.cache`가 사라진 것도 같은 원인으로 봄). 고정 커밋·sha256대로 다시 받음: pmmp 6a7cc02(+`composer.lock` vendor), dragonfly 4c7b5074, cloudburst-data 659ce1e, protocol docs 475bd72, 업그레이드 스키마 4e874a16·7f551f62, PM5 PHP(zip sha256 9.1과 같음), Strawberry Perl 5.42.3.1 portable(zip sha256 `6a081a811781c30aca51dbc036afd93092af91e3297901f02c17043795a10690`, GitHub 릴리스 SP_54231_64bit 값과 같음), `m9-resume.md`(리드가 읽은 내용 그대로), BDS는 P6V가 다시 받음. `cargo xtask dump --check` 통과, dimpl 시험 560개 통과. 앞으로 worktree를 지우기 전에 junction을 `cmd //c rmdir`로 먼저 뗌.
- 재개 시점 verify: dimpl 시험만 실패(위 사고로 Perl 없음), 나머지 검사 모두 통과.
- 사용자 결정(2026-10-05, 사용자와 대화 중인 다른 Claude 세션이 전함): 사용자 정의 상태 효과·인챈트 API는 M9 범위에서 뺌(클라이언트가 새 효과·인챈트를 표시할 수 없음). STATIONS는 숫돌·빈 주민 오퍼만 함. 8.4, 표 41, 설계 7번, 대응표 규칙 4개(`EffectIdMap::register`, `EnchantmentIdMap::register`, `*Effect::__construct`, `*Enchantment::__construct`)를 고침.

#### M9 후속 (2026-10-05, 지시: `.cache/m9-followup.md`)
- 공유 약속(리드): 새 설정 키 `experience-orbs`(`experience-bottles`를 대신함, 구슬과 경험치 병), `durability`, `education-items`, `integrity-check`(`config::IntegrityCheck`), 권한 `anchor.command.check`, `/check` 자리 `src/tick/command/builtin/check.rs`. 8.5 정본 키 66개.
- 커밋: 항목마다 합치고 빌드·clippy·영역 시험이 통과하면 그 항목만 커밋(사용자 결정, 다른 세션이 전함). 푸시 안 함.

| 항목 | 담당 | 상태 |
|---|---|---|
| U1 명령 등록, U2 인자 타입, U3 콘솔 문장 | CMD | 완료 |
| U5 활과 아이템 사용, U4 교육판 아이템, R4 삼지창 소리·투척 물약 | ITEMUSE | 완료 |
| U8 움직임 끊김 | MOVE | 완료 |
| U9 무결성 검사·`/check`, R2 백업 단계, 남은 지적 7 | STORE | 완료 |
| R1 P6 격리, R6 번개 난이도, 남은 지적 10·12 | P6 | 완료 |
| R5 1명 p99 | PERF | 완료 |
| R7 숫돌 삭제, U6 경험치, U7 내구도, 남은 지적 4·5·6·9 | CLEAN | 완료 |
- U8 패킷 합침: 다른 플레이어 이동을 MovePlayer에서 `MoveActorAbsolute`(눈 위치, 땅 플래그, 회전, 보간됨)로 바꿈. 순간이동과 본인 보정만 MovePlayer. P6 others 차이는 intended에 등록.
- U8 틱 합침: Windows std `thread::sleep`이 이미 고해상도 타이머임을 std 소스와 실측(200틱 p99 50.5 ms)으로 확인해 대기 방식은 그대로 둠(tokio 42~63 ms, 돌며 기다리기는 틱마다 CPU 2 ms). 측정 시험 `tests/server.rs` `tick_intervals`(ignored).
- U1 합침: 별칭 enum을 BDS 모양 `<name>CommandAliases`(이름 먼저)로, 플레이어마다 쓸 수 있는 이름·별칭의 `CommandName` enum을 보냄, `help`는 명령으로 보내지 않음(클라이언트 자체 명령, BDS·PMMP와 같음). 권한별 목록·op 변경 뒤 다시 보내기는 원래 맞았음. 실제 클라이언트 확인 대기.
- U2 합침: 기본 명령 21개(give, effect, enchant, gamemode, tp, kill, tell, clear, difficulty, kick, op, deop, me, say, list, stop, time, title, xp, spawnpoint, setworldspawn)의 인자 타입·이름·enum을 BDS 기록에 맞춤(고정 기록 대조 시험), Item·Block 인자에 semantic constraint 2, 새 API `Param::player_name`·`Param::suffixed_int`. 결정: 공식 클라이언트에 보이는 것은 BDS와 같게(CLAUDE.md 8절) — PMMP 전용 입력 `gamemode v|view|3`, 문자열 숫자 난이도, `op @a`는 더 받지 않음. `particle`·`whitelist`·`transferserver`·`time start|stop`은 PMMP 모양 유지. 검증 필요: gamemode 숫자 5·6을 BDS가 받는지.
- U3 합침: 콘솔 문장 표 `src/console/text.rs`(Anchor 고유 문장 139개: 명령 결과·설명, 일반 오류, 채팅 형식, 게임 모드·효과 이름, 접속 키; 매개변수 안 `%key`도 바꿈). 콘솔 명령 결과, 플레이어 명령 결과 로그, 콘솔로 보낸 메시지가 모두 이 표를 씀. 플레이어에게는 키 그대로. 소스의 모든 키에 문장이 있는지 보는 시험.
- 남은 지적 7 합침: world 파일이 없거나 비어도 시작이 남은 백업 `.tmp`를 먼저 지움(잠금 확인 뒤).
- R2 합침: 실행 중 백업의 단계 커밋은 동기화 없이(`Durability::None`), 다 복사한 뒤 지속 커밋 한 번 → 닫기·동기화·이름 바꾸기. 끊긴 복사는 `.tmp`로 남아 백업이 아님(원자성 유지). 동기화 7단계 기준 13번 → 6번(redb 파일 성장 때만 남음).
- U9 합침: `integrity-check` 연결(`after-unclean-shutdown`은 정상 종료가 compact 뒤 마지막 커밋으로 `meta`의 `closed` 표시를 쓰고 다음 시작 첫 커밋이 지움; 실패·패닉·강제 종료는 표시 없음). `backups = true`의 시작 백업은 늘, `backups = false`의 임시 사본은 검사하는 시작에서만. `/check`(op, 게임·콘솔): 다음 저장 커밋 뒤 저장 스레드가 그 커밋의 값을 8 MiB 단계로 읽어 봉인 확인, 단계 사이 요청 먼저 처리(틱 안 막음), 하나씩, 결과는 실행한 사람에게(Anchor 문장). redb `check_integrity`는 닫힌 파일이 필요하고 원본을 고쳐 쓰므로 실행 중 안 씀. 입력 로그 형식 11(검사 결과도 틱 입력), `storage::Start { backup, check }`.
- R5 합침: 1명 p99 +15%의 원인은 M9에서 커진 `light::Engine::step` 안의 `level`·`relax`·`relax_at`이 인라인되지 않은 것(일의 양 같음, codegen-units=1이면 M7과 같음). `#[inline]`으로 청크 한 줄 빛 일 0.267 → 0.172 ms(M7 0.186), 1명 p99 0.469 → 0.376 ms(M7 0.380). 표 9 검증 필요 A 닫음.
- 남은 지적 12 합침: verify의 비공개 문서 검사를 `target/verify-private-docs`로 옮김(바뀐 것 없을 때 두 문서 검사 약 25초 → 약 1초).
- R1 합침: P6 trace가 시나리오 영역(teleport 위치)에서 4칸 밖의 블록 변화·블록 엔티티 데이터·LevelEvent와 이 캡처에서 추가된 적 없는 엔티티의 제거를 보여 주기만 하고 비교하지 않음(영역 8칸 간격의 절반). 시험 `what_other_scenarios_left_behind_is_shown_only`.
- R6 합침: BDS 대조 — 번개 피해 5와 불붙음은 모든 난이도, 맞은 블록의 불은 normal·hard에서만(peaceful·easy 없음). `ignite`가 월드 난이도를 봄, `world::Lightning` 문서. P6 시나리오 `boltpeaceful`·`bolteasy`·`bolthard`와 P6 콘솔 `difficulty`. 검증 필요 B 닫음.
- 남은 지적 10 합침: spawn 시험이 서버 debug 로그를 모아(최대 5만 줄) 실패 때 마지막 300줄을 보이고, RakNet은 봇 로컬 주소·NetherNet은 ICE ufrag로 서버 로그를 찾아 봇이 닫은 것과 idle timeout을 구별함(`nethernet peer closed` 로그에 `ufrag` 필드).
- U5 합침: 원인은 실제 클라이언트가 사용 시작 때 ClickAir와 PlayerAuthInput `StartUsingItem`을 보내고 서버가 UsingItem 엔티티 플래그를 켜 주기를 기다리는데(프로토콜 문서 776), Anchor는 먹기·마시기만 켜고 활·삼지창은 안 켠 것. 활·삼지창·염소 뿔·망원경(PMMP `Releasable`)도 플래그를 켬, `Player::is_using_item`도 포함. 음식·물약·투척은 원래 정상, 양동이·부싯돌·뼛가루는 블록 클릭 경로라 무관, 방패 막기는 PMMP에 없음. 봇 시나리오 `itemuse`가 실제 클라이언트 순서로 당기고 놓음(고치기 전 DIFFERENT, 뒤 SAME). 남은 위험: 마시기 끝은 BDS 자기 시계, Anchor는 PMMP처럼 두 번째 ClickAir.
- R4 합침: 삼지창 던지기 소리는 원래 보냈음(옛 봇이 AddActor 뒤부터 세어 놓침), 위치만 BDS처럼 몸 가운데(발 + 0.9). 투척 물약 지속 시간은 BDS 실측(3,600틱 물약 1.5·2.5·3.5블록 2,565·1,755·945틱, 몸 높이 안에서는 높이 무관)에 맞춰 `min(1, 1.05 − 0.225 × 몸 가운데 세로선까지 거리)`. 검증 필요: 머리 위 높이 깨짐에서 몇 틱 차(2,334 대 2,339), 즉시 효과는 PMMP 식.
- R7 합침(사용자 결정 변경): 숫돌 블록·아이템·갈기 규칙·놓기·받침·떨굼·창·시험·P6 `grindstone` 시나리오·Go 봇 파일·m9-facts STATIONS·9.1 dragonfly 숫돌 행을 지움. 사용자 예외가 숫돌뿐이라 `xtask/data/exceptions.json`과 생성기 예외 경로도 지움(죽은 코드). 프로토콜 상수·바닐라 등록·업그레이드 표는 다른 PMMP에 없는 블록처럼 그대로. 개수 시험은 M8 값(상태 12,468, 아이템 1,613). 숫돌 창이 열린 채 저장된 개발용 체크포인트는 못 읽음.
- U6 합침: 규칙 하나 `Settings::has_orbs()` = `experience` 그리고 `experience-orbs`. `drop_orbs`, 구슬 API, 청크 로드 때 저장된 구슬, 병 던지기, 블록·화로·좀비 경험치 계산이 씀. 결정(추천안): 구슬이 꺼지면 구슬만 없어지고 경험치를 바로 주지 않음. 점수·레벨은 `experience`만 따름.
- U7 합침: 내구도를 깎는 모든 경로가 `interact::breaking::damage` 한 곳을 지나고, `durability = false`면 계산과 난수 뽑기를 하지 않음. `durability`만 끄면 갑옷은 막되 닳지 않음, `armor`를 끄면 방어·갑옷 소모·가시가 모두 없음.
- 남은 지적 4 합침: `.clippy.toml` 금지 목록에 std `Read::read_to_end`·`Read::read_to_string`·`io::read_to_string`. 사용처는 `src/fs.rs` 한 곳(`take` 상한, `safety` 태그 `#[expect]`)과 xtask 시험. LevelDB·zip은 상한 있는 `read_exact`라 해당 없음.
- 남은 지적 9 합침: 불 확률 1/3을 `event::DEFAULT_FIRE_CHANCE` 하나로.
- 남은 지적 6 합침: 로그 스레드가 끝을 알리고 종료 때 최대 5초(`END_WAIT`)만 기다린 뒤 스레드를 놓음.
- 남은 지적 5 합침: 생성기가 인코더에도 디코더와 같은 Mojang 상한 검사(`string_max`, `length_max`, 원소·옵션 포함)를 넣음(packet 62, types 53), 손으로 쓴 PackSetting도. 상한을 넘는 사용자 패킷은 인코드에 실패해 그 연결이 끝남(기존 정책, `Player::send_packet` 문서). 책 시험을 512글자이면서 768바이트 안의 페이지로 고침.
- U4 합침: 교육판 아이템은 클라이언트 안 chemistry 팩으로 그려지고, 그 팩은 StartGame `EducationFeaturesEnabled`가 켜진 월드에서만 적용됨(BDS `resource_packs/chemistry`가 `chemistry` 기능 요구). Anchor는 PMMP처럼 꺼서 보냈음. `education-items = true`면 켬. false면 크리에이티브·명령 Item enum·아이템 인자 해석에서 뺌(`item::is_education` 한 곳). 결정: ItemRegistry에는 남김(옛 저장·블록에서 나온 아이템을 클라이언트가 읽어야 함). P6 하네스는 BDS 월드와 맞추려 false. `/particle` 아이템 데이터는 거르지 않음.

## 11. 버전업
gophertunnel 새 정식 태그 → IR 비교와 외부 불신 검토 → `cargo xtask gen`과 P1~P5 → 기준 목록 교체와 상태 대조 → 새 버전 BDS로 P6 → 깨지는 변경으로 표시.

## 12. 열린 결정
| # | 결정 | 기본값 | 정하는 때 |
|---|---|---|---|
| 1 | 블록 ID 방식 | 해시(M2 P6로 확정) | 정함 |
| 2 | 기준 목록 출처 | dragonfly + CloudburstMC(M4에서 확정, 두 목록이 같음) | 정함 |
| 3 | PMMP와 다른 이벤트 의미 | 8.2절 | M8(사용자가 원하면 바꿈) |
| 4 | 사용자 저장 공급자 trait | 두지 않음 | 원자 커밋을 지킬 수 있는 요구가 생기면 |
| 5 | Bedrock(LevelDB) 월드 가져오기 | 포함(가져오기 도구), M9 운영 기능에서 만듦(10.6) | 정함 |
| 6 | HTTPS 시그널링 | 평문 기본, 인증서를 설정하면 HTTPS(M3 구현) | 정함 |
| 7 | `Vec3` 성분 | f64, 프로토콜 경계에서 f32(10.7) | 정함 |
| 8 | Linux 검증 | 나중에(사용자가 지시할 때). 그때까지 Windows만, 설계는 OS 중립 | 사용자 지시 |

## 13. 상태
| 단계 | 상태 | 메모 |
|---|---|---|
| M0 | 완료(2026-10-02) | verify PASS |
| M1 | 완료(2026-10-03) | 10.1절. 패킷 236개 생성, P1~P4 통과(코퍼스 15,104, 변형 236,000, 패닉 0), verify PASS |
| M2 | 완료(2026-10-03) | 10.2절. P5 6개, P6 스폰 창 BDS와 같음(zlib, Snappy), verify PASS |
| M3 | 완료(2026-10-03) | 10.3절. P5 NetherNet 9개, P6 NetherNet 스폰 창·응답 SDP·송신 조각이 BDS와 같음, 폴백, vendor 사본 3개(재현 시험 16개), verify PASS |
| M4 | 완료(2026-10-03) | 10.4절. PMMP 덤프(상태 11,420, 아이템 1,613), 기준 목록(22,091·2,076), Anchor 상태 12,468, 해시 골든·상태 대조 통과, ItemRegistry·CreativeContent·CraftingData 송신, P6 스폰 창·아이템 대조 BDS와 같음, verify PASS |
| M5 | 완료(2026-10-03) | 10.5절. CPU 풀(rayon), 입력·출력 틱 단계, 입력 로그·크래시 로그·재생, 시드 8개에서 스레드 1과 N의 해시·출력·재생 동일, sctp-proto 이차 비용 수정, verify PASS |
| M6 | 완료(2026-10-03) | 10.6절. redb 사본(#1517·열기 패닉 수정), 저장 스레드·스냅샷 커밋·손상 격리·시작 백업, 서브청크 요청 방식 청크 전송, 블록 변경과 연결 속성(47단계 BDS와 같음), 예약 작업 API, 강제 종료 50회·장애 주입 1,000회 복제·손상 0, P6 청크·블록 BDS와 같음, SCTP RTO 1초, verify PASS |
| M7 | 완료(2026-10-04) | 10.7절. PMMP 덤프 확장(동작 데이터·효과·인챈트·태그), 조명, 저장 형식 3, 프로토콜 상수 생성, 인벤토리·제작·작업대 계열·블록 엔티티, 부수기·놓기·쓰기, 블록 갱신·농사·액체·불, 엔티티(아이템·구슬·낙하·발사체·TNT·NPC), 생존·스폰 창, 거래, 자동 시나리오·보존·강제 종료(아이템 흐름)·결정성·누수 시험, P6 craft·furnace·enchant·crops·trade BDS와 같음, verify PASS |
| M8 | 완료(2026-10-04) | 10.8절. 이벤트 93개(검사 → 리스너 → 다시 검사 → 적용 → 모니터, defer·perform), 플레이어·엔티티·월드·서버 API, 명령(기본 37개)·권한·op·밴·화이트리스트, 폼·스코어보드·보스바·메뉴 창·가짜 블록, 패킷 훅·시작 게임 훅·리소스팩, 예약·비동기·사용자 데이터, 커스텀 블록·아이템·엔티티·레시피·거래, 빠진 PMMP 엔티티 5종, 블록 엔티티 API, 예제 4개(봇 시험), PMMP 대응표 10,217행 빈칸 0(안 함 5,029), P6 명령·폼·입자·킥 대조, 성능 M7 이하, verify PASS |
| M9 | 완료(2026-10-05) | 10.9절. 운영(재생·백업·정상 종료·가져오기·콘솔 로그·status), 설정 스위치 21개·월드별 값, 날씨·번개, 숫돌, 월드 수명주기, 대응표 재검토 구현, 검토 6회와 감사(반박 거친 지적만 수정), P6V로 BDS 대조 검증 필요 닫음, verify PASS, P6 6개 PASS(`p6 anchor`는 시험 도구 타이밍 차이로 FAIL, 남은 지적) |

상태 값: 시작 전, 진행 중, 완료(날짜), 막힘(이유).
