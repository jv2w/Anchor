---
paths:
  - "src/raknet.rs"
  - "src/raknet/**"
  - "src/nethernet.rs"
  - "src/nethernet/**"
  - "src/session.rs"
  - "src/session/**"
  - "src/login.rs"
  - "src/login/**"
  - "src/auth.rs"
  - "src/auth/**"
  - "src/crypto.rs"
  - "src/crypto/**"
  - "src/protocol/batch.rs"
  - "src/protocol/compression.rs"
  - "src/protocol/compression/**"
  - "src/tick/queue.rs"
  - "src/tick/spawn.rs"
---
# 네트워크 규칙

## 공통
- 두 전송(RakNet, NetherNet)은 같은 세션 상태 기계(`session`)에 싣는다. 로그인, 암호화, 압축, 상한은 전송과 무관하게 세션 위 한 곳에 둔다.
- 전송마다 IPv4 소켓 하나와 IPv6 전용 소켓 하나를 같은 포트로 연다.
- 전송마다 구동부 작업 하나가 두 UDP 소켓과 모든 연결(피어) 상태를 가진다. 잠금을 쓰지 않는다.
- 두 전송의 구동부는 닫기 전에 큐의 명령을 처리한다. NetherNet 구동부는 닫힘 신호보다 큐의 명령을 먼저 처리한다.
- Windows UDP의 WSAECONNRESET은 무시하고 계속 읽는다.
- IP별 상한의 키는 함수 하나로 만들고 RakNet, 시그널링, NetherNet 피어가 함께 쓴다.
- NetherNet의 TCP 시그널링을 유지한다.
- NetherNet 게임 데이터는 연결마다 소켓을 열지 않는다. `nethernet-port` 소켓 하나를 모든 피어가 나눠 쓴다.
- RakNet 코드는 늘 유지한다. `raknet = false`면 RakNet 연결은 받지 않지만 서버 목록 핑에는 답한다.

## 세션
- 해제, 복호화, 디코드, 로그인 검증은 연결마다 세션 작업 하나에서 한다(작업끼리 병렬).
- 세션과 틱 사이 큐가 가득 차면 틱은 기다리지 않고 그 세션을 끊는다.
- 로그인 순서의 패킷(RequestNetworkSettings, Login, ClientToServerHandshake, ResourcePackClientResponse, ResourcePackChunkRequest)이 순서를 어기거나 디코드에 실패하면 끊는다. Login 전에는 Login이 아닌 패킷도 끊는다.
- 그 밖의 패킷은 Login 뒤 Join 전에는 대기 큐에 두었다가 Join 뒤 순서대로 틱에 넘긴다(ClientCacheStatus는 받고 버린다).
- 스폰 뒤 패킷 하나의 디코드 실패는 그 패킷만 버리고 warn으로 남긴다. 버린 패킷도 입력 예산에서 뺀다.
- 클라이언트가 프레임마다 되풀이하는 패킷(누르고 있는 블록 우클릭)은 패킷 수 예산에 세지 않는다. 사본만 버리고, 그래서 틱 결과가 달라질 수 있는 경우를 `.claude/docs/network.md`에 적는다.
- 정상 종료 때 아직 Join하지 않은 세션과 틱이 처리하지 않은 Join의 세션에도 세션 층이 Disconnect(`shutdown-message`)를 보낸다.
- 같은 플레이어의 두 번째 접속은 새 접속의 이름을 바꿔 받지 않고 기존 플레이어를 끊은 뒤 받으며(이벤트로 취소 가능), 로그인 직후가 아니라 틱의 Join에서 판정한다.

## 로그인과 인증
- 로그인과 인증은 gophertunnel `minecraft/protocol/login`, `minecraft/service`를 정답으로 맞춘다.
- ClientData 내용은 신뢰하지 않는다.
- JWT 시각 검사의 벽시계는 `clock.rs`에서 받는다.
- 모르는 `kid`에는 JWKS만, 최소 5분 간격으로 다시 받는다.
- `data/auth.json`이 손상(JSON 아님, 필드 빠짐)이면 시작을 거부하고 파일을 남긴다.
- `data/identity.key`와 `data/auth.json`은 임시 파일에 쓰고 동기화한 뒤 rename하고 폴더를 flush한다(`fs::write_atomic`). 남은 임시 파일은 지우고 새로 만든다. `identity.key`는 Unix에서 소유자만 읽는 0600으로 쓴다.
- 온라인 로그인 시험은 자체 RSA 키로 OpenID 설정과 JWKS를 만든 시험용 발급자를 `auth.json`에 넣어 검증한다. 엔진에 시험 전용 진입점을 두지 않는다.

## NetherNet
- NetherNet은 고정한 df-mc/go-nethernet(버전은 `.claude/docs/protocol.md`)을 정답으로, Mojang NetherNet 온보딩 가이드를 보조 근거로 맞춘다.
- 시그널링(`nethernet/signaling.rs`)은 라우팅과 상한만 하고 오퍼 내용을 해석하지 않는다.
- 시그널링 요청에 `Content-Length`와 `chunked`가 함께 오면 400으로 거부한다.
- `nethernet-tls-cert`, `nethernet-tls-key`의 PEM이 틀리면 시작을 거부한다.
- 신원 없는 오퍼는 온라인 모드에서 거부(37)하고 오프라인 모드에서 받는다.
- Login의 클라이언트 키가 오퍼에서 증명된 `cpk`와 다르면 오프라인에서도 NotAuthenticated로 끊는다.
- 모든 SDP 응답에 `a=identity`를 넣는다.
- 서버 JWT의 `cpk`는 go-nethernet의 문자열이 아니라 JWK 객체로 쓴다.
- 서버 신원 키 `data/identity.key`는 재시작해도 유지한다. 없을 때만 만든다. 있는데 읽거나 파싱하지 못하면 새로 만들지 않고 시작을 거부하며 파일을 건드리지 않는다.
- DTLS 인증서는 시작 때 하나 만들어 모든 피어가 쓴다.
- str0m에는 원격 정보만 str0m SDP API로 넣고, 그 내부 오퍼의 클라이언트 setup은 `passive`로 적는다.
- 클라이언트에게 가는 응답 SDP는 `sdp.rs`가 공식 서버 모양으로 쓴다.
- 메시지는 비신뢰 채널로 보내지 않고, 클라이언트 첫 메시지 전에는 보내지 않는다.
- 피어 마감은 str0m에 맡기지 않고 피어가 직접 센다.
- str0m 계열의 패닉은 피어마다 `catch_unwind`로 잡아 그 피어만 닫는다.
- 구동부 작업 하나로 모자라면 받는 작업 하나가 분배하고 피어를 여러 작업에 나눈다.
- 피어 비용은 `#[ignore]` 시험 `nethernet::peer::tests::driver_cost_per_peer`, `handshake_under_random_loss`로 다시 잰다.

## RakNet
- RakNet은 서버 쪽만 직접 구현한다.
- sans-IO 핵심(`raknet/wire.rs`, `raknet/peer.rs`)과 tokio UDP 구동부를 나눈다.
- 0xfe 바이트는 RakNet 층이 떼고 붙인다. 세션은 전송과 무관하게 둔다.
- RakNet 시계는 단조 시계를 쓴다.
- 쿠키(security)는 쓰지 않는다.
- 조각 수만큼 미리 할당하지 않는다.
- `motd`와 `level-name`에 `;`와 제어 문자를 금지한다.

## 암호화
- 암호화는 gophertunnel `minecraft/conn.go`, `packet/encryption.go`를 정답으로 맞춘다.
- Anchor 코드의 암호 기본 요소(ES384, RS256, ECDH P-384, SHA-256, AES-256-CTR, 난수)와 base64, JWS 압축 형식은 `crypto` 모듈에 둔다.

## 압축
- `compression-threshold`, `compression-level`, `max-batch-bytes`는 `batch::Settings::new` 한 곳에서 검사한다.
- 압축 문턱 0은 받지 않는다.
- 서버 송신에도 NetworkSettings로 알린 문턱을 쓴다.
- Snappy는 표준 원소만 받고 S2 확장은 받지 않는다.
- 해제 상한 `max-batch-bytes`는 zlib, Snappy, 압축 안 함 세 방식 모두에, 로그인 전후 똑같이 건다. 로그인 전이라고 줄이지 않는다.
- Snappy는 할당 전에 선언 길이가 상한이나 입력의 22배를 넘는지 본다. zlib은 상한까지만 자라는 버퍼로 푼다.
- 방송과 모든 플레이어에게 같은 스폰 데이터(ItemRegistry, CreativeContent, CraftingData)는 한 번만 인코드·압축해 나눠 쓴다. 공유 배치는 `run()` 시작 때 만든다.
- Snappy의 AVX2 없는 기기 크래시 보고는 `Compression` rustdoc에 적는다.

## 확인할 수 없는 것
- 공식 클라이언트에서만 보이는 것은 모든 경우를 받는 쪽으로 설계하고, `.claude/docs/network.md`의 남은 위험에 적는다.

배경과 자세한 사실은 `.claude/docs/network.md`.
