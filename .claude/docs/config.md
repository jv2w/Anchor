# 설정

Anchor의 설정은 `anchor.toml` 파일 하나다. 이 문서는 모든 키의 타입, 기본값, 범위, 뜻과 읽기 규칙을 적는다. 설정이 저장과 맺는 관계는 `storage.md`에 있다.

작업 규칙은 `.claude/rules/config.md`.

## 파일과 읽기

- 사용자 바이너리가 `Config::default().load("anchor.toml")?`로 읽어 `ServerBuilder::config`에 넘긴다. `config()`를 부르지 않으면 Anchor는 설정 파일을 읽지도 쓰지도 않는다. 라이브러리라 파일 위치와 사용 여부를 사용자가 정한다.
- 파일이 없으면 `load`가 아래 기본 파일을 임시 파일에 쓰고 이름을 바꿔 만든 뒤 그 파일을 읽는다. 그래서 첫 시작과 그 파일로 하는 다음 시작의 설정이 같다. 그 밖에는 사용자가 `Config::update_file`을 부를 때만 파일을 다시 쓴다(아래 "파일 갱신"). 실행 중 API로 바꾼 값은 종료까지만 유지된다.
- 값의 우선순위: `Config::default()` → 코드에서 바꾼 값 → `load`가 파일에 있는 키로 덮음 → 그 뒤 코드에서 바꾼 값. 파일에 없는 키는 앞의 값(보통 기본값)을 유지하므로 새 Anchor도 옛 파일로 시작한다.
- 파일은 1 MiB 이하 UTF-8 TOML이다. 문법 오류는 줄과 칸을 알려 준다.
- 표는 둘이다: `[general]`(서버를 여는 누구나 정하는 값)과 `[developer]`(성능과 프로토콜, 주의가 필요한 값). 월드별 값은 `[worlds.<이름>]`이다.

## 기본 파일

`Config::load`가 쓰는 기본 파일(`src/config.rs`의 `DEFAULT_FILE`)이다. 모든 키가 들어 있다.

```toml
# Anchor server settings. Anchor reads this file at start and writes it only to update it:
# "<server program> update-config" adds the keys of a newer Anchor and removes unknown keys.
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
# Ticks a dropped item stays on the ground (1 to 72000).
item-despawn-ticks = 6000
# Critical hits: "off", "sprint" (only when not sprinting) or "always" (sprinting too).
critical-hits = "sprint"
# The server checks that a block took long enough to break.
break-time-check = true
# The server checks that a move is not too far (more than 15 blocks in one input).
movement-check = true
# Every player sees its coordinates on screen.
show-coordinates = false
# Main world. level-type fills the chunks never saved: "flat" or "void".
level-name = "world"
level-type = "flat"
# Whole number as text. Empty: made from the world name, the same at every start. Other text stops the start.
level-seed = ""
# Largest view distance a player may use, in chunks (2 to 64).
view-distance = 16
# false: player inventories and positions are not kept between joins.
save-player-data = true
# Copies of the data files in data/backups, at start and with the backup command.
backups = true
# Message players see when the server stops.
shutdown-message = "The server has stopped"
# Resource pack files (.mcpack or .zip), relative to data/packs. Earlier entries override later ones.
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
# Largest decompressed batch a client may send, in bytes (4210786 to 67108864: at least
# what one player with the largest skin takes in the player list).
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
# When the data files are checked at start: "always", "after-unclean-shutdown" or "never".
# The check command checks them while the server runs.
integrity-check = "always"
# false: Anchor does not read commands from standard input.
console-input = true

# Per-world values, by world name. They override view-distance, chunks-per-tick,
# spawn-radius and tick-radius for that world, and spawn = [x, y, z] sets where players
# appear in a new world (default: the generator's spawn), for example:
# [worlds.lobby]
# view-distance = 8
# chunks-per-tick = 8
# spawn = [0, -60, 0]
```

## 검증

모든 값은 시작 전에 검사하고, 틀리면 키 이름을 담은 `config::Error`로 시작을 거부한다.

- 모르는 키는 `UnknownKey`, 모르는 최상위 표는 `UnknownTable`(`build()`에서 검사), 타입이나 모양이 틀린 값은 `Invalid`, 범위 밖 정수는 `Range`(키, 값, 최소, 최대)다. `compression-threshold`, `compression-level`, `max-batch-bytes`는 `batch::Settings::new`가 검사해 범위 밖 값이 `Batch`(키와 원인)다. 코덱 범위(1 MiB부터) 안이어도 플레이어 목록 항목 하나보다 작은 `max-batch-bytes`는 `Invalid`다.
- `hardcore = true`는 `difficulty = "hard"`일 때만 된다.
- `nethernet-tls-cert`와 `nethernet-tls-key`는 둘 다 비거나 둘 다 있어야 한다.
- `[worlds.<이름>]`은 서버가 선언한 월드(`level-name`이나 `ServerBuilder::world`)여야 한다. 아니면 `UnknownWorld`다.
- 코드에서 바꾼 값도 같은 검사를 거친다.

## `[general]`

| 키 | 타입 | 기본값 | 범위와 형식 | 뜻 |
|---|---|---|---|---|
| `motd` | 문자열 | `"Anchor Server"` | 1~256바이트, `;`와 제어 문자 없음 | 서버 목록에 보이는 이름. RakNet 핑 응답 한 데이터그램(최소 MTU 576)에 `level-name`과 함께 들어가야 해서 길이를 묶는다 |
| `server-ip` | 문자열 | `"0.0.0.0"` | IPv4 주소 | 받을 IPv4 주소. `0.0.0.0`은 모든 인터페이스 |
| `server-ipv6` | 문자열 | `"::"` | IPv6 주소 | 받을 IPv6 주소 |
| `enable-ipv6` | bool | `true` | | IPv6에서도 받을지 |
| `server-port` | 정수 | `19132` | 0~65535 | UDP는 RakNet과 서버 목록 핑, TCP는 NetherNet 시그널링. IPv4와 IPv6 모두 |
| `nethernet-port` | 정수 | `19133` | 0~65535 | NetherNet 게임 데이터 UDP 포트. 모든 플레이어가 함께 쓴다 |
| `public-ip` | 문자열 | `""` | 빈 값 또는 IP 주소 | 플레이어가 이 서버에 닿는 주소(NAT 뒤). 비면 로컬 인터페이스 주소를 알린다 |
| `max-players` | 정수 | `20` | 1~10000 | 동시 접속 상한. 플레이어와 세션 표의 크기를 정한다 |
| `xbox-auth` | bool | `true` | | Xbox Live 로그인을 요구할지. `false`면 누구나 아무 이름으로 들어온다 |
| `white-list` | bool | `false` | | 목록에 있는 플레이어만 들어올지. 목록은 `whitelist` 명령으로 관리한다 |
| `gamemode` | 문자열 | `"survival"` | `survival`, `creative`, `adventure`, `spectator` | 새 플레이어의 게임 모드. 설정은 전체 이름만 받는다(명령은 `s`, `0` 같은 글자와 숫자 별칭도 받는다) |
| `force-gamemode` | bool | `false` | | 접속할 때마다 기본 게임 모드로 맞출지 |
| `difficulty` | 문자열 | `"normal"` | `peaceful`, `easy`, `normal`, `hard` | 난이도 |
| `hardcore` | bool | `false` | `difficulty = "hard"`일 때만 `true` | 하드코어 |
| `pvp` | bool | `true` | | 플레이어끼리 피해를 줄 수 있는지 |
| `hunger` | bool | `true` | | 배고픔 |
| `experience` | bool | `true` | | 경험치 점수와 레벨을 얻는 것. `false`면 구슬도 없고, 인챈트대와 모루는 레벨 0으로 보아 레벨이 드는 일을 거부한다 |
| `experience-orbs` | bool | `true` | | 경험치 구슬과 경험치 병 던지기. `false`면 떨굴 경험치를 셈하지도, 구슬을 만들거나 읽지도 않고, 병 던지기를 거부한다. 구슬 대신 경험치를 바로 주지 않는다. 점수, 레벨, `/xp`는 `experience`만 따른다 |
| `fall-damage` | bool | `true` | | 낙하 피해 |
| `drowning` | bool | `true` | | 물속 공기, 익사, 질식 피해 |
| `armor` | bool | `true` | | 갑옷 방어, 갑옷 소모, 가시. `false`면 셋 다 없다 |
| `durability` | bool | `true` | | 도구, 무기, 갑옷이 쓰면서 닳는 것. `armor`가 켜져 있으면 갑옷은 막되 닳지 않는다 |
| `beds` | bool | `true` | | 침대 수면과 침대 스폰 지점. `false`면 침대는 장식 블록이다 |
| `daylight-cycle` | bool | `true` | | 시간이 흐르는 것. `false`면 클라이언트 시계도 멈춘다 |
| `recipe-book` | bool | `true` | | 레시피 책. `false`면 숨기고 그 빠른 제작을 거부한다. 제작 칸 제작은 된다 |
| `vanilla-recipes` | bool | `true` | | 바닐라 레시피. `false`면 서버 프로그램이 등록한 레시피만 있다 |
| `item-despawn-ticks` | 정수 | `6000` | 1~72000 | 떨어진 아이템이 남는 틱 |
| `critical-hits` | 문자열 | `"sprint"` | `off`, `sprint`, `always` | 치명타. `off`는 없음, `sprint`는 달리지 않을 때만(바닐라), `always`는 달릴 때도 |
| `break-time-check` | bool | `true` | | 블록을 부수는 데 충분한 시간이 걸렸는지 서버가 검사할지. 도달 거리와 블록 종류 검사는 늘 한다 |
| `movement-check` | bool | `true` | | 한 입력에 15블록을 넘는 이동을 거부할지. `false`면 그런 이동의 땅 판정은 지나온 길 없이 도착 자리만 본다 |
| `show-coordinates` | bool | `false` | | 모든 플레이어 화면에 자기 좌표를 보일지. 게임 규칙 `showCoordinates`의 값이다 |
| `level-name` | 문자열 | `"world"` | 1~64바이트, `;`와 제어 문자 없음, 월드 이름 규칙(`storage.md`) | 기본 월드 이름. 파일은 `data/worlds/<이름>.redb` |
| `level-type` | 문자열 | `"flat"` | `flat`, `void` | 기본 월드에서 저장된 적 없는 청크를 채우는 생성기 |
| `level-seed` | 문자열 | `""` | 빈 값 또는 i64 정수 문자열 | 기본 월드 시드. 비면 월드 이름에서 만들어 시작마다 같다 |
| `view-distance` | 정수 | `16` | 2~64 | 플레이어가 쓸 수 있는 최대 시야(청크) |
| `save-player-data` | bool | `true` | | 인벤토리와 위치를 접속 사이에 남길지 |
| `backups` | bool | `true` | | 시작과 `backup` 명령에서 데이터 파일을 `data/backups`에 복사할지 |
| `shutdown-message` | 문자열 | `"The server has stopped"` | 1024바이트 이하 | 서버가 멈출 때 플레이어가 보는 문장 |
| `resource-packs` | 문자열 배열 | `[]` | 비지 않은 경로. 상대 경로는 `data/packs` 기준이고 `packs`로 시작할 수 없다 | 리소스팩 파일(`.mcpack`, `.zip`). 앞의 것이 위에 쌓인다. 암호화된 팩은 옆에 `<파일>.key`가 필요하다 |
| `force-resources` | bool | `false` | | 리소스팩을 받아야 접속할 수 있는지 |
| `log-level` | 문자열 | `"info"` | `off`, `error`, `warn`, `info`, `debug`, `trace` | Anchor 콘솔 로그 수준. `off`면 Anchor가 콘솔 로그를 쓰지 않는다 |

`resource-packs`의 상대 경로가 `packs`로 시작하면 거부하고 사용자가 고친다. 설정 파일 옆 `packs` 폴더 기준으로 쓴 경로는 `data/packs/packs/...`를 찾게 되기 때문이다. 읽는 곳은 늘 `data/packs` 하나다.

## `[developer]`

| 키 | 타입 | 기본값 | 범위와 형식 | 뜻 |
|---|---|---|---|---|
| `compression` | 문자열 | `"snappy"` | `snappy`, `zlib` | 네트워크 압축. Snappy는 CPU를, zlib은 대역폭을 아낀다 |
| `compression-threshold` | 정수 | `256` | 1~65535 | 이보다 작은 묶음은 압축하지 않는다. 0은 구현마다 뜻이 달라 받지 않는다 |
| `compression-level` | 정수 | `6` | 1~9 | zlib 단계. Snappy를 고를 때도 검사한다 |
| `max-batch-bytes` | 정수 | `16777216` | 4210786~67108864 | 클라이언트가 보낼 수 있는 압축 해제 뒤 묶음의 최대 바이트. 최솟값은 가장 큰 스킨(4 MiB)을 가진 플레이어 하나의 플레이어 목록 패킷이다(나눌 수 없어 묶음 하나에 들어야 한다) |
| `enable-encryption` | bool | `true` | | RakNet 게임 통신 암호화. `false`면 중간자가 로그인까지 읽고 바꿀 수 있어 믿을 수 있는 망에서만 쓴다. NetherNet은 자기 전송이 늘 암호화한다 |
| `raknet` | bool | `true` | | RakNet 연결을 받을지. `false`면 NetherNet만 받고 RakNet 포트는 핑에만 답한다 |
| `max-mtu-size` | 정수 | `1492` | 576~1492 | RakNet 최대 패킷 바이트. 터널이나 VPN 뒤에서 낮춘다. go-raknet 다이얼러가 1492, 1200, 576을 시도한다 |
| `nethernet-tls-cert` | 문자열 | `""` | 빈 값 또는 경로 | HTTPS 시그널링용 PEM 인증서 체인. 비면 HTTP이고 플레이어가 첫 접속에서 서버 키를 확인한다 |
| `nethernet-tls-key` | 문자열 | `""` | 빈 값 또는 경로 | HTTPS 시그널링용 PEM 개인 키 |
| `network-threads` | 문자열 또는 정수 | `"auto"` | `"auto"` 또는 1~65535 | 네트워크 작업 스레드. `auto`는 max(1, L/4)(L = 논리 CPU 수) |
| `cpu-threads` | 문자열 또는 정수 | `"auto"` | `"auto"` 또는 1~65535 | 병렬 게임 작업 스레드. `auto`는 max(1, max(L, 4) − 2 − 네트워크 스레드). 네트워크 + CPU + 2(틱, 저장)가 max(L, 4)를 넘으면 시작을 거부한다 |
| `chunks-per-tick` | 정수 | `4` | 1~256 | 플레이어마다 틱당 보내는 청크 수. 256이면 가장 큰 시야를 약 3초에 보낸다 |
| `spawn-radius` | 정수 | `4` | 1~64 | 스폰 전에 보내는 청크 반경. 실제 값은 min(값, `view-distance`) |
| `tick-radius` | 정수 | `4` | 1~64 | 플레이어 주변에서 블록과 엔티티가 틱을 받는 청크 반경. 실제 값은 min(값, `view-distance`) |
| `random-tick-speed` | 정수 | `3` | 0~4096 | 서브청크마다 틱당 랜덤 블록 틱 수(작물 성장, 잎 부식). 0이면 끈다. 서브청크 블록 수(4096)보다 많으면 비용만 늘어서 상한을 둔다 |
| `disable-block-ticking` | 문자열 배열 | `[]` | `namespace:path` 모양 블록 이름 | 랜덤 틱을 받지 않는 블록 |
| `slow-empty-worlds` | bool | `false` | | 플레이어 없는 월드가 100틱에 한 번만 돌지 |
| `lighting` | 문자열 | `"dynamic"` | `dynamic`, `on-load`, `fixed`, 또는 bool | 서버 조명 계산. `dynamic`은 계산하고 블록 변경을 따른다. `on-load`는 청크를 읽을 때만 계산한다. `fixed`는 계산하지 않고 모든 위치를 블록빛 0, 하늘빛 `fixed-light-level`로 본다. bool도 받는다: `true`는 `dynamic`, `false`는 `fixed` |
| `fixed-light-level` | 정수 | `15` | 0~15 | `lighting = "fixed"`일 때 모든 곳의 하늘빛 |
| `item-pickup-interval` | 정수 | `1` | 1~20 | 플레이어가 주울 아이템을 찾는 간격(틱) |
| `hook-movement-packets` | bool | `true` | | 서버가 보내는 이동 패킷이 송신 훅을 거칠지 |
| `hook-player-input` | bool | `true` | | `PlayerAuthInput` 패킷이 수신 훅을 거칠지 |
| `save-interval-minutes` | 정수 | `10` | 1~1440 | 전체 상태 저장 간격(분) |
| `integrity-check` | 문자열 | `"always"` | `always`, `after-unclean-shutdown`, `never` | 시작에서 데이터 파일을 검사하는 때. 실행 중에는 `check` 명령이 검사한다 |
| `console-input` | bool | `true` | | 표준 입력에서 명령을 읽을지 |

`lighting = "fixed"`의 결과: 랜덤 틱은 지하에서도 그 빛을 본다. 15면 작물과 묘목이 동굴에서도 자라고(묘목은 낮에만, 밤에는 하늘빛이 어두워져서), 9 미만이면 작물이, 8 미만이면 묘목이 자라지 않는다. 블록빛이 없어 얼음 블록과 눈 층은 녹지 않는다(서리 얼음은 하늘빛으로 녹는다). 기본값이 15인 이유는 0이면 작물이 자라지 않고, 블록빛을 15로 두면 얼음과 눈이 녹기 때문이다.

`integrity-check`와 `backups`의 시작 비용은 `storage.md`에 있다. 정상 종료 뒤 시작은 `after-unclean-shutdown`이 가장 빠르고, `always`는 매번 파일 전체를 읽는다.

## 월드별 값 `[worlds.<이름>]`

선언된 월드마다 둘 수 있다. 적은 키만 그 월드에서 전역 값을 대신한다.

| 키 | 타입 | 범위 | 뜻 |
|---|---|---|---|
| `view-distance` | 정수 | 2~64 | 그 월드의 최대 시야 |
| `chunks-per-tick` | 정수 | 1~256 | 그 월드의 틱당 청크 수 |
| `spawn-radius` | 정수 | 1~64 | 그 월드의 스폰 전 청크 반경. 그 월드 시야 이하로 잘린다 |
| `tick-radius` | 정수 | 1~64 | 그 월드의 틱 반경. 그 월드 시야 이하로 잘린다 |
| `spawn` | 정수 셋 `[x, y, z]` | 각각 i32 | 새 월드에서 플레이어가 나타나는 곳. 없으면 생성기의 스폰 |

`spawn`은 월드 파일에 기록이 없는 새 월드에만 적용된다. 그 뒤 스폰은 월드 상태로 월드 파일에 저장되어, 실행 중 옮긴 스폰(`/setworldspawn`)도 다시 켜면 그대로이고 이 값을 바꿔도 있던 월드에는 쓰지 않는다. 실행 중 `create_world`로 만든 영구 월드는 다음 실행에서 `Context::load_world(이름, &world::Options)`로 설정을 다시 받는다(스폰은 파일의 것). 가져온 월드는 `level.dat`의 스폰을 가진다.

## 다른 서버와 이름이 다른 키

PMMP 설정과 이름이 다른 키:

| PMMP | Anchor |
|---|---|
| `batch-threshold` | `compression-threshold` |
| `blocks-per-subchunk-per-tick` | `random-tick-speed`(Java판 이름과 기본값 3, Bedrock 기본값 1과 다름) |
| `chunk-sending.per-tick` | `chunks-per-tick` |
| `ticks-per.autosave` | `save-interval-minutes`(분 단위) |
| `network.enable-encryption` | `enable-encryption` |
| `debug.level` | `log-level`(끄기와 모든 수준) |

PMMP 설정 중 없는 것: PHP 전용(메모리, GC, async-workers, timings, 통계·보고·업데이트), 플러그인, 범위 밖 기능(query, upnp, 월드 형식), 구조로 대신하는 것(XUID 검사, 생성기 문자열, 별칭, 월드 목록, 언어, IPv6 포트, auto-save).

## 파일 갱신

서버 프로그램을 `<서버> update-config`로 실행하면 옛 Anchor가 만든 파일을 이 Anchor의 키로 맞춘다(프로그램 코드는 필요 없다). `Config::load`가 파일을 읽기 전에 `Config::update_file(경로)`을 하고(옛 파일은 갱신 전에는 읽히지 않을 수 있다), `Server::run`이 바뀐 키를 표준 출력에 적고 서버를 켜지 않고 끝난다. `Config::load`를 쓰지 않는 프로그램에는 고칠 파일이 없다. 시작은 이 일을 하지 않으므로 파일은 운영자가 고른 때에만 바뀐다. 서버가 켜져 있어도 되고, 켜진 서버는 다음 시작에 새 파일을 읽는다.

- 더하기: 기본 파일에 있고 파일에 없는 키를 기본값과, 기본 파일에서 그 키 바로 위에 붙은 주석 줄과 함께 더한다. 자리는 기본 파일에서 그 키보다 앞인 키 중 파일에 있는 가장 가까운 키의 줄 다음이고, 그런 키가 없으면 표 머리 줄 다음이다. 파일에 없는 표는 머리 위 주석과 모든 키를 파일 끝에 빈 줄 하나를 두고 더한다.
- 지우기: Anchor가 모르는 `키 = 값`(최상위, 그리고 머리 줄로 적힌 `[general]`·`[developer]` 안)은 그 줄들과 바로 위에 붙은 주석 줄(`#`로 시작하는 줄, 빈 줄에서 끊김)과 함께 지운다. 여러 줄 문자열 값의 줄은 주석으로 보지 않는다. 점 키나 하위 표(`[general.x]`)는 두어 `load`가 알린다. 모르는 표와 `[worlds.<이름>]` 안의 키도 지우지 않는다.
- 그 밖의 바이트(값, 주석, 키 순서, BOM, 줄 끝)는 그대로다. 파일이 CRLF를 쓰면 더한 줄도 CRLF다. `[worlds.<이름>]` 표는 건드리지 않는다: 거기서 빠진 키는 전역 값을 뜻한다.
- 값은 고치지 않는다. 결과가 `load`로 읽히지 않으면(모르는 키, 범위 밖 값, 문법 오류 등) 그 오류를 돌려주고 파일을 그대로 둔다. 파일이 없으면 `Read`(NotFound), 키가 빠진 표가 머리 없이(인라인 표, 점 키) 적혀 있으면 `HeaderlessTable`, 결과가 1 MiB를 넘으면 `TooLarge`다.
- 바뀐 것이 있을 때만 임시 파일에 쓰고 이름을 바꿔 통째로 바꾼다. 돌려주는 `config::Update`가 더한 키(`added`)와 지운 키(`removed`)를 알려 준다. 다시 부르면 아무것도 바꾸지 않는다.
- 더한 값은 기본값이라 파일의 뜻은 그대로다(새 키의 기본값은 그 키가 없을 때의 동작이다). 다만 `load` 전에 코드에서 바꾼 값은, 그 키가 더해지면 파일 값에 덮인다. 파일이 없을 때 기본 파일을 쓰는 것과 같다.
- 계속 읽히는 옛 값 형식(`lighting = true` 등)과 이미 있는 키의 주석은 바꾸지 않는다.

## 기능 스위치

`hunger`부터 `movement-check`까지의 bool과 `backups`처럼 기능을 켜고 끄는 키는, `false`면 그 기능의 코드가 아예 돌지 않는다: 틱 작업, 이벤트, 패킷, 그 기능의 표와 메모리가 없다.

새 키가 생겨도 옛 파일은 키를 더하지 않고 같은 서버가 된다.

## 저장하지 않음

설정은 데이터 파일에 저장하지 않는다. 매 시작에 `anchor.toml`과 코드에서만 온다. 그 이유와 실행 중 바꾼 값이 다시 켜면 어떻게 되는지는 `storage.md`에 있다.
