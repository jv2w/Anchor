# 네트워크

작업 규칙은 `.claude/rules/network.md`.

패킷 형식과 코덱은 `protocol.md`, 설정 키 전체는 `config.md`, 의존성 검토와 vendor 사본은 `dependencies.md`, 봇과 공식 서버 대조는 `testing.md`에 있다.

## 포트와 소켓

| 포트(설정 키, 기본값은 `config.md`) | 용도 |
|---|---|
| UDP `server-port` | RakNet, 서버 목록 핑 |
| TCP `server-port` | NetherNet HTTP 시그널링 |
| UDP `nethernet-port` | NetherNet 게임 데이터(모든 피어가 소켓 하나) |

- IPv4 주소 키는 `server-ip`, IPv6는 `server-ipv6`, `enable-ipv6`다.
- Windows UDP에서 WSAECONNRESET을 무시하지 않으면 상대 하나의 ICMP 응답이 소켓 전체 읽기를 멈춘다(tokio-rs/tokio#2017).
- IP별 상한의 묶음 단위는 IPv4 주소 하나, IPv6 /64다.
- 클라이언트의 주소 접속 순서: `GET /v1/join`을 `https://호스트:포트`, `http://호스트:포트` 순으로 보내 처음 답한 곳을 쓰고, 모두 실패하면 RakNet으로 간다(go-nethernet `endpoint/handler.go`). 서버 목록 정보도 이 GET의 JSON이다. UDP만으로 NetherNet에 닿는 길은 LAN 발견(UDP 7551)뿐이고 Anchor는 이를 구현하지 않는다(TCP 시그널링을 유지하는 이유).
- 공식 서버는 연결마다 게임 UDP 소켓을 따로 열고 포트를 하나씩 올린다. 소켓 하나를 나눠 쓰면 방화벽 규칙이 포트 하나로 끝나고, 한 작업이 모든 피어를 잠금 없이 다룬다.

## NetherNet

응답 형식은 공식 서버 1.26.52.3을 `transport=nethernet`으로 띄워 관찰한 값과 바이트 구조가 같다.

### 시그널링 (`nethernet/signaling.rs`)
- 최소 HTTP/1.1 서버다. 연결 시도마다 요청 하나다(trickle ICE 없음).
- `GET /v1/join`: 200, `Content-Type: application/json`, 본문 키 순서 `name, protocol, version, level, players, maxPlayers, gameType`.
- `POST /v1/join/{networkId}`: 본문은 SDP 오퍼, 답은 SDP 응답(`application/sdp`). 협상 실패도 공식 서버처럼 HTTP 200이고 본문은 오류 코드 숫자다: 신원 실패 `37`, SDP 실패 `13`, 내부 실패(광고할 소켓 없음 등) `11`.
- 없는 경로 404 `Not Found`, 숫자가 아닌 networkId(20자리 초과 포함) 400, 로그인 전 상한 초과 503. 모든 응답에 `Connection: close`이고 응답 뒤 항상 닫는다.
- 메서드는 GET, POST만이다. 본문은 `Content-Length`와 `chunked`를 받는다. 둘이 함께 온 요청을 거부하는 이유는 요청 밀반입 방지다.
- 상한: 동시 연결 64, IP별 4, 머리 8 KiB(넘으면 431), 본문 64 KiB(넘으면 413), chunked 틀 8 KiB, 요청 하나 10초(읽기, 협상, 쓰기 포함).
- 신뢰: 기본은 평문 HTTP이고 클라이언트는 처음 보는 서버 키에 확인 창을 띄운다(TOFU). `nethernet-tls-cert`, `nethernet-tls-key`(PEM, 1 MiB 이하)를 설정하면 HTTPS다(tokio-rustls, aws-lc-rs 공급자). TLS가 없을 때 첫 바이트가 0x16(TLS ClientHello)이면 공식 서버처럼 handshake_failure 경고를 보내고 닫는다: 클라이언트가 https 시도를 빨리 접고 http로 넘어간다.

### SDP (`nethernet/sdp.rs`)
- 오퍼 상한과 규칙: 본문 64 KiB, 줄 1024개, 후보 32개(UDP만 쓰고 TCP 후보는 버림), 미디어 섹션 정확히 하나(`application`, `UDP/DTLS/SCTP`, `webrtc-datachannel`), `ice-ufrag` 4~256자, `ice-pwd` 22~256자, 지문은 sha-256 한 줄(32바이트), `setup`은 actpass나 passive, `max-message-size` 필수이고 1024 이상, `sctp-port` 5000. go-nethernet은 후보 줄을 `candidate:candidate:...`로 두 번 접두해 보내므로 둘 다 받는다.
- 응답 줄 순서는 공식 서버와 같다: `v, o, s, t, a=group:BUNDLE 0, a=extmap-allow-mixed, a=msid-semantic: WMS, a=identity, m=application <포트> UDP/DTLS/SCTP webrtc-datachannel, c, a=candidate(host), a=ice-ufrag(4자), a=ice-pwd(24자), a=ice-options:trickle, a=fingerprint:sha-256 <대문자 HEX>, a=setup:active, a=mid:0, a=sctp-port:5000, a=max-message-size:262144`. `a=end-of-candidates`는 없다. 후보 우선순위는 2122260223(공식 서버 값)이다.
- 광고 주소: `public-ip`가 있으면 그것, 없으면 클라이언트가 실제로 닿은 시그널링 TCP 연결의 로컬 주소다(표준 라이브러리에 인터페이스 나열이 없고, 이 주소는 클라이언트가 닿는 것이 확실하다). IPv4 매핑 IPv6는 IPv4로 바꾼다. 포트는 그 주소 체계의 `nethernet-port` 소켓이고, 그 소켓이 없으면 코드 11이다.

### 신원 (`nethernet/identity.rs`)
- 오퍼의 `a=identity`: GameServerToken과 오퍼 지문에 대한 분리 JWS(ES384, 토큰의 `cpk`로 검증)다. 토큰은 온라인이면 Login 토큰과 같은 규칙(아래 로그인)으로 검증하고, 오프라인이면 서명 없이 `alg`(ES384나 RS256만), `cpk`, 있는 시각 클레임(`exp`, `nbf`, `iat`)만 본다. `cpk`는 base64 SPKI 문자열과 P-384 JWK 객체를 모두 받는다. 신원 JSON 32 KiB 상한, 토큰은 Login이 받는 가장 긴 토큰 길이(본문 상한의 base64와 머리·서명 상한) 상한. 실패는 코드 37.
- 신원이 없는 오퍼: 공식 서버는 오프라인에서도 37로 거부한다. Anchor가 오프라인에서 받는 이유는 사용자 정의 클라이언트다.
- 응답의 `a=identity`: 없으면 클라이언트가 응답을 거부한다. 세션 수준(`m=` 앞)이고 값은 base64(STD, 패딩) JSON `{"assertion":"{\"fingerprints\":\"<분리 JWS>\",\"token\":\"<JWT>\"}","idp":{"domain":"self","protocol":"default"}}`다(키 순서 그대로).
  - 서버 JWT: 헤더 `{"alg":"ES384","x5u":"<base64 SPKI>"}`, 본문 `{"cpk":{JWK},"exp":iat+60,"iat":..}`. 공식 서버와 Mojang 가이드도 `cpk`를 JWK 객체로 쓴다. 공식 서버처럼 응답마다 새로 발급하고 60초 유효하다.
  - 분리 JWS: 헤더 `{"alg":"ES384"}`, 서명 대상 `{"fingerprint":[{"algorithm":"sha-256","digest":"<SDP에 쓴 그대로>"}]}`.
- 서버 신원 키 `data/identity.key`: P-384 PKCS#8 DER(4 KiB 이하만 읽음). 시작 때 블로킹 풀에서 한 번 읽는다. 키가 바뀌면 모든 플레이어에게 확인 창이 다시 뜬다.
- 신원 결합: 신원 없는 오프라인 오퍼는 결합할 것이 없다. 공식 서버는 오프라인에서 Login 키와 `cpk`의 불일치를 받는다. Anchor가 끊는 이유: 시그널링에서 증명한 신원과 게임 안 신원이 같아야 한다.

### WebRTC 피어 (`nethernet.rs`, `nethernet/peer.rs`, `nethernet/message.rs`)
- 구동부: 구조는 RakNet과 같다. 들어온 데이터그램은 STUN USERNAME의 서버 ufrag → 피어, 성립 뒤 출발 주소 → 피어 색인(`BTreeMap`)으로 나눈다. 오퍼는 별도 작업이 파싱하고 검증한 뒤 구동부에 피어 생성을 요청한다.
- 피어마다 str0m `Rtc` 하나(소켓 없음)다. 서버는 ICE controlled, DTLS client(`setup:active`), SCTP client로 공식 서버와 역할이 같다. DTLS 인증서를 하나만 두면 피어마다 키 생성 비용이 없고 지연 초기화도 없다. ICE ufrag 4자, pwd 24자는 OS 난수(`crypto`)로 만들고, ufrag는 모두 세 번 뽑아 겹치지 않는 것을 쓴다(셋 다 겹치면 코드 11).
- 채널: 클라이언트가 `ReliableDataChannel`(순서, 신뢰)과 `UnreliableDataChannel`(비순서, maxRetransmits 0)을 연다. 다른 라벨이나 매개변수의 채널, 세 번째 채널이면 끊는다. 비신뢰 채널은 262,144바이트 이하 메시지를 읽고 버리며 trace로 센다(내용 형식은 공식 클라이언트만 알아 어느 경우든 받는다). 신뢰 채널 메시지가 비신뢰 채널 열림보다 먼저 오면(go-nethernet은 열림 확인을 기다리지 않는다) 메시지 하나 크기까지 모았다가 열린 뒤 넘긴다.
- 조각: 메시지 앞 1바이트가 남은 조각 수, 최대 256조각. 보내는 조각 본문은 공식 서버처럼 262,143바이트다. 받는 쪽은 65,535바이트 조각과 262,143바이트 조각을 모두 받는다. 조각 수 어긋남, 빈 조각, 재조립 상한 초과는 끊는다. 재조립 상한은 아래 메시지 상한과 같다(0xfe와 체크섬은 없다).
- str0m의 직접 API는 원격 `max-message-size`를 받지 않아 원격 정보는 SDP API로 넣는다. 그 내부 오퍼의 클라이언트 setup이 `actpass`면 str0m이 DTLS server를 골라 공식 서버와 역할이 달라진다.
- 마감: 응답부터 두 채널이 열릴 때까지 10초, 열린 뒤 무응답 10초. str0m ICE는 끊김을 알리기만 하고 SCTP는 끝없이 재전송한다.
- 닫기: 닫힌 피어는 보낸 것이 모두 SCTP ACK를 받거나 5초가 지날 때까지 남는다. 그래서 마지막 Disconnect가 클라이언트에 닿는다.
- 상한: 로그인 전 피어 64, IP별 4(RakNet과 따로 센다), 피어당 출발 주소 8개, 데이터그램 1,500바이트, 받는 큐 1024개와 2 × 메시지 바이트, 보내지 못한 바이트 32 MiB, SCTP 송신 버퍼 1 MiB, 명령 큐 4096, 오퍼 큐 64, 갱신 10ms.
- 핑: NetherNet은 SCTP RTT를 밖에서 얻을 수 없어 ICE consent RTT를 쓴다.
- 격리: str0m 계열에는 패닉 가능성이 남아 있다. 피어 단위 격리는 연결 작업 패닉은 그 연결만 끊는다는 규칙을 한 작업 안의 피어에 적용한 것이다.
- 비용: 실측으로 피어 송신 3.55 ms/MiB, 수신 2.91 ms/MiB, 플레이어 한 명(50 KB/s 내림, 5 KB/s 올림) 초당 약 0.53 ms, 접속 한 번(2 MiB) 약 16 ms다. 작업 하나로 약 500명까지 충분하다.
- NetherNet에는 Minecraft 암호화와 배치 헤더 0xfe가 없다(DTLS가 암호화한다). 압축 바이트는 그대로 있다.

## RakNet (`raknet.rs`, `raknet/`)
- sans-IO 핵심은 메시지 형식, 신뢰성, 순서, 조각, ACK/NAK, 혼잡 제어를 맡는다.
- RakNet 프로토콜 11만 받고 다르면 IncompatibleProtocolVersion이다. MTU는 min(요청 크기, `max-mtu-size`)이고 400~1492다(공식 서버는 1492 요청에 1400으로 답한다).
- 쿠키(security)를 쓰지 않는 이유: 공식 서버가 security=false로 답하고, 공식 클라이언트의 쿠키 처리는 관찰할 수 없다. 위조 출발지는 로그인 전 상한으로만 막는다.
- 상수(정본은 `src/raknet/`): 조각 수는 ⌈메시지 / 최소 조각⌉, 동시 조각 묶음 16, 순서 창 2048(채널 32개 따로), 데이터그램 기록 1024, 신뢰 인덱스 빈칸 8192, 혼잡 창 32(최소 4, 최대 2048, 손실 때 ×3/4), RTO 500ms(100ms~2s), ACK 지연 100ms, flush당 데이터그램 32, 출력 32 MiB, 입력 큐 1024개와 2 × 메시지 바이트, 소켓 버퍼 4 MiB, 갱신 10ms, 핸드셰이크 10초, 무응답 5초 + 2·RTT, 닫기 5초, 핑 응답 데이터 1429바이트 이하. 메시지 하나는 아래 재조립 식 + 9바이트(0xfe, 체크섬 8)다.
- 로그인 전 연결 64, IP별 4. 넘으면 0x14(NoFreeIncomingConnections).
- 핑: IP별 토큰 버킷(초당 5, 순간 10). IP 표는 4,096개이고 1분 쓰지 않은 항목은 지우며, 가득 차면 새 IP의 핑에 답하지 않는다. 응답은 공식 서버 형식 `MCPE;motd;<프로토콜>;<게임 버전>;접속자;최대;guid;level;모드;1;포트;포트;0;1;1;`(번호와 판은 `protocol.md`)다. 응답이 `;`로 나뉘어 `motd`와 `level-name`의 `;`와 제어 문자는 응답을 깬다.
- 26.60은 RakNet을 "deprecated"로만 표시했고(제거 아님), 공식 서버 1.26.52는 RakNet 전송에서 "플레이어는 NetherNet 없이 접속할 수 없다"고 기록한다. 공식 클라이언트가 RakNet으로 들어오는지는 확인되지 않았다. 어느 경우든 RakNet은 폴백과 사용자 정의 클라이언트에 필요하다.

## 세션 (`session.rs`, `session/`)
- 세션은 `Transport` enum(RakNet, NetherNet)으로 두 전송을 같은 흐름에 태운다.
- 순서: RequestNetworkSettings → NetworkSettings(압축) → Login 검증 → ServerToClientHandshake와 (RakNet만) 암호화 시작 → ClientToServerHandshake → PlayStatus(LoginSuccess)와 ResourcePacksInfo(공식 서버처럼 한 배치에 이 순서) → 응답(HaveAllPacks) → ResourcePackStack → 응답(Completed) → 틱 Join → StartGame 계열 → RequestChunkRadius → ChunkRadiusUpdated, 청크, PlayStatus(PlayerSpawn) → SetLocalPlayerAsInitialised. NetherNet도 ServerToClientHandshake를 보내지만 암호화는 켜지 않는다(공식 서버와 같다).
- 프로토콜 번호가 다르면(RequestNetworkSettings나 Login) 클라이언트가 높으면 PlayStatus 서버 구버전, 낮으면 클라이언트 구버전을 보내고 끊는다.
- Join 전 대기 큐는 32개이고 넘치면 끊는다. Join 전 패킷을 미루는 이유: gophertunnel도 미뤄 두고, 공식 클라이언트 순서는 관찰할 수 없다. ClientCacheStatus는 받고 버린다.
- 스폰 뒤 디코드 실패를 그 패킷만 버리는 이유: 공식 클라이언트 출력의 모든 경우를 관찰할 수 없어 받는 쪽을 넓힌다.
- 마감: 연결부터 Login 검증까지 10초, 스폰 완료까지 60초. 리소스팩을 요청하면 30초 + 조각마다 2초씩 늘린다(최저 약 64 KiB/s). 팩 조각은 128 KiB, 200ms 간격이다.
- 연결 상한: 로그인 전은 전송마다 64, IP별 4(합은 128). 로그인 검증을 마치면 그 자리를 돌려주고, "로그인했고 아직 Join하지 않은" 세션은 시작 때 `max-players` 크기의 세마포어로 센다(모자라면 PlayStatus 서버 가득 참). Join 뒤는 틱이 `max-players`로 판정한다.
- 입력 예산: 세션마다 해제 바이트 토큰 버킷(용량 `max-batch-bytes`, 초당 `max-batch-bytes`/4). 출력: 보내지 못한 바이트가 32 MiB를 넘으면 끊는다.
- 종료 유예는 10초다.
- 같은 플레이어의 두 번째 접속: 같은 플레이어는 온라인이면 같은 XUID, 오프라인이면 대소문자를 무시한 같은 이름이다. 틱의 Join에서 `PlayerDuplicateLogin`을 쏘고, 취소되지 않으면 기존 플레이어를 LoggedInOtherLocation("Logged in from other location")으로 끊고 새 플레이어를 받는다. 취소되면 새 접속을 같은 문구로 끊는다. 판정을 Join에서 하는 이유: 게임 상태와 사용자 콜백은 틱에만 있고, 로그인에서 틱을 기다리면 세션과 틱 사이 왕복이 생긴다. 공식 서버는 같은 이름을 `이름(2)`로 바꿔 받는다.

## 로그인과 인증 (`login.rs`, `auth.rs`, `auth/`)
- 온라인(`xbox-auth = true`, 기본): Login의 `Token`(OIDC 멀티플레이 토큰)만 본다(레거시 체인은 무시). RS256만, `kid`와 `exp` 필수, `nbf`와 `iat`는 있으면 검사, 시계 오차 60초. `iss`는 OpenID 설정의 `issuer`와 같아야 하고 `aud`에 `api://auth-minecraft-services/multiplayer`가 있어야 한다. `xid`는 숫자, `xname`은 비어 있지 않고, `cpk`는 P-384 SPKI(base64)다. 온라인 이름은 검사하지 않는다.
- 오프라인(`xbox-auth = false`): `Token`이 있으면 서명 없이 클레임만 읽고, 없으면 레거시 체인(첫 토큰 자체 서명 `x5u`)을 읽는다. XUID는 비운다. 이름 규칙: 1~16바이트, `A-Z a-z 0-9 _ 공백`만, 소문자로 `rcon`, `console`이면 거부.
- ClientData: ES384, `cpk`로 서명만 검증한다. 내용은 gophertunnel `ClientData.Validate` 규칙으로 검사한다. JSON은 엄격하다(대소문자, null, 중복 키 거부).
- JWT 시각 검사는 틱 밖 일이라 벽시계를 쓴다.
- 상한: 체인 JSON 64 KiB, 토큰 16 KiB, 체인 8개, `aud` 16개, RSA 2048~4096비트, JWKS 키 64개.
- 인증 환경: 서비스 디스커버리(`https://client.discovery.minecraft-services.net/api/v1.0/discovery/MinecraftPE/builds/<게임 버전>`)의 `auth.prod`, OpenID 설정, JWKS를 받는다. 한 번 받기(세 요청 모두)는 10초 안에 끝나야 하고(DNS 조회는 OS 상한만), 응답 본문은 1 MiB 미만이다.
- 캐시 `data/auth.json`: OpenID 설정, JWKS, 받은 시각을 담는다(디스커버리는 OpenID 설정 주소를 찾을 때만 쓴다). 시작 때 캐시가 1시간 안이면 그대로 쓰고, 아니면 받는다. 받기에 실패하면 캐시로 돌고, 받지도 못하고 캐시도 없으면 시작을 거부한다. 실행 중 1시간마다 갱신한다. 모르는 `kid`의 재요청 간격은 위조 `kid`로 외부 요청을 늘리지 못하게 한다.

## 암호화 (`crypto.rs`, `crypto/`)
RakNet에만 있다.
- 서버 키는 실행마다 새 P-384 키 하나(서명과 ECDH에 같은 키)이고, salt는 연결마다 16바이트 난수다. 서버가 salt를 ServerToClientHandshake의 ES384 JWT로 보낸다.
- 키 = SHA-256(salt ‖ ECDH-P384 공유 비밀). AES-256-CTR, IV = 키[0..12] ‖ 00000002. 범위는 0xfe 뒤 전부다(gophertunnel 주석의 CFB8은 틀렸고 코드는 CTR이다).
- 체크섬 = SHA-256(LE64(카운터) ‖ 평문 ‖ 키)의 앞 8바이트, 송수신 카운터는 따로다. 틀리면 끊는다.
- `crypto` 모듈은 aws-lc-rs를 감싼다.
- `enable-encryption = false`면 RakNet 로그인에서 ServerToClientHandshake와 암호화를 하지 않고 LoginSuccess로 바로 간다. 평문이므로 신뢰하는 망에서만 쓴다. NetherNet은 이 설정과 무관하다.

## 압축 (`protocol/batch.rs`, `protocol/compression*`)
zlib과 Snappy를 둘 다 구현하고 `compression`으로 고른다. 기본은 Snappy(CPU가 적게 든다), zlib은 대역폭이 적게 든다.
- 설정 키는 `compression`, `compression-threshold`, `compression-level`(zlib), `max-batch-bytes`다(기본값과 범위는 `config.md`). 문턱 0은 출처마다 뜻이 다르다.
- 문턱 256은 gophertunnel과 Java판의 기본값과 같고, 측정에서 100바이트 안팎 배치는 압축하면 커졌다. 문턱은 NetworkSettings로 알린다.
- 형식: NetworkSettings 압축 ID 0 zlib, 1 Snappy. 배치 첫 바이트 0x00 zlib, 0x01 Snappy, 0xff 압축 안 함(문턱 미만). 협상한 ID와 0xff 말고는 끊는다. zlib은 헤더 없는 raw DEFLATE이고 마지막 블록 뒤 남는 바이트는 받고 버린다. Snappy는 블록 형식이다(오프셋 0, 쓴 길이를 넘는 오프셋, 선언 길이 초과, 남는 입력, 길이 불일치는 오류, copy-4는 받는다). gophertunnel과 달리 S2 확장은 받지 않는다(`protocol.md`).
- 해제 상한 16 MiB 근거: 정상 Login이 2 MiB를 넘는 일이 흔하고 다른 서버의 8~10 MiB 상한에서 실패 보고가 있으며, 공식 서버는 큰 스킨을 PlayerList로 돌려보내며 약 9 MB 배치를 낸다. Snappy의 22배 근거: 정상 스트림은 3바이트 복사당 최대 64바이트다.
- 로그인 전: Login이 가장 큰 정상 배치다. 대신 NetworkSettings 전 메시지는 1 KiB 이하, 첫 배치의 첫 패킷은 Login(나머지는 검증 뒤 처리), 인증 전 세션 전체의 재조립·해제 바이트 합은 8 × `max-batch-bytes` 이하다.
- 재조립 상한: 메시지 하나 `max-batch-bytes + max-batch-bytes/6 + 42`바이트(Snappy 최악 크기). RakNet 조각 수 상한은 이것을 조각 크기로 나눈 값이다.
- 출력 배치를 끊는 크기와 개수는 `protocol.md`의 배치 형식에 있다. 순서를 지킨다.
- 보내는 때(`send-release`, `session/release.rs`): 기본 `immediate`는 틱 출력을 받는 즉시 보낸다. `after-pass`는 공식 서버의 RakNet 갱신 스레드를 따른다. 그 스레드는 어느 연결이든 데이터그램이 오면, 아니면 `WaitOnEvent(10)`이 끝나면(Windows 기본 타이머라 10 ms 뒤 첫 15.625 ms 경계) 모든 연결의 큐를 한 번에 보낸다. 서버 루프는 패스의 입력이 적용된 뒤에야 그 패스의 출력을 큐에 넣으므로, 패스 끝 경계의 타임아웃은 그 패스를 아직 보내지 않는다. Anchor는 출력마다 패스 번호를 붙이고(틱 출력은 진행 중인 패스), 틱이 전달을 마친 패스 수를 `Release`에 알린다. RakNet과 NetherNet 드라이버는 데이터그램을 읽을 때마다(읽기 묶음당 한 번) 전달을 마친 모든 패스를 풀고, 타이머 작업은 마지막으로 푼 때부터 기다린 뒤 그 경계보다 먼저 끝난 패스까지 푼다. 세션은 풀린 패스까지의 메시지를 큐 순서대로 배치 하나로 보낸다(미리 인코드한 공용 배치만 따로). 잡아 두는 메시지는 출력 큐 크기(1024개)까지이고, 넘치면 출력 큐가 차서 틱이 그 세션을 끊는다. 틱이 세션을 닫으면 잡아 둔 것을 모두 보낸 뒤 마지막 패킷을 보낸다. 틱 밖의 일이라 상태 해시와 로그에 들지 않는다.
- Snappy 주의: gophertunnel 주석에 "AVX2 없는 기기에서 크래시"라는 확인되지 않은 보고가 있다(Mojira BDS-19188 정황). 공식 클라이언트에서만 보이는 일이라 확인하지 못했다. 서버 쪽 형식은 공식 서버 대조로 확인했다.

## 남은 위험과 확인하지 못한 것
공식 서버와 봇으로 닫을 수 없는, 공식 클라이언트에서만 보이는 것들이다.
- 공식 클라이언트가 RakNet으로 들어오는지.
- Snappy의 AVX2 없는 기기 크래시 보고.
- Join 전 대기 32개가 충분한지, ClientData의 엄격한 JSON이 공식 클라이언트 출력을 모두 받는지, 핑 응답에서 서바이벌 외 모드 뒤 숫자(공식 서버는 `Survival;1`만 보낸다).
- 비신뢰 데이터 채널의 내용 형식, 공식 클라이언트 NetherNet에서 ICE consent RTT가 들어오는지.
- 암호화 없는 RakNet 로그인(`enable-encryption = false`)을 공식 클라이언트가 받는지.
- Windows는 폴더를 flush하지 못한다(`storage.md`의 크래시). 처음 만든 `identity.key`가 전원 차단으로 사라지면 다음 시작에 키가 다시 생긴다.
- dimpl과 sctp-proto는 `log`로 기록하므로 `tracing`만 쓰는 바이너리에서는 그 기록이 보이지 않는다. 필요하면 사용자 바이너리가 다리를 둔다.
