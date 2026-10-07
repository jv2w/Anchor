# 프로토콜

작업 규칙은 `.claude/rules/protocol.md`.

이 문서는 정답 출처와 고정 버전, IR에서 생성 코드까지의 파이프라인, 디코드 상한, gophertunnel 결함, 시험 P1~P5를 적는다. 전송, 로그인, 암호화, 압축은 `network.md`, 공식 서버 대조(P6)는 `testing.md`, 블록과 아이템 데이터는 `data.md`, 외부 불신 절차와 의존성 검토는 `dependencies.md`에 있다.

## 한 번에 한 버전
- 지원 버전은 프로토콜 2193, 게임 버전 1.26.50이다(`protocol::packet::PROTOCOL_VERSION`, `GAME_VERSION`). 인증 디스커버리는 `GAME_VERSION`으로 주소를 만든다.
- 한 버전만 받는 이유: 여러 버전을 함께 받으면 코덱, 블록 표, 시험이 버전 수만큼 늘어나 일관성과 단순함을 잃는다.
- 다른 번호의 클라이언트는 RequestNetworkSettings(또는 Login)에서 PlayStatus(클라이언트가 높으면 서버 구버전, 낮으면 클라이언트 구버전)를 받고 끊긴다.

## 정답 출처와 고정 버전
- IR(`xtask/data/protocol.json`)의 `source`는 `xtask/go/go.mod`의 의사 버전과 같은 값을 적는다.

| 모듈 | 고정 버전 |
|---|---|
| github.com/sandertv/gophertunnel | `v1.62.1-0.20260928233358-80c811b61860`(커밋 `80c811b6186016b3860c358368cfa47e507f26e9`, v1.62.0 뒤 19커밋, 프로토콜 2193, 1.26.50) |
| github.com/sandertv/go-raknet | `v1.15.2-0.20260705184311-0d1fd09e2cf6` |
| github.com/df-mc/go-nethernet | `v1.0.20-0.20260818142457-6fc1eb6f907c` |
| Mojang bedrock-protocol-docs(필드 상한) | 커밋 `475bd72ed89036af4eb18426774ef3b953de7603`(v1.26.50 릴리스, 프로토콜 2193) |

- 지금 고정한 커밋은 v1.62.0 뒤의 형식 수정 ClientBoundDebugRenderer(`98c3d1f`), NBT 고정 길이 배열 검사(`398154f`), 문자열 `ReadFull`(`a705cae`)을 포함한다.
- 기준 서버는 같은 프로토콜의 공식 서버 1.26.52.3이다. 공식 서버의 빌드 번호는 1.26.52이고 Anchor는 프로토콜 출처의 1.26.50을 알리지만 둘 다 2193이다.
- gophertunnel도 형식 버그를 자주 고친다. P3 통과는 "gophertunnel과 같다"는 뜻일 뿐이다.

## 파이프라인: Go 추출 → IR → Rust 생성

```
gophertunnel 소스 ──(xtask/go/extract)──> xtask/data/protocol.json ──(cargo xtask gen)──> src/protocol/{types,packet,consts}.rs
Mojang 스키마 ──(cargo xtask dump)──> xtask/data/reference/protocol_limits.json ──┘
```

### 추출기 (`xtask/go/extract`)
- `golang.org/x/tools/go/packages`로 타입 정보까지 읽고, 각 패킷과 타입의 `Marshal(io protocol.IO)`를 걸어 IR을 낸다.
- 모르는 구문에서 실패하는 이유: 조용히 건너뛴 필드는 틀린 코덱이 된다.

### IR (`xtask/data/protocol.json`)
- `source`: 모듈, 의사 버전, 커밋, 프로토콜 번호, 게임 버전.
- `packets`: ID, 이름, 방향(gophertunnel `pool.go`의 클라이언트·서버 풀). 236개.
- `types`: 필드 이름, Go 타입, Marshal 문장 목록. 문장은 IO 호출, 중첩 Marshal, 목록(길이 형식 포함), Optional, if, switch, 지역 변수, 대입, 오류(`InvalidValue`, `UnknownEnumOption`)뿐이다.
- `io_methods`(`protocol.IO`의 메서드), `functions`(IO를 받는 자유 함수), `unions`(`lookup*` 함수의 ID→타입 표), `enums`(문자열↔값 변환 함수 표), `constants`, `const_decls`(protocol·packet 패키지의 export 상수).

### 생성기 (`cargo xtask gen`, `xtask/src/gen*`)
- `gen`은 Go 추출 → IR → Rust 생성을 한 번에 한다. 출력은 `src/protocol/types.rs`(gophertunnel `protocol` 패키지 타입), `src/protocol/packet.rs`(패킷, `Packet` enum, `PROTOCOL_VERSION`), `src/protocol/consts.rs`(IR 상수에서 만든 enum과 상수)다. 같은 명령이 블록, 아이템, 레시피 표도 만든다(`data.md`).
- `cargo xtask gen --check`는 `target/`에 다시 만들어 커밋된 IR과 생성 파일을 바이트로 비교한다. `cargo xtask verify`가 맨 앞에서 이것을 돈다. Go가 PATH에 없으면 이것과 시험 P1~P5가 실패한다.
- 이름 변환: 용어 표의 이름(`Http`, `Xuid`, `Uuid` 등)을 먼저 한 단어로 묶고, 나머지는 대소문자 경계로 나눈다(`HTTP|Server`). 숫자는 앞 단어에 붙인다. 모듈 이름과 겹치는 앞뒤는 뗀다(`packet::ViolationWarning`). Rust 키워드인 필드는 raw identifier(`r#type`)다.

### Rust 표현
- 생성 struct가 Go 구조체와 1:1이라 P3 판정과 버전업 비교가 단순하다.
- `Optional` → `Option`, 슬라이스 → `Vec`, 인터페이스 → enum, `mgl32.Vec3` → `[f32; 3]`, `uuid.UUID` → `Uuid`, NBT 컴파운드 → `BTreeMap<String, nbt::Value>`(바이트 순으로 쓰고 같은 키는 뒤가 이긴다: Go map과 같다), EntityMetadata → `BTreeMap<u32, _>`(gophertunnel도 키를 정렬해 쓴다).
- 문자열은 `String`이다. gophertunnel은 UTF-8을 검사하지 않는다.
- NBT 목록이 빈 목록에도 원소 타입을 지니므로 모든 목록이 같은 바이트로 다시 쓰인다(gophertunnel은 빈 Int·Long 목록만 타입을 지키고 나머지는 End로 다시 쓴다). NBT 중첩이 힙 스택에 있어 깊이와 무관하게 스레드 스택이 평평하다.
- `anchor::protocol`을 공개하는 이유: 패킷 훅이 패킷 값을 다루고 xtask 시험이 이 층을 부른다. NBT는 저장에서도 쓴다.

### 손으로 쓰는 코덱
`protocol.IO`의 원시형이 아닌 메서드는 Marshal로 표현되지 않아 손으로 쓴다: 벡터·위치, UUID, 색, NBT(`src/nbt.rs`), EntityMetadata, ItemStack 계열, StackRequestAction 등 union의 태그 틀(`union_frame.rs`), GameRule, AbilityValue, Bitset, PackSetting, ShapeData, MaterialReducer, 이벤트 틀, 거래 데이터 틀, Login 요청 봉투(`protocol/login.rs`), 서브청크 페이로드(`block_storage.rs`).

### 배치 형식 (`protocol/batch.rs`)
- 패킷 머리 varuint32: ID는 0~9비트, 보낸 쪽 하위 클라이언트 10~11, 받는 쪽 12~13. 그보다 높은 비트는 읽을 때 무시한다(gophertunnel과 같고, 공식 클라이언트 출력은 관찰할 수 없어 받는 쪽을 넓힌다).
- 배치당 패킷은 812개 이하(gophertunnel 값)이고 공식 서버는 최대 45개를 보낸다. 출력은 256 KiB 전과 812개에서 배치를 끊는다. 압축 바이트와 해제 상한은 `network.md`에 있다.

## 디코드 상한
- 선언 길이는 남은 입력 이하. 목록 개수는 1024 이하(gophertunnel `maxSliceLength`, EntityMetadata 개수 포함)이고 남은 입력 이하.
- 할당 예산: Reader는 배치 하나마다 2 × `max-batch-bytes` 예산을 받는다. 개수 상한만으로는 중첩 목록과 큰 원소가 입력 크기의 수십 배를 할당할 수 있어서 예산을 둔다.
- NBT는 깊이 512(루트 포함), 루트 하나 4 MiB 이하.
- 목록 중첩은 64단 이하(패킷 자신의 목록 포함, `MAX_LIST_DEPTH`). 자기를 담는 타입(`DataStorePropertyValue`)의 디코더는 목록마다 스레드 스택에서 재귀하므로, 상한이 없으면 패킷 하나가 스택을 넘쳐 프로세스가 끝난다.
- 필드별 상한:
  - `cargo xtask dump`가 고정 커밋의 스키마를 읽고, 스키마 항목을 gophertunnel 필드에 잇는 표(`xtask/src/dump/limits.rs`)로 `xtask/data/reference/protocol_limits.json`(필드 113개)을 쓴다. 문자열과 바이트는 바이트 수, 목록은 원소 수다. `[]`가 붙은 항목은 목록의 각 원소를 제한한다.
  - 스키마 상한이 표에 없거나 표 행이 스키마에 없으면 dump가 실패하고, 어떤 코덱에도 쓰이지 않는 상한은 gen이 실패한다. 그래서 버전업 때 생기거나 사라진 상한이 모두 드러난다.
  - 디코드의 목록 상한은 min(필드 상한, 1024)이다. 인코더는 필드 상한으로 오류를 내므로 Anchor가 스키마를 넘는 패킷을 보내지 않는다. 1024는 받는 쪽 상한이라 인코드에는 걸지 않는다(서버는 ItemRegistry, CreativeContent처럼 1024개를 넘는 목록을 보낸다).
- 서브청크 페이로드 해제는 엄격하다: 비트 수 {0,1,2,3,4,5,6,8,16}만, 지속(persistent) 팔레트 플래그와 0x7f 거부, 팔레트 길이 ≤ 2^bits, 모든 인덱스 < 팔레트 길이.

## gophertunnel 결함
고정한 gophertunnel에서 찾은 결함이다.
- MaterialReducer: gophertunnel Reader가 `mix << 16`으로 읽는다(`>> 16`이 맞다). 코퍼스는 mix 0~0x7fff만 쓴다.
- Event 타입 8, 9, 10, 12, 14, 20, 21, 22는 ordinal이 없어 읽기도 쓰기도 안 되고, ordinal 21을 세 이벤트가 나눠 쓴다.
- 쓸 때 덮어쓰거나 읽고 버리는 값: ItemDescriptorCount aux 32767, StackRequestAction legacy 바이트, Skin `trusted`, 몇 패킷의 이름 문자열, InventoryTransaction `hasLegacy`.
- EntityMetadata 개수 상한 없음, NBT 깊이 계수 누수, 부분 읽기 허용, ItemInstance·Login 길이 조용히 자름.
- Snappy 해제가 `s2.Decode`라 S2 확장도 받는다.
- 로그인 마감 없음, 지연 패킷 큐 상한 없음, 0xff 원문 상한 없음: Anchor 세션에는 모두 있다(`network.md`).

## 시험 P1~P5
P6(공식 서버 대조)는 `testing.md`에 있다.

- Go 오라클(`xtask/go/oracle`):
  - `corpus`: 무작위 값을 고르며 쓰는 `protocol.IO` 구현으로 각 패킷의 Marshal을 돌려 표본을 만든다(시드 고정 PCG, 목록 길이 0~4, `-limits`로 필드 상한 안 표본만). 평범한 gophertunnel Writer로 쓰고 패킷 최상위 NBT만 자체 정렬 인코더로 쓴다(Go map 순서가 무작위라서). gophertunnel Reader로 다시 읽어 성공한 표본만 남긴다. 코퍼스는 시험 때 `target/`에 만든다.
  - `check`: 받은 바이트를 gophertunnel로 디코드하고 다시 인코딩해 돌려준다. 바이트 비교는 Rust가 한다.
  - `compress`, `decompress`: gophertunnel `packet` 패키지의 zlib과 Snappy.
  - shield 아이템 ID는 양쪽 모두 고정값을 쓴다.
- P1 디코드 완결(`xtask/tests/protocol.rs`): 모든 패킷마다 코퍼스 표본 64개, 모든 표본에서 Rust 디코드가 성공하고 남는 바이트가 0이다. IR 패킷 수 = gophertunnel 두 풀의 합집합 = `Packet` variant 수(236)다.
- P2 왕복: 모든 표본 b에서 `encode(decode(b)) == b`. P4에서 디코드에 성공한 변형 m마다 `e = encode(decode(m))`이면 `encode(decode(e)) == e`(바이트 비교라 NaN에도 맞다).
- P3 gophertunnel과 양방향 일치: (가) Go가 만든 바이트를 Rust가 다시 인코딩하면 같다. (나) 변형의 Rust 인코딩 R → Go 디코드 → Go 재인코딩 → Rust 디코드 → Rust 재인코딩 == R. 차이는 빈 NBT 목록의 원소 타입 하나만 값 구조 비교로 허용한다. 배치 압축(`xtask/tests/compression.rs`)은 zlib과 Snappy 각각 Rust 압축 → Go 해제, Go 압축 → Rust 해제가 원문과 같다(크기 0, 1, 255, 256, 64 KiB, 1 MiB, 무작위와 반복 데이터).
- P4 변형 입력: 패킷마다 고정 시드 변형 1000개(비트 뒤집기, 바이트 넣기·빼기, 자르기, 길이 필드 키우기)에서 패닉 0. Snappy 결함 입력 5종과 zlib 폭탄은 오류다. 외부 입력을 읽는 다른 디코더(RakNet 데이터그램, 시그널링 요청, SDP, 피어)도 고정 시드 변형 1만 개에서 패닉 0을 시험한다.
- P5 봇 접속(`xtask/tests/spawn.rs`): gophertunnel `packet`, `login`과 go-raknet, go-nethernet으로 만든 Go 봇(`xtask/go/bot`)이 같은 프로세스에서 띄운 Anchor에 RakNet과 NetherNet, zlib과 Snappy 각각으로 스폰한다. 큰 스킨(해제 6 MiB) Login, 두 전송 동시 스폰, NetherNet 실패 때 RakNet 폴백, 로그인 마감과 로그인 전 상한, 신원 결합 불일치, 중복 로그인 두 경우를 본다. 시험용 발급자는 자체 키로 만든 OpenID 설정, JWKS, 토큰이다. `xtask/tests/go.rs`는 Go 도구 자체의 시험을 돈다.
