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
vendor/               결함을 고친 의존성 사본(사용자 결정, M3). 9.1
```
- vendor 규칙(사용자 결정): 결함 부분만 고친 사본을 `[patch.crates-io]` path로 고정한다. 결함마다 재현 시험을 두어 고치기 전 실패, 고친 뒤 통과를 확인한다. 상류가 고친 릴리스를 내면 사본을 지우고 원래 크레이트로 돌아간다. 상류에 이슈나 PR은 올리지 않는다. 사본은 작업공간에서 제외해(`exclude`) 우리 lint를 걸지 않고, 자체 시험이 있는 사본은 verify가 따로 돌린다.
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
| sandertv/gophertunnel(MIT) | 80c811b(v1.62.0 뒤, 1.26.50, 2193) | 패킷, 로그인, 암호화, 압축 형식, Go 봇 | 형식 버그 수정이 잦음, 주석과 코드 불일치, 4절 결함, NBT map 쓰기 순서가 무작위(시험 도구가 정렬해 씀). 로그인(M2): 정규식에 앵커가 없어 이름·버전 검사가 느슨함 → 모든 글자 검사, `ServerAddress`를 DNS로 조회(블로킹) → 모양만 검사, 오프라인 UUID가 무작위 → `OfflinePlayer:`+이름의 v3, base64가 느슨함 → 엄격. 봇은 기본 스킨 JSON 두 개를 복사해 씀 |
| sandertv/go-raknet, df-mc/go-nethernet | gophertunnel go.mod가 고정 | RakNet, NetherNet 형식, 봇 | go-nethernet은 비신뢰 채널을 읽지 않음. go-raknet(M2, Anchor는 고쳐 구현): 쿠키가 CRC32, 오류마다 IP를 10초 막음(위조로 남을 막음), OCR2 응답 손실에 멈춤, 신뢰 인덱스 중복 제거 없음(조각 묶음 누수), 24비트 번호 넘침 미처리, 조각 512개 상한, 재조립 O(n²), 모르는 ACK 종류 무시, 혼잡 제어 없음, 사실상 상한 없는 큐, MTU 400 미만 처리 불일치, 순서 채널 무시 |
| pmmp/PocketMine-MP(LGPL-3.0, 보관) | 6a7cc02(M3부터 `.cache/pmmp`, M4부터 의존성은 `composer.lock` 그대로 `vendor`) | 블록·아이템 종류와 동작, API, 이름 규칙과 중복 로그인(M3), 덤프(M4) | 1.26.30 데이터, 상태가 1:1 아님, 공개 때 라이선스 검토. 중복 판정에 UUID도 쓰지만 Anchor는 사용자 결정대로 XUID·이름만. M4 덤프에서: smithing 레시피를 읽지 않음 → 덤프가 PMMP 아이템 해석기로 읽음, 모르는 인챈트를 버려 크리에이티브에 빈 인챈트 책 61개가 남음 → 뺌, 불길한 배너를 검은색(meta 0)으로 둠 → BDS처럼 흰색(15), 블록 아이템 상태가 바닐라 아이템 상태와 다름 → 네트워크 형태만 바닐라 상태(10.4) |
| pmmp/PHP-Binaries | php-build-524, PHP 8.4 PM5 Windows(zip sha256 `cf4023b8549c99eec47cfd403805ac7d78100fa366394dab4e45bb544dcc9be4`), `.cache/php-pm5` | PMMP 덤프 실행(개발 도구) | 일반 PHP에는 PMMP 확장이 없음 → PM5 빌드 |
| pocketmine/bedrock-data(CC0) | 6.7.0+bedrock-1.26.30 | 레시피 등 데이터 | 26.40 이후 없음 |
| df-mc/dragonfly(MIT) | v0.11.5 `4c7b5074`(1.26.50), `.cache/dragonfly` | 기준 목록(블록 상태, 아이템, 크리에이티브 아이템 상태), 야생 시스템 참고 | 해시 함수 패닉(Anchor는 코드를 쓰지 않고 시험에서 다시 구현), 비결정 코드, 조명 갱신 없음. 크리에이티브 목록은 빈 블록 속성을 생략함(빈 속성으로 읽음) |
| CloudburstMC/Data(Apache-2.0) | `659ce1e`(26.50), `.cache/cloudburst-data` | 블록 ID 골든, 교차 확인, 크리에이티브 아이템 상태 | 팔레트는 gzip big-endian NBT. dragonfly와 블록·아이템 목록이 같음을 도구가 확인한다 |
| Mojang/bedrock-protocol-docs | 목표 버전(M1) | 필드 길이 상한, NetherNet 가이드 | gophertunnel과 다르면 CLAUDE.md 7절 |
| BDS(기준 서버) | 1.26.52.3(프로토콜 2193), `.cache/bds/` 복사본 | P6 대조 | 실행만, 분석하지 않음. RakNet 전송을 "플레이어 접속 불가"로 기록함 |
| libdeflater(MIT, libdeflate 1.26) | =1.26.1 | zlib 압축(level 6에서 miniz_oxide보다 2~4배 빠름, 실측) | `use_rust_alloc` 기능의 OOM null 역참조 → 기본 기능만. `Decompressor` 내부 상태 패닉 → `.clippy.toml` 금지. `Compressor::new`는 malloc 실패에만 패닉(생성 때만, 외부 입력 닿지 않음) |
| miniz_oxide(MIT/Apache-2.0/Zlib) | =0.9.1 | zlib 해제 | `decompress_to_vec*`는 상한 없음 → 금지, 상한까지만 자라는 자체 루프 |
| serde, serde_json, toml | =1.0.229, =1.0.151, =1.1.6 | 설정, 인증 캐시, JWT, IR 읽기 | toml은 깊이 80에서 멈춤(`unbounded` 기능이 켜지면 사라짐), toml 오류 `Display` 안의 `expect` → `message()`와 `span()`만 씀. `serde_json::from_reader` 금지 |
| tokio(MIT) | =1.53.1 | 런타임, 상한 큐, UDP, 시그널 | 상한 없는 채널, `blocking_*`, `block_on`, 런타임 생성, 시계 진입점을 금지 목록에 둠 |
| aws-lc-rs(ISC/Apache-2.0) | =1.18.1, 기본 기능 끔 | ES384, RS256, ECDH P-384, SHA-256, AES-256-CTR, 난수 | ECDSA 공개키가 SPKI 뒤 바이트와 SEC1 점도 받음 → DER SEQUENCE 하나만, 스트리밍 CTR이 무작위 IV → 금지하고 자체 카운터, 서명 키와 ECDH 키가 다른 타입 → PKCS#8로 잇고 공개키 같음을 확인, 난수와 키 생성 진입점과 `SystemRandom` 타입 금지 |
| rustls(aws_lc_rs, ring 끔), webpki-roots | =0.23.45, 1.0.9 | 인증 문서 HTTPS | 공급자가 없으면 패닉 → 늘 aws-lc-rs를 줌. ring은 메타데이터에만 있고 빌드되지 않음(`allowed-duplicate-crates` windows-sys) |
| ureq(MIT/Apache-2.0) | =3.4.2 | 인증 문서 받기(블로킹 풀) | 시간 상한이 있으면 DNS마다 분리 스레드 → 인라인 resolver(DNS는 OS 상한), 본문 상한이 상한과 같은 크기를 거부 → 1 MiB 미만, 기본으로 프록시 환경 변수를 읽음(둠). 블로킹 호출과 상한 없는 본문 읽기 금지 |
| tracing(MIT) | =0.1.44, std만 | 로그 | `attributes` 기능을 꺼 syn 2를 뺌 |
| socket2(MIT/Apache-2.0) | =0.6.5 | IPv6 전용 소켓, 소켓 버퍼 | 블로킹 입출력 진입점 금지 |
| str0m(+is, str0m-proto, str0m-aws-lc-rs)(MIT/Apache-2.0) | =0.24.0(0.11.1, 0.7.0, 0.6.0), 기본 기능 끔 + `aws-lc-rs`. str0m은 `vendor/str0m` 사본 | NetherNet WebRTC(SDP API는 원격 정보 입력에만, 응답 SDP는 자체) | 사본에서 고친 것: 상류 main 18ffec80(#1062, 미릴리스)을 옮겼다. ① SCTP 송신 버퍼가 128 KiB 고정이라 그보다 큰 메시지(262,144바이트 조각)를 영원히 못 보냄 → `set_sctp_max_buffered_amount` 설정(Anchor 1 MiB), ② `poll_output`이 대기 SCTP 패킷마다 재귀해 큰 송신에서 스택 넘침(실측, `catch_unwind`로 못 막고 프로세스가 죽음) → 반복문. 재현: `peer::tests::a_segment_of_the_remote_max_message_size_is_sent`, `channels_open_and_messages_round_trip_both_ways`(원본에서 둘 다 실패). 그 밖: 기본 설정이 전역 공급자·fastrand ICE 자격·자체 인증서 → 모두 명시 설정과 금지 목록, 직접 API는 원격 `max-message-size`를 못 받음 → SDP API에 `setup:passive`로 적은 오퍼를 줌(DTLS client 역할 유지), ICE는 끊김만 알림·SCTP 무한 재전송 → 피어 마감과 무응답 10초, `accepts`는 선형 → ufrag·주소 색인, 남은 내부 패닉 → 피어마다 `catch_unwind`, ICE 비밀번호가 trace에 찍힘 → `pii` 끔, 설정 로그 금지 |
| dimpl(MIT/Apache-2.0) | 0.7.4, `vendor/dimpl` 사본 | DTLS 1.2 | 사본에서 고친 것: ① ChangeCipherSpec 전 같은 epoch-1 레코드 두 번에 assert(engine.rs:364) → replay처럼 버림, ② 피어 인증서가 poll 버퍼(str0m 2,000바이트)보다 크면 assert(dtls12·13 client.rs) → `BufferTooSmall`을 돌려 str0m이 오류로 끊음, ③ Auto 모드 supported_versions 잘림에 범위 밖 읽기(auto.rs:436) → 길이 검사, ④ 받는 레코드마다 버퍼 풀이 자람(32,768개에 약 38 MiB 실측) → 상류 main 98f893dc를 옮기되 상류의 release `assert!` 대신 풀 밖 버퍼는 버림. 재현: `xtask/tests/dimpl_repro.rs` 5개(원본에서 모두 실패). 남김: 레코드 순서가 바뀐 데이터그램의 핸드셰이크 멈춤·dimpl끼리 재전송 폭주(#168/#169, 미병합, 원격은 dimpl이 아니고 10초 마감이 있어 영향은 핸드셰이크 실패) → 검증 필요(M5). DTLS 1.2 고정, `log` 사용 |
| sctp-proto(MIT/Apache-2.0) | 0.10.5, `vendor/sctp-proto` 사본 | SCTP | FORWARD-TSN `+1` 루프는 0.10.4에서 고침(확인). 사본에서 고친 것: ① 수신 창이 0이어도 빠진 청크를 무제한 받음 → 누적 TSN 바로 다음 청크만(RFC 4960 6.2), ② 받는 TSN 기록이 읽힌 데이터를 붙잡고 창에 안 셈 → 데이터 없이 TSN만 두고 기록마다 창에서 뺌, ③ 스트림 수 협상이 뒤집힘(INIT·INIT-ACK) → RFC 4960 5.1.1대로(업스트림 시험 기대값도 바로잡음), 협상 수 밖 스트림은 만들지 않음, 다시 연 스트림은 대기열에 한 번만, ④ 같은 원격의 새 INIT마다 연결이 생겨 쌓임 → 원격 하나에 연결 하나(RFC 4960 1.4), ⑤ 빈 DATA를 받아 둠 → 연결 종료(RFC 4960 6.2), ⑥ SACK 갭 오프셋 u16 넘침과 TSN `+1` 넘침(디버그 패닉) → 순환 산술. 재현: 사본 안 `association/anchor_test.rs` 8개와 `endpoint/anchor_test.rs` 1개(원본에서 모두 실패, verify가 사본 시험 258개를 따로 돌림). 남김: SSN 정렬 비교자가 전순서가 아님(실사용 방식 약 400만 번에서 재현 안 됨 → 고치지 않음, 패닉이 나도 피어 하나만 닫힘), 연결 중 INIT·ERROR 청크에 연결을 닫음(RFC 위반, 그 피어만 끊김), `Endpoint::connect`가 시계를 읽음, `log` 사용 |
| tokio-rustls(MIT/Apache-2.0) | =0.26.6, `aws_lc_rs`·`tls12`만 | HTTPS 시그널링 | 0.26.5 회귀(#204) 피함, 기한 없음 → 요청 10초 안에서만, 전역 공급자 builder 금지(`.clippy.toml`), 티켓과 세션 저장 끔 |
| go-nethernet(봇만) | gophertunnel go.mod 판 | 봇 NetherNet 다이얼 | 개발 도구. 후보 줄을 `candidate:candidate:`로 두 번 접두, 채널 열림 확인 전에 보냄(Anchor가 둘 다 받음) |
| go-jose, go-oidc(봇만) | gophertunnel go.mod 판 | 봇 토큰 발급과 봇 시험 | 개발 도구 |
| golang.org/x/tools(BSD-3) | v0.42.0(xtask/go) | 추출기의 타입 정보 로드 | 개발 도구, gophertunnel 의존성 버전을 바꾸지 않는 판을 골랐다 |

### 9.2 추천 후보 (2026-10-01 조사, 강제 아님)
| 용도 | 추천 | 가드 |
|---|---|---|
| 저장 | redb(사용자 지정) | 4.4.0 이상(쓰기마다 메모리 증가 #1517 수정), 없으면 수정 커밋. 손상 파일 결함(#1503, #1505)과 읽기 체크섬 없음(#1334)은 6절 규칙으로 막는다 |
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
  - 이름은 5.1을 따른다. Rust 키워드인 필드는 raw identifier(`r#type`)다. 문서는 gophertunnel 주석에서 오고, 없으면 생성기가 이름으로 한 줄을 만든다.
- 상한(디코드는 하나의 상한 도우미를 거친다):
  - 선언 길이는 남은 바이트 이하. 목록 개수는 1024 이하(gophertunnel `maxSliceLength`, 모든 목록에 같은 값. EntityMetadata 개수 포함).
  - 할당 예산: Reader는 배치 하나마다 예산(2 × `max-batch-bytes`)을 받고, 모든 `Vec`, `String`, NBT 할당은 `개수 × size_of`를 먼저 뺀다. 넘으면 크기 오류. 이유: 개수 상한만으로는 중첩 목록과 큰 원소가 입력 크기의 수십 배를 할당할 수 있다.
  - NBT 깊이 512, NBT 하나 4 MiB. 배치당 패킷 812개.
  - 필드별 상한(Mojang `maxItems`, `maxLength`)은 클라이언트 패킷을 처리하기 시작하는 M2에서 더한다(공통 상한과 예산이 M1의 안전성을 지킨다).
- Go 시험 도구(oracle):
  - `corpus`: 무작위 값을 고르며 쓰는 `protocol.IO` 구현으로 각 패킷의 Marshal을 돌려 표본을 만든다(시드 고정 PCG, 목록 길이 0~4). NBT는 키를 정렬해 쓴다. gophertunnel Reader로 다시 읽어 성공한 표본만 남긴다.
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
  - Go 봇 `xtask/go/bot`: gophertunnel `packet`, `login`과 go-raknet으로 만든 클라이언트다. Dialer는 시험용 토큰을 넣을 수 없어 쓰지 않는다. 시나리오는 spawn, big-skin(해제 6 MiB Login), online(시험용 발급자 키)이고, 받은 패킷을 순서대로 JSON 줄로 기록한다.
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
  - `cargo xtask verify`는 검사를 `target/verify`에서 돌린다(`cargo test --workspace`의 기능 통합이 달라 실행 중인 xtask.exe를 다시 빌드하려다 Windows에서 실패해서).
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
  - `nethernet.rs`: 구동부. UDP 소켓(IPv4, IPv6 전용, `nethernet-port`) 두 개와 모든 피어를 한 작업이 가진다(RakNet과 같은 구조, 잠금 없음. DTLS·SCTP가 한 작업에 모이는 비용은 M5 측정 대상으로 남긴다). 오퍼 처리 작업, 들어온 데이터그램 분배 색인(STUN USERNAME의 서버 ufrag → 피어, 성립 뒤 출발 주소 → 피어, 둘 다 `BTreeMap`), `Connection`(RakNet과 같은 `addr/recv/send/logged_in/close`에 증명된 `client_key`를 더함).
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
  - dimpl의 레코드 순서 바뀜 문제(#168/#169)와 sctp-proto의 수신 큐 정렬 비용(넣을 때마다 정렬)은 검증 필요(M5).
  - dimpl, sctp-proto는 `log`를 써서 Anchor(`tracing`)만 쓰는 바이너리에서는 그 기록이 사라진다. 사용자 바이너리가 다리를 둘지 정한다(M8 문서).
  - 모든 피어의 DTLS·SCTP가 구동 작업 하나에서 돈다. 비용은 M5 측정 대상이다.
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
  - 상태 업그레이드 `xtask/data/block_upgrade.json`: PMMP 업그레이드 스키마 형식(`addedProperties`)으로 1.26.30 → 1.26.50 차이를 적는다(손으로 쓴 데이터, 7절 "저장 업그레이드 규칙"과 같은 형식). 기본값은 BDS의 아이템 형태 값이다(`corner` = `none`, `connection_*` = 0, CloudburstMC 크리에이티브 목록으로 확인, P6로 다시 확인).
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
- 정한 값과 규칙(정본은 생성기와 `src/block.rs`, `src/item.rs`, `src/crafting.rs`):
  - 블록 조회: 네트워크 ID 정렬 배열 이분 탐색, 이름+속성은 이름 범위 안 이분 탐색(속성 8개 이하, 입력 순서 무관, 할당 없음). `PropertyValue<'a>`는 빌린 문자열을 받는다.
  - 아이템 조회(PMMP 규칙): 같은 이름(런타임 ID)·블록 중 meta가 같은 타입, 여럿이면 형태 NBT가 스택 NBT에 든 가장 구체적인 것, 없고 이름·블록의 타입이 하나면 그 타입(meta는 스택 상태: 도구 내구도, 폭죽 별 색). (이름, meta)는 유일하지 않다(배너, 버섯 블록). 블록 → 아이템 역조회는 덤프에 `asItem` 정보가 없어 두지 않는다(M7).
  - 크리에이티브 그룹: PMMP `CreativeInventoryCache` 규칙(처음 나올 때 번호, 같은 분류·그룹의 연속 구간이 한 그룹, 익명 그룹은 분류마다 하나이고 이름 있는 그룹이 나오면 새로 시작, 아이콘 NBT에 `___GroupBugWorkaround___`), 분류 번호 1~4. 107그룹.
  - CraftingData(PMMP `CraftingDataCache`·`TypeConverter`): 네트워크 ID 1부터 모양·무형·화로(PMMP `FurnaceType` 순)·대장장이 순, UUID nil, priority 50, 재료는 1.26.40 형식인 이름+meta(와일드카드 32767, 정확한 블록 재료는 PMMP 레거시 meta, 태그는 이름), 물약은 런타임 ID, 레시피 ID는 네트워크 ID의 10진 문자열(PMMP의 4바이트는 UTF-8이 아닐 수 있음), 해금 조건은 "항상"(`Some(context 1, 재료 없음)`, BDS도 늘 넣음), smithing은 dragonfly처럼 보냄, `clear_recipes` true.
- 남은 위험과 검증 필요:
  - 공식 클라이언트에서만 보이는 것: 교육판 아이템이 교육 기능을 끈 클라이언트의 크리에이티브 창에 어떻게 보이는지, BDS와 다른 레시피 ID·블록 이름(`crafting_table` 대 `deprecated`)이 레시피 책에 주는 영향. PMMP가 같은 값으로 쓰여 왔으므로 그대로 둔다. 검증 필요(M7): 레시피 책·제작 P6에서 다시 본다.
  - 클라이언트가 블록 아이템을 보낸 형태가 Anchor가 보낸 네트워크 블록 ID와 다르면 그 아이템은 `None`이다. 검증 필요(M7): 인벤토리 요청 P6.
  - Anchor 디코더의 목록 상한 1,024(10.1)는 ItemRegistry·CraftingData 전체를 디코드하지 못한다. 서버는 이 패킷을 받지 않으므로 영향이 없고, 시험은 1,024개씩 나눠 왕복한다.

## 11. 버전업
gophertunnel 새 정식 태그 → IR 비교와 외부 불신 검토 → `cargo xtask gen`과 P1~P5 → 기준 목록 교체와 상태 대조 → 새 버전 BDS로 P6 → 깨지는 변경으로 표시.

## 12. 열린 결정
| # | 결정 | 기본값 | 정하는 때 |
|---|---|---|---|
| 1 | 블록 ID 방식 | 해시(M2 P6로 확정) | 정함 |
| 2 | 기준 목록 출처 | dragonfly + CloudburstMC(M4에서 확정, 두 목록이 같음) | 정함 |
| 3 | PMMP와 다른 이벤트 의미 | 8.2절 | M8(사용자가 원하면 바꿈) |
| 4 | 사용자 저장 공급자 trait | 두지 않음 | 원자 커밋을 지킬 수 있는 요구가 생기면 |
| 5 | Bedrock(LevelDB) 월드 가져오기 | 포함(가져오기 도구) | M6 |
| 6 | HTTPS 시그널링 | 평문 기본, 인증서를 설정하면 HTTPS(M3 구현) | 정함 |
| 7 | `Vec3` 성분 | f64, 프로토콜 경계에서 f32 | M7 |
| 8 | Linux 검증 | 나중에(사용자가 지시할 때). 그때까지 Windows만, 설계는 OS 중립 | 사용자 지시 |

## 13. 상태
| 단계 | 상태 | 메모 |
|---|---|---|
| M0 | 완료(2026-10-02) | verify PASS |
| M1 | 완료(2026-10-03) | 10.1절. 패킷 236개 생성, P1~P4 통과(코퍼스 15,104, 변형 236,000, 패닉 0), verify PASS |
| M2 | 완료(2026-10-03) | 10.2절. P5 6개, P6 스폰 창 BDS와 같음(zlib, Snappy), verify PASS |
| M3 | 완료(2026-10-03) | 10.3절. P5 NetherNet 9개, P6 NetherNet 스폰 창·응답 SDP·송신 조각이 BDS와 같음, 폴백, vendor 사본 3개(재현 시험 16개), verify PASS |
| M4 | 완료(2026-10-03) | 10.4절. PMMP 덤프(상태 11,420, 아이템 1,613), 기준 목록(22,091·2,076), Anchor 상태 12,468, 해시 골든·상태 대조 통과, ItemRegistry·CreativeContent·CraftingData 송신, P6 스폰 창·아이템 대조 BDS와 같음, verify PASS |
| M5 | 시작 전 | |
| M6 | 시작 전 | |
| M7 | 시작 전 | |
| M8 | 시작 전 | |
| M9 | 시작 전 | |

상태 값: 시작 전, 진행 중, 완료(날짜), 막힘(이유).
