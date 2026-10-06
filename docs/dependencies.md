# 외부 의존성

Anchor는 직접 구현이 매우 어려운 것만 들여온다. 들이는 모든 것은 아래 외부 불신 절차를 거치고, 결함이 남으면
고친 판 고정, 가드, 사본 수정 가운데 하나로 없앤 뒤에만 쓴다. 데이터 출처(PMMP, dragonfly, CloudburstMC, 업그레이드
스키마)는 `data.md`에 있다.

## 들이는 조건

다음을 모두 만족해야 한다.
1. 직접 구현이 매우 어렵다.
2. 매우 정확하고, 정답(표준, 기준 서버, 시험 벡터)과 시험으로 맞춰 볼 수 있다.
3. 숨은 스레드, 전역 상태, 상한 없는 큐, 외부 입력이 닿는 패닉이 없거나 막을 수 있다.
4. 열린 RustSec 권고가 없고, 유지보수가 살아 있으며, 라이선스가 허용형이다.
5. Bedrock 형식과 바이트 단위로 맞는다.
6. Windows와 Linux를 지원한다.

Anchor는 라이브러리라 전역 할당기, 빌드 프로필, 전역 로그 구독자를 정하지 않는다. 모든 의존성은 `=` 판으로
고정하고, 판을 올릴 때마다 이 절차를 다시 거친다.

## 외부 불신 절차

크레이트, gophertunnel, PMMP, dragonfly, 문서 등 밖에서 가져오는 모든 것에 적용한다.
1. 분석: 쓰는 부분의 코드를 직접 읽는다. 알려진 이슈와 권고를 찾는다. 경계 조건(패닉, 넘침, 할당, 숨은 스레드,
   결정성)을 시험한다. 핫패스에서 부르는 함수는 안을 열어 비용을 확인한다.
2. 목록: 찾은 결함을 적는다(이 문서의 표).
3. 제거: 고친 릴리스 → 고친 커밋 고정 → 가드 → 다른 선택 순으로 없앤다. 없앨 수 없으면 들이지 않는다.
4. 기록: 이 문서에 결함과 조치를 남긴다.

근거는 공식 문서, 코드, 실측만 쓴다. 출처끼리 다르면 기준 서버(BDS) 대조, 프로토콜 출처(gophertunnel), Mojang
문서 순으로 따른다.

가드의 기본 수단은 `.clippy.toml`의 `disallowed-methods`와 `disallowed-types`다. 들인 의존성의 시계, 난수, 상한
없는 큐, 블로킹 호출, 상한 없는 할당 진입점은 들이는 변경에서 금지 목록에 넣고, 임시 코드로 그 경로가 실제로
걸리는지 확인한다. 금지를 푸는 곳은 그 진입점을 실제로 부르는 가장 작은 항목 하나뿐이고, 이유를 `#[expect]`에 적는다.

## 런타임 의존성

| 크레이트 | 고정 | 용도 | 결함과 조치 |
|---|---|---|---|
| tokio (MIT) | =1.53.1 | 비동기 런타임, 상한 큐, UDP, 시그널 | 상한 없는 채널, `blocking_*` 송수신, 상한 없는 io-util 읽기(`read_to_end`, `read_line`, `lines` 등), `tokio::sync::Mutex`·`RwLock`, `block_on`, 런타임 생성, 시계 진입점을 금지 |
| rayon (MIT/Apache-2.0) | =1.12.0(rayon-core 1.13.0, crossbeam-epoch 0.9.21) | CPU 풀 | 전역 풀을 몰래 만드는 진입점(`join`, `scope`, `spawn`, 병렬 반복자 등) 금지, `src/pool.rs`만 씀. 풀 drop이 스레드를 기다리지 않음 → `exit_handler`로 세어 종료 때 기다림. 패닉 처리기 없는 `spawn` 잡 패닉은 abort → `spawn`을 쓰지 않음. 잡 패닉은 호출한 틱으로 전파됨을 확인. crossbeam 권고는 고친 판보다 새 판 |
| aws-lc-rs (ISC/Apache-2.0) | =1.18.1, 기본 기능 끔 | ES384, RS256, ECDH P-384, SHA-256, AES-256-CTR, 난수 | ECDSA 공개키 파서가 SPKI 뒤 남는 바이트와 SEC1 점도 받음 → DER SEQUENCE 하나만 받게 먼저 검사. 스트리밍 CTR이 무작위 IV → 금지하고 자체 카운터. 서명 키와 ECDH 키가 다른 타입 → PKCS#8로 잇고 공개키가 같은지 확인. 난수·키 생성 진입점과 `SystemRandom` 금지(난수는 crypto 모듈 한 곳) |
| rustls (Apache-2.0/MIT/ISC), webpki-roots | =0.23.45(aws_lc_rs, ring 끔), 1.0.9 | 인증 문서 HTTPS | 공급자가 없으면 패닉 → 늘 aws-lc-rs 공급자를 명시. 전역 공급자 진입점 금지. ring은 메타데이터에만 있고 빌드되지 않음 |
| tokio-rustls (MIT/Apache-2.0) | =0.26.6, `aws_lc_rs`·`tls12`만 | NetherNet HTTPS 시그널링 | 0.26.5의 회귀를 피한 판. 기한이 없음 → 요청 10초 마감 안에서만 씀. 티켓·세션 저장 끔 |
| ureq (MIT/Apache-2.0) | =3.4.2 | 인증 문서 받기(블로킹 풀에서) | 시간 상한을 켜면 DNS마다 분리 스레드 → 인라인 resolver(DNS는 OS 상한). 본문 상한이 상한과 같은 크기를 거부 → 상한을 1 MiB 미만으로. 블로킹 호출과 상한 없는 본문 읽기 금지 |
| libdeflater (MIT) | =1.26.1(libdeflate 1.26) | zlib 압축(level 6에서 miniz_oxide보다 2~4배 빠름, 실측) | `use_rust_alloc` 기능의 메모리 부족 null 역참조 → 기본 기능만. `Decompressor`가 내부 상태로 패닉 → 금지(해제는 miniz_oxide). `Compressor::new`는 할당 실패에만 패닉(생성 때만, 외부 입력 무관) |
| miniz_oxide (MIT/Apache-2.0/Zlib) | =0.9.1 | zlib 해제 | `decompress_to_vec*`는 상한 없음 → 금지, 상한까지만 자라는 자체 루프 |
| serde, serde_json, toml | =1.0.229, =1.0.151, =1.1.6 | 설정, 인증 캐시, JWT, IR 읽기 | toml은 깊이 80에서 멈춤(`unbounded` 기능은 켜지 않음). toml 오류 `Display` 안의 `expect` → `message()`와 `span()`만 씀. `serde_json::from_reader` 금지, 신뢰 못 할 JSON은 길이 상한 뒤 타입 있는 구조체로 |
| tracing (MIT) | =0.1.44, std만 | 로그 | `attributes` 기능을 꺼 syn 2 빌드를 뺌 |
| socket2 (MIT/Apache-2.0) | =0.6.5 | IPv6 전용 소켓, 소켓 버퍼 | 블로킹 입출력 진입점 금지 |
| memory-stats (MIT/Apache-2.0) | =1.2.0, `always_use_statm` | `/status` 상주 메모리(Anchor는 `unsafe`를 쓰지 않아 OS 호출을 맡김) | 기본 Linux 경로가 `/proc/self/smaps` 전체를 읽음 → `always_use_statm`. 파일 읽기 → 블로킹 스레드에서만(금지 목록). windows-sys 0.52 고정 → 중복 허용(`.clippy.toml`에 이유) |
| redb (MIT/Apache-2.0) | =4.3.0, `vendor/redb` 사본 | 서버 데이터 저장 | 아래 사본 절 |
| str0m (MIT/Apache-2.0) | =0.24.1, 기본 기능 끔 + `aws-lc-rs`, `vendor/str0m` 사본 | NetherNet WebRTC | 아래 사본 절 |
| dimpl, sctp-proto | 0.7.5, 0.10.5, 사본 | DTLS 1.2, SCTP(str0m 아래) | 아래 사본 절 |
| str0m-proto, str0m-aws-lc-rs, is (MIT/Apache-2.0) | 0.7.0, 0.6.0, 0.11.1, 사본(코드 그대로) | str0m과 dimpl 사이 | 아래 사본 절 |

`.clippy.toml`의 `allowed-duplicate-crates`는 windows-sys(memory-stats와 쓰지 않는 ring), nom과 untrusted(메타데이터에만
있고 빌드되지 않음), syn(str0m 쪽 proc-macro)만 허용하고 각 이유를 그 자리에 적는다.

## 직접 구현하는 것

| 대상 | 이유 |
|---|---|
| RakNet | 상류(go-raknet) 결함이 많다(쿠키가 CRC32, 위조 주소로 남을 막는 오류 차단, 신뢰 인덱스 중복 제거 없음, 24비트 번호 넘침, 이차 재조립, 혼잡 제어 없음, 사실상 상한 없는 큐 등) |
| Snappy | snap 크레이트는 외부 입력 경로에 `unsafe`가 있다 |
| 시그널링 HTTP | httparse, hyper는 필요한 것보다 크고 상한을 Anchor가 정해야 한다. tokio 위의 작은 구현으로 충분하다 |
| LevelDB 리더(가져오기) | rusty-leveldb는 열 때 원본 폴더에 쓰고(LOCK, 로그 재생, compaction) 디스크 입력 경로에 `unsafe`와 snap이 있다 |
| 오류 타입, CRC, base64, UUID, NBT | 작고 정답 시험이 쉽다. 오류 타입은 모듈마다 손으로 쓴다 |
| 결정적 수학(`sin`, `cos`, `atan2` 등) | std 삼각함수는 플랫폼마다 다를 수 있어 금지다. 사칙연산과 `sqrt`만 쓰는 근사(fdlibm 계수)를 둔다 |

## vendor 사본

상류가 아직 고치지 않은 결함은 `vendor/<크레이트>/`에 사본을 두고 고친다. 규칙:
- 결함만 고친다. 기능을 더하거나 다듬지 않는다. 고친 곳과 이유는 이 문서의 사본 절에 적고, 코드에도 `Anchor` 주석을 단다(dimpl처럼 주석이 없는 사본은 이 문서가 기준이다).
- 고칠 때마다 재현 시험을 둔다. 원본에서 실패하고 사본에서 통과해야 한다.
- 사본은 crates.io의 고정 판에서 시작한다(`.cargo_vcs_info.json`이 상류 커밋을 적는다). 고친 곳과 다른 사본을 가리키는
  `path` 줄만 원본과 다르다.
- 상류 릴리스가 결함을 고치면 그 고침을 사본에서 빼고, 남은 고침이 없으면 사본을 지우고 릴리스로 돌아간다. 새
  상류 판이 나오면 이 문서의 절차로 검토해 올린다.
- 사본은 `[patch]`가 아니라 `path` 의존성이다. `[patch]`는 가장 바깥 작업 공간에만 적용되어, Anchor를 의존성으로 쓰는
  사용자 프로젝트에는 결함 있는 원본이 들어간다. `xtask/tests/downstream.rs`가 저장소 밖 임시 프로젝트에서 모든 사본
  크레이트가 사본으로 풀리는지 확인한다.
- 사본은 작업 공간 멤버가 아니다(`exclude = ["vendor"]`). 포맷과 lint 대상이 아니고, `verify`가 사본 자체 시험을 따로
  돌린다: sctp-proto `--lib`, redb `--tests`, str0m `--lib`(Anchor 기능), dimpl `--tests`.
- dimpl 시험은 OpenSSL을 소스로 빌드하므로 Windows에서 Strawberry Perl(`.cache/tools`, `data.md`)을 그 명령에만
  `OPENSSL_SRC_PERL`로 넘기고 MSVC 링커를 쓴다(rust-lld는 OpenSSL 정적 라이브러리의 디버그 정보에 경고를 내고, 경고는
  `verify` 실패다). wolfssl 시험은 Windows가 아닐 때만 돈다.
- str0m 통합 시험은 공개 크레이트에 없는 캡처 파일 7개가 필요해 커밋하지 않는다. 사본을 바꿀 때
  `cargo xtask vendor-test str0m`이 상류 커밋에서 받아 sha256을 확인하고 돌린다.

### redb 4.3.0

| 고침 | 결함 | 재현 시험 |
|---|---|---|
| ① | 캐시가 파일보다 크면 쓰기마다 페이지 캐시 큐가 자란다(#1517) → 상류 master `f95d4609`의 `lru_cache.rs`를 옮김 | 사본 안 `lru_cache::test::queue_stays_bounded_without_eviction` |
| ② | 열 때 체크섬 확인 없이 시스템 트리를 읽어 손상 바이트 하나에 패닉(#1505, #1334 계열) → 열 때 시스템 트리 체크섬을 먼저 확인해 `Corrupted` 오류로. 디버그 빌드가 데이터 트리를 확인 없이 걷는 것도 먼저 확인 | `xtask/tests/redb_repro.rs`(1 MB 파일 무작위 1바이트 손상 2만 번 × 3상태에서 원본 패닉, 사본 0) |
| ③ | 파일이 자랄 때마다 두 배로 늘려, `compact` 뒤 작은 커밋 하나가 파일을 두 배로 만든다 → 파일의 8분의 1(할당의 2배 이상)씩 늘림 | `a_small_commit_after_a_compaction_grows_the_file_by_a_step_not_double` |

사본은 ②와 부딪치는 상류 시험 기대 한 곳(`check_integrity_failure`)도 바로잡는다. 남는 위험: 슬롯 체크섬까지 맞춘 조작
파일은 최대 4 GiB를 할당한다(#1503, 무작위 손상으로는 닿지 않음). 실행 중 생긴 페이지 손상은 읽기 패닉을 낼 수 있다
(#1334, 저장 스레드 패닉 → 종료, 다음 시작에서 거부). 숨은 스레드·시계·난수는 없고 기본 캐시는 64 MiB로 줄인다.
열기, `begin_read`, `begin_write`, `check_integrity`, `compact`, `commit`은 금지 목록에 있고 storage 모듈만 쓴다.

### str0m 0.24.1

| 고침 | 결함 | 재현 시험 |
|---|---|---|
| ③ | SCTP RTO 초기값이 RFC 4960의 3초라 INIT를 잃으면 3초를 기다린다(손실 10%에서 성립 마감 10초를 넘기는 경우가 생김) → RFC 9260의 1초(`sctp::RTO_INITIAL_MS`) | `src/nethernet/peer.rs`의 `a_lost_sctp_init_is_sent_again_after_one_second` |
| ④ | 클라이언트 SCTP가 받는 스트림 수를 65,535로 알려, 원격이 스트림 ID마다 DATA를 쓰면 항목이 쌓이고 poll마다 모두 훑는다 → 16(`sctp::MAX_INBOUND_STREAMS`, NetherNet 채널 ID 1·3) | 사본 안 `sctp::tests::many_remote_streams_stay_bounded` |
| ⑤ | 모르는 스트림의 ChannelData 버림을 warn으로 찍어 원격이 로그를 채운다 → debug | - |

번호 ①(128 KiB 고정 SCTP 송신 버퍼)과 ②(`poll_output` 재귀 스택 넘침)는 0.24.1 상류가 고쳐 사본에 없다. 사본 밖
가드: 기본 설정이 전역 공급자·fastrand ICE 자격·자체 인증서를 쓰므로 모두 명시 설정하고 해당 진입점을 금지한다.
직접 API는 원격 `max-message-size`를 받지 못해 SDP API에 `setup:passive` 오퍼를 준다(응답 SDP는 Anchor가 만든다).
ICE는 끊김만 알리고 SCTP는 무한 재전송하므로 피어 마감과 무응답 10초를 둔다. `accepts`가 선형이라 ufrag·주소 색인을
둔다. 남은 내부 패닉은 피어마다 `catch_unwind`로 그 피어만 닫는다. ICE 비밀번호가 trace에 찍히므로 `pii`를 끈다.

### dimpl 0.7.5

| 고침 | 결함 |
|---|---|
| ① | ChangeCipherSpec 전에 같은 epoch 레코드가 두 번 오면 assert → replay처럼 버림 |
| ② | 피어 인증서가 poll 버퍼(str0m 2,000바이트)보다 크면 assert → `BufferTooSmall`을 돌려 str0m이 오류로 끊음 |
| ③ | Auto 모드 `supported_versions`가 잘리면 범위 밖 읽기 → 길이 검사 |

재현 시험은 `xtask/tests/dimpl_repro.rs`(원본에서 모두 실패)다. 버퍼 풀이 자라는 결함은 0.7.5 상류가 고쳐 사본에 없다.
상류는 풀 밖 버퍼 반납에 release `assert!`를 두지만 풀 밖 버퍼가 반납되는 길이 없음을 확인했고, 판을 올릴 때마다
다시 본다. 한 데이터그램 안 레코드 순서가 바뀌면 0.7.5는 그 데이터그램을 버리고 재전송으로 회복한다.

### sctp-proto 0.10.5

| 고침 | 결함 |
|---|---|
| ① | 수신 창이 0이어도 빠진 청크를 무제한 받음 → 누적 TSN 바로 다음 청크만(RFC 4960 6.2) |
| ② | 받은 TSN 기록이 읽힌 데이터를 붙잡고 창에 세지 않음 → TSN만 두고 기록마다 창에서 뺌 |
| ③ | 스트림 수 협상이 뒤집힘(INIT·INIT-ACK) → RFC 4960 5.1.1대로. 협상 수 밖 스트림은 만들지 않고, 그 DATA는 TSN을 인정하고 데이터만 버림(6.5). 다시 연 스트림은 대기열에 한 번만 |
| ④ | 같은 원격의 새 INIT마다 연결이 쌓임 → 원격 하나에 연결 하나 |
| ⑤ | 빈 DATA를 받아 둠 → 연결 종료(6.2) |
| ⑥ | SACK 갭 오프셋 u16 넘침과 TSN `+1` 넘침(디버그 패닉) → 순환 산술(갭 블록 루프 포함) |
| ⑦ | 보낼 때·받을 때 청크마다 큐 전체를 다시 정렬(이차 비용) → 정렬된 자리에 넣음 |
| ⑧ | poll마다 전송 중 청크 전체를 재전송 탐색 → T3 타임아웃 표시가 없으면 건너뜀 |

재현 시험은 사본 안 `association/anchor_test.rs`와 `endpoint/anchor_test.rs`다. 남김: SSN 정렬 비교자가 전순서가
아니다(실사용 방식 약 400만 번에서 재현되지 않고, 패닉이 나도 그 피어만 닫힘). 연결 중 INIT·ERROR 청크에 연결을
닫는다(RFC 위반, 그 피어만 끊김). `Endpoint::connect`가 시계를 읽는다.

### str0m-proto, str0m-aws-lc-rs, is

코드를 바꾸지 않은 사본이다. 사본이 없으면 레지스트리 판이 따라와 dimpl이 둘이 되고 타입이 맞지 않는다(결함 있는
원본). crates.io 파일과 바이트 단위로 같고(sha256 = `Cargo.lock` checksum) `path` 줄과, 공개 크레이트에 빠진 str0m
저장소의 `LICENSE-MIT.txt`만 더한다. str0m 릴리스로 돌아갈 때 함께 지운다.

## 개발 도구 (Go, xtask/go)

Anchor 빌드에는 들지 않는다. `xtask/go/go.mod`가 판을 고정한다.

| 모듈 | 고정 | 용도 | 결함과 조치 |
|---|---|---|---|
| sandertv/gophertunnel (MIT) | `80c811b`(v1.62.0 뒤, 프로토콜 2193) | 프로토콜 정답, 추출기, 봇 | 주석과 코드 불일치, NBT map 쓰기 순서가 무작위 → 시험 도구가 정렬해 씀. 로그인 검사가 느슨함(앵커 없는 정규식, 블로킹 DNS, 무작위 오프라인 UUID, 느슨한 base64) → Anchor는 엄격하게 고쳐 구현(`protocol.md`, `network.md`) |
| sandertv/go-raknet, df-mc/go-nethernet | gophertunnel `go.mod`가 정한 판 | 봇 | 봇 전용. go-nethernet은 후보 줄을 두 번 접두하고 채널 열림 확인 전에 보냄 → Anchor가 둘 다 받음 |
| go-jose, go-oidc | `go.mod` 판 | 봇 토큰 | 개발 도구 |
| df-mc/goleveldb (BSD-2) | v1.1.9 | 가져오기 리더 대조 | 압축 2(zlib 헤더)가 없고 열 때 폴더에 씀 → 사본을 열어 읽음 |
| golang.org/x/tools (BSD-3) | v0.42.0 | 추출기 타입 정보 | gophertunnel 의존성 판을 바꾸지 않는 판 |

PM5 PHP, Strawberry Perl, BDS 같은 바이너리 도구의 출처와 sha256은 `data.md`에 있다.

## 저작권 규칙

- PMMP, BDS, dragonfly, gophertunnel 등 다른 서버의 문장, 출력 형식, 주석, 코드 덩어리를 Anchor에 옮기지 않는다.
  원리만 배우고 Anchor 고유의 코드와 문장으로 쓴다.
- 서버가 만드는 모든 문장(콘솔 로그, 명령 출력, 킥 사유, 오류 메시지, status 출력 등)은 Anchor 고유 문장이다.
  클라이언트 번역 키를 보내 클라이언트가 자기 문장을 보이게 하는 것은 된다.
- 프로토콜이 요구하는 형식(핑 응답 필드 순서 등)은 형식이 아니라 규약이라 그대로 따른다.
- 생성 코드의 문서는 생성기가 IR의 정보(이름, 패킷 ID와 방향, 와이어 형식, 상한)로 쓴다. 추출기는 gophertunnel
  주석을 읽지 않는다.
- 데이터는 사실(값, 번호, 상한)만 쓴다. Mojang 프로토콜 문서는 숫자만 옮기고 파일을 복사하지 않는다. PMMP 데이터는
  PMMP를 실행한 결과다.

## 빌드 시간

저장소의 설정은 같은 결과에 시간만 줄이는 것만 둔다.
- `[profile.dev] debug = "line-tables-only"`, 의존성 `debug = false`: 패닉과 백트레이스의 파일·줄은 남고 디버그 정보
  쓰기와 링크 시간이 준다.
- Windows `.cargo/config.toml`의 `linker = "rust-lld.exe"`: 많은 시험 바이너리를 link.exe보다 빨리 잇는다.
- 의존성 `opt-level = 1`은 빌드가 느려져 쓰지 않는다. sccache는 target 경로가 캐시 키에 들고 proc-macro, 빌드
  스크립트, 링크가 캐시되지 않아 효과가 없다.

Anchor를 쓰는 사용자 프로젝트에 권장하는 설정은 크레이트 문서 첫 페이지 "Build, start and stop times"에 있다.
의존성의 `debug = "line-tables-only"`와 Windows rust-lld다. 둘 다 release 코드는 같아 게임 성능과 무관하다. 빈
서버로 잰 결과(8잡): 깨끗한 dev 빌드 -25%, Anchor를 바꾼 뒤 dev 재빌드 -35%, 사용자 코드만 바꾼 뒤 dev -55%,
release -14%. release `incremental`과 `codegen-units` 변경은 인라인이 달라져 틱 성능을 깎을 수 있어 권장하지 않는다.
release에서 Anchor를 다시 빌드하는 시간의 대부분은 anchor 크레이트 하나의 프런트엔드와 코드젠이다.
