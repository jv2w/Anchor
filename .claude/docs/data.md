# 블록과 아이템 데이터

작업 규칙은 `.claude/rules/data.md`.

데이터는 덤프 원본(아래 출처 표의 첫 줄)의 레지스트리를 그 전용 PHP로 실행한 결과라 덤프 원본이 실제로 돌리는 값과 같다.

## 출처와 고정 판

| 출처 | 고정 | 라이선스 | 쓰는 것 |
|---|---|---|---|
| pmmp/PocketMine-MP | 마지막 커밋 `6a7cc02e9dff59b69241aa0bcffdb9903ce86beb`(5.44.3 뒤, Bedrock 1.26.30 데이터) | LGPL-3.0, 보관됨 | 덤프 원본: 블록·아이템 종류와 동작, 레시피, 크리에이티브 목록, 효과·인챈트 표, 골든 |
| pocketmine/bedrock-data | 6.7.0+bedrock-1.26.30(덤프 원본 `composer.lock`) | CC0 | 덤프 원본이 읽는 레시피 등 |
| pmmp/PHP-Binaries | php-build-524, PHP 8.4 PM5 Windows x64(zip sha256 `cf4023b8549c99eec47cfd403805ac7d78100fa366394dab4e45bb544dcc9be4`) | PHP 라이선스 | 덤프용 PHP(개발 도구) |
| df-mc/dragonfly | PR #1481 head `d86aefd9ff9b6a357d6dfadab2093af811545256`(포크 HashimTheArab/dragonfly, 브랜치 `update/1.26.60-upstream`, gophertunnel v1.63.0, 1.26.60). 병합 전 PR 고정, 정식판이 나오면 바꿈 | MIT | 기준 목록 출처: 블록 상태, 아이템, 크리에이티브 아이템 상태 |
| CloudburstMC/Data | `293c848098cc636a2758937fb6610b8bed1f12c5`(26.60) | Apache-2.0 | 기준 목록 출처: 블록 네트워크 ID, 교차 확인, 바이옴, 공식 서버 블록 물리, 그대로 보내는 파일 |
| Mojang/bedrock-protocol-docs | `protocol.md`의 고정 버전 표 | Minecraft EULA | 필드 길이 상한(숫자만 옮김) |
| opencollab-incubator/BedrockBlockUpgradeSchema | 5.4.1 `1077ee640ed39f60266f7a7c8ab331ef76d7931e` | CC0 | 옛 블록 상태 업그레이드 |
| opencollab-incubator/BedrockItemUpgradeSchema | 1.19.0 `dfc12b388be886ba6980d9a3b67cb1b432b563e2` | CC0 | 옛 아이템 이름 업그레이드 |
| 공식 서버(Mojang 배포) | 1.26.52.3(프로토콜 2193) | Minecraft EULA | 대조 실행만. 카메라·조준 보조 프리셋은 공식 서버가 보낸 패킷 캡처에서 옮김 |

- `cargo xtask dump`는 시작할 때 각 체크아웃의 `HEAD`가 위 커밋인지 확인하고, 아니면 실패한다.
- 블록 스키마는 덤프 원본이 고정한 5.2.0이 아니라 5.4.1이다. 5.2.0에는 1.26.30부터의 변경이 없다.
- 아이템 스키마는 덤프 원본이 고정한 1.17.0이 아니라 1.19.0이다. 1.26.50의 이름 바꾸기(`photo` → `photo_item`)와 1.26.60의 `frostbite_spawn_egg`가 있다.
- CloudburstMC 26.60 커밋 뒤의 `88a2436`은 `blocks.json` 형식만 바꾼 것이라 쓰지 않는다.

## 덤프 (`cargo xtask dump`)

순서는 다음과 같다.
1. `xtask/go/reference`(Go)가 기준 목록 출처 두 곳을 읽어 `xtask/data/reference/`에 기준 목록을 쓴다.
   덤프 원본 실행이 기준 상태를 덤프 원본의 역직렬화기로 읽는다.
2. 덤프용 PHP가 `.cache/pmmp`를 작업 폴더로 `xtask/php/dump.php`를 돌려 `xtask/data/pmmp/`에 쓴다.
3. 기준 목록 출처의 바이옴, 엔티티 식별자, 블록 모양을 `xtask/data/reference/`로 옮긴다.
4. Mojang 프로토콜 스키마에서 필드 상한(`protocol_limits.json`)을 만든다(쓰는 법은 `protocol.md`).
5. 업그레이드 스키마를 `xtask/data/upgrade/`로 옮기고 덤프 원본의 업그레이더로 골든을 만든다(아래 업그레이드 절).
6. 기준 목록의 모든 상태에 공식 서버 블록 물리를 붙여 `block_physics.json`에 쓴다(아래 블록 물리). 물리가 없는 상태가 있으면 실패한다.

`--check`는 같은 일을 `target/xtask-dump/`에 하고 커밋된 파일과 바이트가 다르면 실패한다. 덤프는 `verify`에
들지 않는다. 체크아웃, 덤프용 PHP, Go가 필요하고, 입력은 고정 커밋이라 바뀌지 않는다. 대신 `verify`의
`gen --check`가 커밋된 데이터에서 만든 코드를 검사한다.

일반 PHP에는 덤프 원본이 쓰는 전용 확장이 없어 레지스트리를 띄우지 못한다. `.cache/pmmp/vendor`가
없으면 `dump`가 composer 명령을 알려 주며 멈춘다.

### PHP 스크립트

| 스크립트 | 실행 | 출력 |
|---|---|---|
| `xtask/php/dump.php` | `dump`가 실행 | `xtask/data/pmmp/` 아래 `blocks.json`, `deserialized.json`, `items.json`, `creative.json`, `recipes.json`, `tables.json` |
| `xtask/php/upgrade.php` | `dump`가 실행 | `xtask/data/upgrade/golden/` 세 파일 |
| `xtask/php/place.php` | 손으로(`php place.php <출력>`) | `xtask/data/pmmp/place.json`: 놓기 결과 골든 |
| `xtask/php/liquid.php` | 손으로 | `xtask/data/pmmp/liquid.txt`: 액체 흐름 장면 골든 |
| `xtask/php/api.php` | 손으로 | `xtask/data/pmmp/api.json`: 덤프 원본의 공개 API 목록(`cargo xtask table`이 대응표를 만듦) |

NBT는 타입 있는 JSON(`{"byte": 1}`, 실수는 비트 패턴)이다.

### 덤프 내용

- `blocks.json`: 덤프 상태마다 Bedrock 직렬화, 빛 방출과 필터, 직사광 차단, 랜덤 틱, 정확한 블록 레시피 재료의 레거시
  meta, 원본 클래스·부모·트레이트, 부수기 정보, 불 값, 물리 플래그, 마찰, 타입 태그, 6면 지지, 충돌 상자, 아이템 형태,
  결정적 드롭과 경험치, 이웃으로 상태가 정해지는 블록(울타리, 판유리, 벽, 울타리 문, 계단, 트립와이어)의 변형별 값.
- `deserialized.json`: 기준 목록 상태마다 덤프 원본의 역직렬화 결과가 다르면 그 결과(거부하면 null). 덤프 원본은 바닐라
  속성 일부(호박 방향, 매단 간판의 쓰지 않는 방향 등)를 무시하고 한 값으로 쓴다.
- `items.json`: 덤프 아이템 형태마다 Bedrock 직렬화, 원본 이름과 별칭, 스택·내구도·도구·갑옷·음식·연료·인챈트·쿨다운 값.
- `creative.json`: 크리에이티브 목록(순서, 분류, 그룹 그대로).
- `recipes.json`: 덤프 원본이 bedrock-data에서 읽는 레시피. 덤프 원본이 읽지 않는 smithing 레시피는 그 아이템 해석기로
  읽어 더한다. 모르는 아이템이 든 레시피는 덤프 원본처럼 뺀다.
- `tables.json`: 효과, 물약, 인챈트, 아이템 태그, 배너 무늬, 화로 계열 표.

### 덤프와 gen이 바로잡는 원본 결함

- 덤프 원본은 모르는 인챈트를 버려 빈 인챈트 책을 만든다. 이를 빼면 크리에이티브 목록은 1,694개에서 1,633개가 된다.
- 불길한 배너: 덤프 원본은 검은색(meta 0)이고 공식 서버는 흰색(meta 15)이다.
- 블록 아이템의 네트워크 형태: 덤프 원본의 기본 상태(계단 방향 등)가 공식 서버와 다르다.

## 기준 목록 (`xtask/data/reference/`)

| 파일 | 출처 | 쓰임 |
|---|---|---|
| `blocks.json` | dragonfly `block_states.nbt` + CloudburstMC `block_palette.nbt`의 `network_id` | 블록 상태 23,960개와 네트워크 ID |
| `items.json` | dragonfly `vanilla_items.nbt`, CloudburstMC `runtime_item_states.json`과 대조 | 아이템 2,080개(런타임 ID 순, 컴포넌트 데이터 포함). ItemRegistry 전체 |
| `item_blocks.json` | CloudburstMC `creative_items.json`, dragonfly `creative_items.nbt`와 대조 | 블록 아이템의 네트워크 형태 |
| `block_physics.json` | CloudburstMC `blocks.json`(공식 서버 덤프), 아래 블록 물리 규칙 | 비활성 블록의 굳기, 빛, 충돌 상자 |
| `biomes.json` | CloudburstMC `stripped_biome_definitions.json` | BiomeDefinitionList(89개) |
| `entity_identifiers.dat`, `voxel_shapes.json` | CloudburstMC 그대로 | AvailableActorIdentifiers, VoxelShapes |
| `jigsaw_structure_data.dat`, `entity_properties.dat` | CloudburstMC `.nbt`를 네트워크 NBT로 변환 | 스폰 때 보내는 데이터 |
| `protocol_limits.json` | Mojang 프로토콜 스키마 | 디코더 필드 상한 |
| `bds_camera.json` | 공식 서버 스폰 캡처 | 카메라·조준 보조 프리셋 |

- 기준 목록 출처 두 곳의 블록 상태나 아이템이 하나라도 다르면 변환 도구가 실패한다.
- 블록 네트워크 ID는 해시 방식(FNV-1a 32비트, 이름과 정렬한 상태 NBT)이다. 공식 서버도 해시 ID를 쓴다. 기준 목록
  전체가 Rust 해시 함수 결과와 같은지 골든 시험한다.
- 블록 물리: CloudburstMC 물리 갱신 전 임시 규칙 둘이 있다(`xtask/src/dump.rs`, 적용 수는 단위 시험이 고정). 26.60
  커밋의 `blocks.json`은 아직 26.50 상태만 담는다.
  - 연결: 1.26.60이 불·후렴 식물(`minecraft:connection_*`)과 레드스톤 가루(`redstone_*`)에 더한 연결 속성을 가진 상태
    1,872개(불 512, 레드스톤 가루 1,296, 후렴 식물 64)는 그 속성을 뺀 옛 상태(블록 스키마 `0361`이 이 상태로 잇는
    상태)의 물리를 쓴다.
  - 빌린 값: 1.26.60 새 블록 상태 30개는 비슷한 블록의 물리를 빌린다. `icicle`(20) → `pointed_dripstone`,
    `ice_crystal`(6) → `amethyst_cluster`, `disc_press`(4) → `stone`. 두 블록이 함께 가진 속성(방향, 굵기)이 같은
    상태 하나를 빌리고, 하나가 아니면 덤프가 실패한다. Mojang bedrock-samples에는 1.26.60 정식판이 없고(main은
    1.26.50.4, 1.26.60은 프리뷰 태그뿐) 이 블록의 정의도 없다. 블록 동작은 없다(비활성 블록).
  - 이 두 규칙 밖에서 물리가 없는 상태가 있으면 덤프가 실패한다. CloudburstMC가 물리를 갱신하면 규칙을 지운다.
- 바이옴에서 패킷 필드가 없는 값(밀도 등)이 0이 아니면 덤프가 실패한다.
- 덤프 상태 대부분은 기준 목록과 1:1이 아니다. `SHARED`에 든 것은 침대, 물 가마솥, 배너, 벽 배너다.
- light dampening은 덤프 원본의 빛 필터와 일부(잎, 발광석 등) 다르다. 공식 서버의 `isSolid`는 덤프 원본과 뜻이 다르다.
- 공식 서버는 StartGame에 data-driven 블록 정의를 보낸다(색깔별 양털·콘크리트 계단과 반 블록 등). 모두 덤프에 없는
  블록이다.

## 생성 표 (`cargo xtask gen`)

| 생성 파일 | 입력 | 내용 |
|---|---|---|
| `src/block/table.rs` | 덤프, 업그레이드 스키마 `0351`·`0361`, 기준 목록 | Anchor 상태 14,307개와 별칭 4,884개 |
| `src/block/inert/table.rs` | 기준 목록, `block_physics.json` | 비활성 상태 4,769개(이름 264개)와 물리 |
| `src/block/drops.rs` | 덤프 | 결정적 드롭 골든 |
| `src/item/table.rs` | 덤프, 기준 목록 | 덤프 아이템 형태 1,613개 |
| `src/item/registry.rs` | 기준 목록 | ItemRegistry 2,080개 |
| `src/item/creative.rs` | 덤프 | 크리에이티브 107그룹, 1,633개 |
| `src/crafting/table.rs` | 덤프 | 모양·무형·화로·물약·smithing 레시피 |
| `src/effect/table.rs` | `tables.json` | 효과 27개, 물약 43개, 인챈트 |
| `src/world/biome/table.rs` | `biomes.json` | 바이옴 정의 |
| `src/import/upgrade/table.rs` | `xtask/data/upgrade/` | 가져오기 업그레이드 표 |

표가 `static`이라 시작 비용과 할당이 없다.

Anchor 블록 상태는 덤프 상태의 Bedrock 직렬화(중복 제거)에 최신 업그레이드 스키마를 적용한 것과, 그 스키마가 더한
속성의 나머지 값을 합친 것이다. 최신 스키마는 덤프 원본 데이터(1.26.30) 뒤에 속성을 더한 스키마 모두(`0351`,
`0361`)다. 1.26.50은 덤프 블록 115개 이름에 속성을 더했다(울타리·판유리·철창·트립와이어의
`minecraft:connection_*`, 계단의 `minecraft:corner`). 공식 서버는 이 값을 서버에서 이웃으로 계산해 보낸다.
1.26.60은 불·후렴 식물의 `minecraft:connection_*`와 레드스톤 가루의 `redstone_*`를 더했다. Anchor는 이 값을
계산하지 않고 기본값(연결 없음)으로 보낸다. 순서는
이름, 그다음 속성 이름순의 (속성 이름, 값)이고, 값은 타입(byte, int, string) 다음 값 순이다. `block::State`는 그
번호이고 gen이 이 순서를 검사한다.

별칭은 덤프 원본이 다르게 읽는 기준 상태(호박 방향 등)다. `State::read`는 정확한 조회 뒤 별칭 표를 본다. 그래서 가져온
월드, 디스크 팔레트, 아이템 블록 NBT가 덤프 원본과 같은 상태가 된다. 보내는 상태는 바뀌지 않는다.

gen이 데이터 오류에서 실패하는 것은 데이터 오류가 실행 중 조용한 오동작이 되지 않게 하기 위해서다.

## 알 수 없는 블록과 비활성 블록

- 모르는 상태의 `{name, states}` 해시가 기준 목록의 상태(Anchor 상태도 별칭도 아닌 것)이고 이름도 같으면 비활성
  블록(`block::Inert`)이다. 덤프에 없는 블록을 구현하는 것이 아니라 원래 블록을 그대로 보여 주는 것이다.
- 판정은 이분 검색이다. 저장 형식은 그대로라 이미 가져온 월드도 다시 가져오지 않고 원래 블록으로 보인다.
- 서버가 아는 것은 공식 서버 값의 충돌 상자, 빛(방출, 차단; 차단이 있으면 하늘빛도 막음), 굳기뿐이다. 동작(흐름, 성장,
  랜덤 틱)은 없다.
- 맞는 도구는 데이터에 없다.
- 놓기 대역 때문에 블록은 클릭한 면 옆 칸에 놓인다. 해초·켈프 칸의 대체 여부는 데이터에 없어 원본 보존 쪽을 골랐다.
- 남은 한계: 폭발에 부서지지 않는다. 엔티티는 켈프·해초 칸을 물로 보지 않는다.
- 모르는 화분 식물이 비활성 블록이 아니면 빈 화분으로 보인다. 꺼내기, 바꿔 넣기, 드롭은 없다. 덤프 원본은 이 타일을
  읽지 못하고, 모르는 배너 무늬 ID는 버린다.
- 가져온 월드에는 덤프에 없는 바닐라 아이템이 흔하다.

## 업그레이드 스키마와 옛 데이터 (`xtask/data/upgrade/`)

월드 가져오기(`storage.md`)는 옛 블록 상태와 아이템 이름을 아래 스키마로 올린다.

| 파일 | 내용 |
|---|---|
| `blocks/` | 블록 스키마 37개(`nbt_upgrade_schema/`) |
| `items/` | 아이템 스키마와 레거시 ID 표 |
| `block_legacy_id_map.json`, `legacy_1.12.0.jsonl` | 레거시 숫자 ID와 1.12.0 `id_meta_to_nbt` 표(바이너리를 타입 있는 JSON 줄로 변환) |
| `golden/` | 덤프 원본 업그레이더 실행 결과(`legacy.jsonl`, `states.jsonl`, `reference.jsonl`) |
| `sources.json` | 원본 커밋과 원본 파일마다의 SHA-256 |

생성 표는 블록 스키마 37개, 레거시 항목 3,311개(상태 2,812개로 올라감), 아이템 스키마 30개다.
`BLOCK_STATE_VERSION`은 18,168,865(= 1.21.60.33)이다.

스키마 저장소와 덤프 원본 업그레이더의 결함:
- `0321`부터 모든 스키마가 같은 출력 버전(1.21.60.33)을 써서, 덤프 원본은 현재 상태에도 `0321`을 다시 적용해
  `red_mushroom_block` 10/15를 `mushroom_stem`으로 바꾼다.
- 한 스키마에서 같은 ID가 이름 바꾸기와 평탄화에 함께 있으면 덤프 원본은 실행 중 던진다. 덤프 원본은 byte 값을 부호
  있게 읽는다.
- 덤프 원본은 모르는 레거시 meta를 meta 0으로 바꾼다.
- 상태 버전이 가장 새 스키마보다 높으면 덤프 원본은 아무것도 적용하지 않아 1.26 속성이 없는 블록(울타리, 계단 등)이
  알 수 없는 블록이 된다. 가장 새 버전의 스키마는 없는 속성만 더하고 옛 이름만 바꾸므로, 그 스키마를 적용해도 이미
  가진 상태는 그대로다.
- 아이템 스키마의 대문자 ID는 PHP가 찾지 못한다.

gen이 규칙 해석기와 같은 코드로 스키마를 검사하므로 규칙이 숨기는 결함이 생성 때 드러난다. 골든은 덤프 원본의
업그레이더를 덤프용 PHP로 실행한 결과이고 가져오기 시험이 이와 비교한다.

## `.cache/` 다시 만들기

`.cache/`는 git이 무시한다. `.cache/`가 필요한 것은 `dump`, `p6`, `vendor-test`, dimpl 사본 시험, 일부 xtask
시험이다. 아래 sha256은 지금 `.cache/`의 파일 값이다.

| 폴더 | 원본 | 고정 | 읽는 명령 |
|---|---|---|---|
| `pmmp` | `https://github.com/pmmp/PocketMine-MP.git` | 출처 표의 커밋 + `vendor/`(`composer.lock`) | `dump`, PHP 스크립트 전부 |
| `dragonfly` | `https://github.com/HashimTheArab/dragonfly.git`(PR #1481의 포크) | 출처 표의 커밋 | `dump` |
| `cloudburst-data` | `https://github.com/CloudburstMC/Data.git` | 출처 표의 커밋 | `dump`, `p6`(바이옴·jigsaw 대조), `xtask/tests/spawn.rs`(없으면 건너뜀) |
| `bedrock-protocol-docs` | `https://github.com/Mojang/bedrock-protocol-docs.git` | `protocol.md` 고정 버전 표의 커밋 | `dump`(필드 상한) |
| `block-upgrade-schema` | `https://github.com/opencollab-incubator/BedrockBlockUpgradeSchema.git` | 출처 표의 커밋 | `dump` |
| `item-upgrade-schema` | `https://github.com/opencollab-incubator/BedrockItemUpgradeSchema.git` | 출처 표의 커밋 | `dump` |
| `php-pm5/bin/php/php.exe` | pmmp/PHP-Binaries 릴리스 php-build-524, PHP 8.4 PM5 Windows x64 | zip `cf4023b8…9be4`(위 표), `php.exe` `80625be7d15a3f156bb5ae5f8fb454bcfbec043d800022d70fa59333512f484c`(PHP 8.4.16 ZTS) | `dump`, PHP 스크립트 |
| `bds/1.26.52.3/` | minecraft.net Bedrock Dedicated Server, Windows 1.26.52.3 | `bedrock_server.exe` `19c88569af2e4b7d984e999055a31cbcb0799dacf8bbbf7371eda42f5772a443` | `p6 bds`, `p6 game bds`, `p6 api bds`, `p6 import` |
| `bds/1.26.52.3/`(Linux 체크아웃) | minecraft.net Bedrock Dedicated Server, Linux 1.26.52.3 (`bin-linux/bedrock-server-1.26.52.3.zip`) | zip `f6348d84fa714d04ca194f207e89453ca6bba0a1359396475271a52a150471c6`, `bedrock_server` `0a490c711d4a2ce075debcd979e8862b7eaba008373f9970f0edbb67f98f9207` | Linux의 `p6` 전부 |
| `tools/strawberry-perl-5.42.3.1/` | StrawberryPerl GitHub 릴리스 SP_54231_64bit, portable zip | zip `6a081a811781c30aca51dbc036afd93092af91e3297901f02c17043795a10690`, `perl/bin/perl.exe` `d4fa3a6ebe78f5f60e185cf7d7809a7dd8c541739ca4a7bc8687c75548473f42` | `verify`의 dimpl 사본 시험(OpenSSL 소스 빌드) |
| `str0m-test-data/` | `https://raw.githubusercontent.com/algesten/str0m/<커밋>/tests/data/<파일>` | `vendor/str0m/.cargo_vcs_info.json`의 커밋, 파일마다 `xtask/data/vendor/str0m-test-data.sha256` | `vendor-test str0m`(없으면 스스로 받음) |

- `p6`는 공식 서버 폴더에 `server.properties`와 `worlds`, 폼 시험 팩(`development_behavior_packs`)을 스스로 쓴다.
- `vendor-test str0m`은 있는 파일도 매번 sha256을 확인한다.
- Go(`xtask/go`, `go.mod`가 모듈 판을 고정)는 `dump`, `p6`, `gen`의 추출기에 필요하고 PATH에 있어야 한다.
