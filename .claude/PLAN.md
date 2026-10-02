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
xtask/                개발 도구(verify, gen). go/: Go 추출기와 시험 도구(M1). data/: IR, 덤프, 기준 목록, 출처 커밋
```
- 크레이트는 `anchor`와 `xtask` 둘이다(개발 도구 의존성이 라이브러리에 들어가지 않게). 모듈과 폴더는 필요할 때 만든다. 내려받은 외부 소스는 `.cache/`(M1에서 .gitignore에 더함).
- 도구 설정을 점 이름으로 두면 상위 폴더와 홈의 설정이 섞이지 않는다(조사 때 실측). rust-toolchain.toml과 Cargo.lock은 점 이름을 쓸 수 없다.
- 서버 실행 폴더는 `anchor.toml`(8.5절)과 `data/`(`world.redb`, `identity.key`, `auth.json`, `backups/`, `crash/`)뿐이다. ops, 화이트리스트, 밴은 `world.redb`의 표다. 로그 출력은 사용자 바이너리가 정한다.

## 2. 네트워크
| 포트(기본, 설정 가능) | 용도 |
|---|---|
| UDP 19132 | RakNet(폴백), 서버 목록 핑 |
| TCP 19132 | NetherNet HTTP 시그널링 |
| UDP 19133 | NetherNet 게임 데이터(모든 피어가 소켓 하나) |
- IPv4와 IPv6를 같은 포트로 받는다. Windows UDP의 WSAECONNRESET은 무시하고 계속 읽는다(tokio-rs/tokio#2017).
- 검증 필요(M3): 클라이언트가 시그널링에 쓰는 포트와 스킴, 서버 목록 정보를 어디서 얻는지, UDP만으로 직접 접속하는 길이 있는지(있으면 TCP를 뺀다).

### 2.1 NetherNet
정답은 고정한 gophertunnel의 go.mod가 고정한 df-mc/go-nethernet이다(v1.62.0 기준 `endpoint/handler.go`). Mojang NetherNet 온보딩 가이드(https://mojang.github.io/bedrock-protocol-docs/guides/nether-net-onboarding-guide/)는 보조 근거다.
- 시그널링: 연결 시도마다 HTTP 요청 한 번(trickle ICE 없음). `GET /v1/join`은 2xx와 JSON `{name, protocol, version, level, players, maxPlayers, gameType}`, `POST /v1/join/{networkId}`는 SDP 오퍼를 받아 SDP 응답을 돌려준다. 최소 HTTP만 구현하고, 본문은 `Content-Length`와 `chunked`를 받는다(둘이 함께 오면 거부). 헤더, 본문, 시간, IP별 연결에 상한을 둔다.
- 오퍼의 `a=identity`: GameServerToken(RS256, JWKS)과 오퍼 지문에 대한 분리 JWS(ES384, `cpk`). 그 밖의 `alg`는 거부한다. 없으면 온라인 모드는 거부, 오프라인 모드는 받는다.
- 응답의 `a=identity`: 모든 응답에 반드시 넣는다(없으면 클라이언트가 거부). 서버 신원 JWT(자체 서명, 장기 P-384 키)와 응답 지문에 대한 분리 JWS다. 검증 필요(M3): 위치(가이드끼리 다름)는 기준 서버 대조(4절 P6)로 정한다.
- 신원 키: 처음 실행할 때 만들고 원자적으로 저장하며 재시작해도 유지한다(바뀌면 모든 플레이어에게 확인 창이 다시 뜬다). 깨졌으면 새로 만들지 않고 시작을 거부한다. JWT는 만료 전에 다시 발급한다.
- 신뢰: 기본은 평문 HTTP(처음 보는 키에 확인 창, TOFU). 인증서를 설정하면 HTTPS.
- WebRTC: 클라이언트는 host 후보만 쓴다. 서버는 설정한 공인 주소를 광고한다. 데이터 채널은 `ReliableDataChannel`과 `UnreliableDataChannel` 둘이고, 메시지 앞 1바이트는 남은 조각 수다. 재조립에 상한을 둔다. 다른 채널이 열리면 끊는다. 비신뢰 채널은 상한 안에서 읽고 버리며 기록한다(내용 형식은 공식 클라이언트만 알 수 있어 어느 경우든 받는다).
- 신원 결합: Login의 클라이언트 키가 오퍼에서 증명된 `cpk`와 같아야 한다. NetherNet은 Minecraft 암호화와 배치 헤더 0xfe가 없고, 압축 바이트는 그대로 있다.

### 2.2 RakNet
- 서버 쪽만, 기본은 직접 구현이다(조건을 만족하는 크레이트가 있으면 M2에서 비교). 범위는 핑, 연결 수립, 신뢰성·순서·조각, ACK/NAK, 혼잡 제어다. 단조 시계를 쓰고 ACK 범위, 조각, 재조립, 대기 데이터에 상한을 둔다.
- 검증 필요(M2): RakNet 프로토콜 버전과 MTU 협상(go-raknet 기준). 26.60은 RakNet을 "deprecated"로만 표시했다(제거 아님). RakNet 코드는 어느 경우든 유지한다.

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
  - P6 기준 서버 대조(M2부터): 목표 프로토콜의 BDS 복사본(`.cache/bds/<버전>/`, 원본에서 실행 안 함)을 `online-mode=false`로 띄우고 콘솔 명령으로 상황을 만든다. 2026-10-01 기준 후보는 `C:\Users\User\Desktop\bedrock-server-1.26.52.3`(검증 필요(M2): 프로토콜 번호). 봇과 시나리오는 BDS에서 먼저 통과해야 믿는다. 같은 시나리오를 BDS와 Anchor에 돌려 받은 패킷의 순서, 종류, 정해진 필드를 비교하고(세계마다 다른 값은 뺀다), 다르면 BDS를 따른다. 시나리오는 마일스톤마다 늘린다.
- gophertunnel도 형식 버그를 자주 고친다. P3 통과는 gophertunnel과 같다는 뜻일 뿐이고, P6와 다르면 CLAUDE.md 7절을 따른다.

## 5. 틱과 스레드
- 20 TPS, 엔진 시계. 50ms를 넘으면 최대 1틱만 따라잡고 기록한다. 대기는 `std::thread::sleep`(Rust 1.98 Windows는 이미 고해상도 타이머).
- 틱 함수는 `fn step(state: &mut State, input: &[Input]) -> Output`이고 입력은 (세션 ID, 도착 순번)으로 정렬한다.
- 단계: 1 입력(직렬: 입력, 수신 훅, 비동기 완료) → 2 계산(병렬 읽기 전용) → 3 적용(직렬: 결과, 이벤트, 예약 작업) → 4 출력(병렬 읽기 전용: 패킷 생성, 송신 훅).
- 직렬 단계 밖의 사용자 코드는 셋뿐이다: 송신 훅(읽기 전용), 월드 생성기(시드와 좌표만 받는 순수 함수), `spawn`/`spawn_blocking` 본문(게임 상태에 닿지 못함).
- 스레드 예산: 틱 1, 저장 1, 네트워크 워커, CPU 풀(설정 `network-threads`, `cpu-threads`). 블로킹 풀은 따로 상한(tokio 기본 512는 쓰지 않음).
- 결정성: 실수 `max`, `min`, `clamp`는 ±0을 비결정적으로 골라서 틱 코드는 엔진 도우미 하나를 쓴다. 입력 로그(입력, 비동기 결과, 시각)는 마지막 DB 커밋부터 기록하고, 패닉하면 `data/crash/`에 남긴다.

## 6. 월드와 저장
- 서브청크는 팔레트와 비트 배열이다. 월드는 영구와 임시(저장 안 함) 두 가지다. 생성기는 void와 flat, 사용자 생성기는 trait이다. 청크 전송은 시야, 틱당 수, 스폰 반경에 상한을 둔다(방식은 M6).
- 저장(redb, 파일 하나): 표는 `chunks`(키 = world_id, x, z, 종류를 고정 길이 빅엔디안, 부호 비트 뒤집음), `players`(키 = XUID, 오프라인은 `offline:`+이름), `world_meta`, 사용자 표, ops·화이트리스트·밴. 값은 압축 + 자체 체크섬이고, 디스크 값도 외부 입력으로 다룬다. 디스크에는 블록 이름과 상태만 쓴다.
- 커밋: 저장 주기(기본 10분)마다 틱 경계의 바뀐 부분을 COW 스냅샷으로 떼어 저장 스레드가 한 트랜잭션으로 커밋한다. 모든 쓰기 트랜잭션에 `set_quick_repair(true)`. 관리 명령(op, ban 등)은 다음 틱 경계에 커밋을 요청한다. 외부 시스템에 쓴 사용자 효과는 크래시 때 돌아가지 않는다(문서에 적는다).
- 실패와 손상: 커밋 실패는 핸들을 다시 열어 3번 재시도하고, 그래도 실패하면 플레이어를 내보내고 종료한다. redb 읽기는 페이지 체크섬을 확인하지 않으므로(cberner/redb#1334) 매 시작마다 닫힌 파일을 백업한 뒤 `check_integrity()`를 하고, 손상이면 시작을 거부하고 원본을 남긴다. 실행 중 손상 청크는 격리한다. 백업은 읽기 트랜잭션으로 새 파일에 복사한다(열린 파일 복사는 Windows에서 실패). `compact()`는 시작과 종료 때만.
- 서버 조명(사용자 결정): 게임 로직용으로 계산하고 클라이언트로는 보내지 않는다. 블록 값(밝기, 필터, 하늘빛 차단)은 PMMP 덤프에서 온다. 블록 변경은 증분 갱신하고 틱당 처리량에 상한을 둔다(PMMP는 상한 없음, dragonfly는 갱신 안 함). 시간대 보정은 PMMP 식을 표로 만든다(`cos` 금지). 계산 안 된 위치는 `None`. 설정 `lighting = false`면 계산하지 않고 모든 위치를 블록빛 0, 하늘빛 15(하늘에 열림)로 본다(0이면 작물이 안 자라고, 블록빛 15면 얼음과 눈이 녹아서). 그 결과(작물이 지하에서도 자람 등)를 rustdoc에 적는다.
- 랜덤 틱: 설정 `tick-radius`, `random-tick-speed`, `disable-block-ticking`(PMMP 관례). 대상은 반경 안이고 주변 3x3이 로드되고 조명 계산을 마친 청크다(`lighting = false`면 조명 조건은 만족). 청크 순서는 정렬 순서, 난수는 엔진 RNG(dragonfly의 비결정성을 고친다). 동작 기준은 PMMP.

## 7. 블록과 아이템 데이터
- 종류, 상태, 직렬화 규칙, 동작은 PMMP 5.44.3(Bedrock 1.26.30 데이터)에서 온다. 그 뒤 추가된 블록과 아이템은 없다(채우지 않는다).
- 추출(M4): PMMP 마지막 커밋 6a7cc02를 `.cache/`에 받아 한 번 실행해 덤프한다(소스 파싱 안 함). 블록 상태마다 Bedrock 직렬화, 조명 값, 랜덤 틱 여부를, 아이템마다 직렬화를 JSON으로 쓴다(조사 때 실측: 상태 11,420, 오류 0, 아이템 323). PMMP 상태와 Bedrock 상태는 1:1이 아니다(1,888개가 겹침. 그 값은 블록 엔티티나 런타임 데이터로 둔다).
- 버전별 번호는 기준 목록에서 온다(검증과 번호에만 쓰고 블록을 만들지 않는다): 블록 상태는 dragonfly `block_states.nbt`, 교차 확인 CloudburstMC `block_palette.nbt`(완전히 같을 때만 채택, 다르면 BDS를 따름). 아이템 런타임 ID는 dragonfly `vanilla_items.nbt`(버전마다 바뀜).
- 블록 ID는 해시 방식(FNV-1a 32, dragonfly `network_block_hash.go`)이다. 기준 목록 전체가 CloudburstMC `network_id`와 같은지 골든 시험한다. 검증 필요(M2): BDS가 해시 방식을 쓰는지(P6). 아니면 순서 기반으로 바꾼다.
- 상태 대조: 구현한 모든 상태가 기준 목록에 있어야 한다. 26.50에서는 울타리 등 71개 타입에 연결 속성(`minecraft:connection_*`)과 계단 `minecraft:corner`가 생겼다. 검증 필요(M4): 서버가 이 속성을 이웃으로 계산해야 하는지(P6에서 BDS 동작을 따른다).
- 저장 업그레이드 규칙은 PMMP 업그레이드 스키마 형식이다(스키마 저장소에 결함이 있어 검증 후 들인다).
- 알 수 없는 블록은 클라이언트에 `minecraft:info_update`로 보내고 저장 원본은 보존한다. 클라이언트가 보낸 알 수 없는 블록이나 아이템은 그 요청만 거부한다.
- 검증 필요(M2): 26.50의 data-driven 블록 정의를 StartGame에 보내야 하는지(보내더라도 정의만, 블록 목록에 더하지 않는다).

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
- 게임 범위(사용자 결정): PMMP 동작 ∪ dragonfly 야생 시스템 − CLAUDE.md 1절 제외. 블록은 PMMP에 있는 것만이라 숫돌, 퇴비통, 다시마는 없다.
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
hunger = true
# Main world. Created on first start with level-type: "flat" or "void".
level-name = "world"
level-type = "flat"
# Whole number as text. Empty: random, chosen once when the world is created. Other text stops the start.
level-seed = ""
# Largest view distance a player may use, in chunks (2 to 64).
view-distance = 16
# false: player inventories and positions are not kept between joins.
save-player-data = true
# Message players see when the server stops.
shutdown-message = "Server closed"
# Resource pack files (.mcpack or .zip), relative to this file. Earlier entries override later ones.
# An encrypted pack needs its key in "<file>.key" next to it.
resource-packs = []
# true: players must accept the resource packs to join.
force-resources = false

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
# false: no light is calculated; every check sees an open sky (see the docs for the effects).
lighting = true
# Minutes between whole-state saves (1 to 1440).
save-interval-minutes = 10
# false: Anchor does not read commands from standard input.
console-input = true
```
- PMMP에서 이름을 바꾼 것: `batch-threshold` → `compression-threshold`, `blocks-per-subchunk-per-tick` → `random-tick-speed`(Java판 이름과 기본값 3, Bedrock 기본 1과 다름), `chunk-sending.per-tick` → `chunks-per-tick`, `ticks-per.autosave` → `save-interval-minutes`, 중첩 경로는 평평하게. PMMP에 없는 키: NetherNet 포트와 TLS, `public-ip`, `max-batch-bytes`, `raknet`, 스레드 수, `lighting`, `hunger`(사용자가 확정한 토글).
- PMMP에서 뺀 것: PHP 전용(메모리, GC, async-workers, timings, 통계·보고·업데이트), 플러그인, 범위 밖(query, upnp, 월드 형식), 항상 켬(암호화), 구조로 대체(XUID 검사, 생성기 문자열, 별칭, 월드 목록, 언어, 디버그 레벨, IPv6 포트, auto-save).
- 검증: 모르는 키와 표, 타입 오류, 범위 밖 값, hardcore인데 hard가 아님은 시작을 거부한다(조용히 고치지 않는다). 빠진 키는 기본값이다. 모르는 표 검사는 `build()`에서 한다. `spawn-radius`와 `tick-radius`의 실제 값은 min(값, `view-distance`)이다.
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
| sandertv/gophertunnel(MIT) | v1.62.0(1.26.50, 2193). M1에서 다시 정함 | 패킷, 로그인, 암호화, 압축 형식, Go 봇 | 형식 버그 수정이 잦음, 주석과 코드 불일치, 4절 결함 |
| sandertv/go-raknet, df-mc/go-nethernet | gophertunnel go.mod가 고정 | RakNet, NetherNet 형식 | go-nethernet은 비신뢰 채널을 읽지 않음 |
| pmmp/PocketMine-MP(LGPL-3.0, 보관) | 6a7cc02 | 블록·아이템 종류와 동작, API | 1.26.30 데이터, 상태가 1:1 아님, 공개 때 라이선스 검토 |
| pocketmine/bedrock-data(CC0) | 6.7.0+bedrock-1.26.30 | 레시피 등 데이터 | 26.40 이후 없음 |
| df-mc/dragonfly(MIT) | 커밋(M4) | 기준 목록, 야생 시스템 참고 | 해시 함수 패닉, 비결정 코드, 조명 갱신 없음 |
| CloudburstMC/Data(Apache-2.0) | 커밋(M4) | 블록 ID 골든, 교차 확인 | 교차 확인에만 |
| Mojang/bedrock-protocol-docs | 목표 버전(M1) | 필드 길이 상한, NetherNet 가이드 | gophertunnel과 다르면 CLAUDE.md 7절 |
| BDS(기준 서버) | 목표 프로토콜 배포본 | P6 대조 | 실행만, 분석하지 않음 |

### 9.2 추천 후보 (2026-10-01 조사, 강제 아님)
| 용도 | 추천 | 가드 |
|---|---|---|
| 저장 | redb(사용자 지정) | 4.4.0 이상(쓰기마다 메모리 증가 #1517 수정), 없으면 수정 커밋. 손상 파일 결함(#1503, #1505)과 읽기 체크섬 없음(#1334)은 6절 규칙으로 막는다 |
| 비동기 런타임 | tokio 1.x | 멀티스레드 런타임 하나, 상한 없는 채널 금지 |
| CPU 풀 | rayon | 고정 크기 풀 하나 |
| 암호 | aws-lc-rs | Windows는 NASM 필요. 같은 키로 서명과 ECDH 하는 변환은 검증 필요(M2) |
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

## 11. 버전업
gophertunnel 새 정식 태그 → IR 비교와 외부 불신 검토 → `cargo xtask gen`과 P1~P5 → 기준 목록 교체와 상태 대조 → 새 버전 BDS로 P6 → 깨지는 변경으로 표시.

## 12. 열린 결정
| # | 결정 | 기본값 | 정하는 때 |
|---|---|---|---|
| 1 | 블록 ID 방식 | 해시 | M2(P6) |
| 2 | 기준 목록 출처 | dragonfly + CloudburstMC | M4 |
| 3 | PMMP와 다른 이벤트 의미 | 8.2절 | M8(사용자가 원하면 바꿈) |
| 4 | 사용자 저장 공급자 trait | 두지 않음 | 원자 커밋을 지킬 수 있는 요구가 생기면 |
| 5 | Bedrock(LevelDB) 월드 가져오기 | 포함(가져오기 도구) | M6 |
| 6 | HTTPS 시그널링 | 평문 기본 | M3 |
| 7 | `Vec3` 성분 | f64, 프로토콜 경계에서 f32 | M7 |
| 8 | Linux 검증 | 나중에(사용자가 지시할 때). 그때까지 Windows만, 설계는 OS 중립 | 사용자 지시 |

## 13. 상태
| 단계 | 상태 | 메모 |
|---|---|---|
| M0 | 완료(2026-10-02) | verify PASS |
| M1 | 시작 전 | |
| M2 | 시작 전 | |
| M3 | 시작 전 | |
| M4 | 시작 전 | |
| M5 | 시작 전 | |
| M6 | 시작 전 | |
| M7 | 시작 전 | |
| M8 | 시작 전 | |
| M9 | 시작 전 | |

상태 값: 시작 전, 진행 중, 완료(날짜), 막힘(이유).
