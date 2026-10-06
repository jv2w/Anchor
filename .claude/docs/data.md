# 블록과 아이템 데이터

Anchor의 블록, 아이템, 레시피, 크리에이티브 목록, 효과, 인챈트 표는 손으로 쓰지 않는다. 고정한 외부 출처를
한 번 실행하거나 변환해 `xtask/data/`에 커밋하고, `cargo xtask gen`이 그 데이터로 `src/` 아래 생성 표를 만든다.

- 종류, 상태, 직렬화 규칙, 동작은 PMMP에서 온다. PMMP에 없는 블록과 아이템은 채우지 않는다.
- 버전별 번호(블록 네트워크 ID, 아이템 런타임 ID)는 프로토콜 기준 목록에서 온다. 기준 목록은 검증과 번호에만
  쓰고, 블록을 만들지 않는다.
- 두 출처가 다르면 BDS가 실제로 보내는 값을 따른다(BDS 대조는 `testing.md`).
- PMMP 소스를 파싱하지 않는다. PMMP 레지스트리를 PMMP용 PHP로 실행해 그 결과를 쓴다. 그래서 데이터는 PMMP가
  실제로 돌리는 값과 같다.

## 출처와 고정 판

| 출처 | 고정 | 라이선스 | 쓰는 것 |
|---|---|---|---|
| pmmp/PocketMine-MP | 마지막 커밋 `6a7cc02e9dff59b69241aa0bcffdb9903ce86beb`(5.44.3 뒤, Bedrock 1.26.30 데이터) | LGPL-3.0, 보관됨 | 블록·아이템 종류와 동작, 레시피, 크리에이티브 목록, 효과·인챈트 표, 골든 |
| pocketmine/bedrock-data | 6.7.0+bedrock-1.26.30(PMMP `composer.lock`) | CC0 | PMMP가 읽는 레시피 등 |
| pmmp/PHP-Binaries | php-build-524, PHP 8.4 PM5 Windows x64(zip sha256 `cf4023b8549c99eec47cfd403805ac7d78100fa366394dab4e45bb544dcc9be4`) | PHP 라이선스 | PMMP 실행(개발 도구) |
| df-mc/dragonfly | v0.11.5 `4c7b5074be94fa83a1cd98e9c752083ad04a6e21`(gophertunnel v1.62.0, 1.26.50) | MIT | 기준 목록(블록 상태, 아이템, 크리에이티브 아이템 상태) |
| CloudburstMC/Data | `659ce1e2eee3a67045693f4fd5515c6ccf571953`(26.50) | Apache-2.0 | 블록 네트워크 ID, 교차 확인, 바이옴, BDS 블록 물리, 그대로 보내는 파일 |
| Mojang/bedrock-protocol-docs | v1.26.50 `475bd72ed89036af4eb18426774ef3b953de7603`(프로토콜 2193) | Minecraft EULA | 필드 길이 상한(숫자만 옮김) |
| pmmp/BedrockBlockUpgradeSchema | 5.3.0 `4e874a162c4d41a9db7df72980e97849d6942d5d` | CC0 | 옛 블록 상태 업그레이드 |
| pmmp/BedrockItemUpgradeSchema | 1.18.0 `7f551f625fee3be0c8d33be1fee295c7ec6a277e` | CC0 | 옛 아이템 이름 업그레이드 |
| BDS | 1.26.52.3(프로토콜 2193) | Minecraft EULA | 대조 실행만. 카메라·조준 보조 프리셋은 BDS가 보낸 패킷 캡처에서 옮김 |

- `cargo xtask dump`는 시작할 때 각 체크아웃이 위 커밋인지 확인하고, 아니면 실패한다.
- 블록 스키마는 PMMP가 고정한 5.2.0이 아니라 5.3.0이다. 5.2.0에는 1.26.30 변경이 없다.
- 아이템 스키마는 PMMP가 고정한 1.17.0이 아니라 1.18.0이다. 1.26.50의 이름 바꾸기(`photo` → `photo_item`)가 있다.
- Mojang 문서는 숫자만 옮기고 스키마 파일은 복사하지 않는다.

## PMMP 덤프 (`cargo xtask dump`)

순서는 다음과 같다.
1. `xtask/go/reference`(Go)가 dragonfly와 CloudburstMC를 읽어 `xtask/data/reference/`에 기준 목록을 쓴다.
   PMMP 실행이 기준 상태를 PMMP 역직렬화기로 읽어야 하므로 먼저 한다.
2. PM5 PHP가 `.cache/pmmp`를 작업 폴더로 `xtask/php/dump.php`를 돌려 `xtask/data/pmmp/`에 쓴다.
3. CloudburstMC의 바이옴, 엔티티 식별자, 블록 모양, 블록 물리를 `xtask/data/reference/`로 옮긴다.
4. Mojang 프로토콜 스키마에서 필드 상한(`protocol_limits.json`)을 만든다(쓰는 법은 `protocol.md`).
5. 업그레이드 스키마를 `xtask/data/upgrade/`로 옮기고 PMMP 업그레이더로 골든을 만든다(아래 업그레이드 절).

`--check`는 같은 일을 `target/xtask-dump/`에 하고 커밋된 파일과 바이트가 다르면 실패한다. 덤프는 `verify`에
들지 않는다. 체크아웃, PM5 PHP, Go가 필요하고, 입력은 고정 커밋이라 바뀌지 않는다. 대신 `verify`의
`gen --check`가 커밋된 데이터에서 만든 코드를 검사한다.

일반 PHP에는 PMMP가 쓰는 확장(`pmmp\encoding` 등)이 없어 레지스트리를 띄우지 못한다. 그래서 pmmp/PHP-Binaries의
PM5 빌드를 개발 도구로 쓴다. PMMP의 PHP 의존성은 PMMP `composer.lock` 그대로 `.cache/pmmp/vendor`에 둔다.

### PHP 스크립트

| 스크립트 | 실행 | 출력 |
|---|---|---|
| `xtask/php/dump.php` | `dump`가 실행 | `xtask/data/pmmp/` 아래 `blocks.json`, `deserialized.json`, `items.json`, `creative.json`, `recipes.json`, `tables.json` |
| `xtask/php/upgrade.php` | `dump`가 실행 | `xtask/data/upgrade/golden/` 세 파일 |
| `xtask/php/place.php` | 손으로(`php place.php <출력>`) | `xtask/data/pmmp/place.json`: PMMP 놓기 결과 골든 |
| `xtask/php/liquid.php` | 손으로 | `xtask/data/pmmp/liquid.txt`: 액체 흐름 장면 골든 |
| `xtask/php/api.php` | 손으로 | `xtask/data/pmmp/api.json`: PMMP 공개 API 목록(`cargo xtask table`이 대응표를 만듦) |

모든 스크립트는 `.cache/pmmp`를 작업 폴더로 PM5 PHP에서 돈다. 출력은 정렬하고 한 줄에 한 항목이라 같은 체크아웃은
같은 바이트를 낸다. NBT는 타입 있는 JSON(`{"byte": 1}`, 실수는 비트 패턴)이다.

### 덤프 내용

- `blocks.json`: PMMP 상태마다 Bedrock 직렬화, 빛 방출과 필터, 직사광 차단, 랜덤 틱, 정확한 블록 레시피 재료의 레거시
  meta, PMMP 클래스·부모·트레이트, 부수기 정보, 불 값, 물리 플래그, 마찰, 타입 태그, 6면 지지, 충돌 상자, 아이템 형태,
  결정적 드롭과 경험치, 이웃으로 상태가 정해지는 블록(울타리, 판유리, 벽, 울타리 문, 계단, 트립와이어)의 변형별 값.
- `deserialized.json`: 기준 목록 상태마다 PMMP 역직렬화 결과가 다르면 그 결과(PMMP가 거부하면 null). PMMP는 바닐라
  속성 일부(호박 방향, 매단 간판의 쓰지 않는 방향 등)를 무시하고 한 값으로 쓴다.
- `items.json`: PMMP 아이템 형태마다 Bedrock 직렬화, PMMP 이름과 별칭, 스택·내구도·도구·갑옷·음식·연료·인챈트·쿨다운 값.
- `creative.json`: 크리에이티브 목록(순서, 분류, 그룹 그대로).
- `recipes.json`: PMMP가 bedrock-data에서 읽는 레시피. PMMP가 읽지 않는 smithing 레시피는 PMMP 아이템 해석기로 읽어
  더한다. 모르는 아이템이 든 레시피는 PMMP처럼 뺀다.
- `tables.json`: 효과, 물약, 인챈트, 아이템 태그, 배너 무늬, 화로 계열 표.

### 덤프와 gen이 바로잡는 PMMP 결함

- 모르는 인챈트를 버려 생기는 빈 인챈트 책은 크리에이티브 목록에서 뺀다(1,694 → 1,633).
- 불길한 배너: PMMP는 검은색(meta 0)이고 BDS는 흰색(meta 15)이다. 덤프의 모든 아이템이 거치는 한 곳
  (`xtask/src/gen/data.rs`)에서 BDS 값으로 바꾼다.
- 블록 아이템의 네트워크 형태: PMMP 기본 상태(계단 방향 등)가 BDS와 다르다. 이름에 PMMP 블록 아이템과 바닐라 상태가
  하나씩일 때 바닐라 크리에이티브 상태(`item_blocks.json`)로 보낸다. 아이템이 놓는 상태는 그대로 PMMP 상태다.
- 크리에이티브 아이템 네트워크 ID는 BDS처럼 1부터 센다.

## 기준 목록 (`xtask/data/reference/`)

| 파일 | 출처 | 쓰임 |
|---|---|---|
| `blocks.json` | dragonfly `block_states.nbt` + CloudburstMC `block_palette.nbt`의 `network_id` | 블록 상태 22,091개와 네트워크 ID |
| `items.json` | dragonfly `vanilla_items.nbt`, CloudburstMC `runtime_item_states.json`과 대조 | 아이템 2,076개(런타임 ID 순, 컴포넌트 데이터 포함). ItemRegistry 전체 |
| `item_blocks.json` | CloudburstMC `creative_items.json`, dragonfly `creative_items.nbt`와 대조 | 블록 아이템의 네트워크 형태 |
| `block_physics.json` | CloudburstMC `blocks.json`(BDS 덤프) | 비활성 블록의 굳기, 빛, 충돌 상자 |
| `biomes.json` | CloudburstMC `stripped_biome_definitions.json` | BiomeDefinitionList(89개) |
| `entity_identifiers.dat`, `voxel_shapes.json` | CloudburstMC 그대로 | AvailableActorIdentifiers, VoxelShapes |
| `jigsaw_structure_data.dat`, `entity_properties.dat` | CloudburstMC `.nbt`를 네트워크 NBT로 변환 | 스폰 때 보내는 데이터 |
| `protocol_limits.json` | Mojang 프로토콜 스키마 | 디코더 필드 상한 |
| `bds_camera.json` | BDS 스폰 캡처 | 카메라·조준 보조 프리셋 |

- dragonfly와 CloudburstMC의 블록 상태(이름, 속성 이름·타입·값, 버전)와 아이템(이름, 런타임 ID, 버전, 컴포넌트
  여부)은 완전히 같아야 한다. 하나라도 다르면 변환 도구가 실패한다.
- 블록 네트워크 ID는 해시 방식(FNV-1a 32비트, 이름과 정렬한 상태 NBT)이다. BDS도 해시 ID를 쓴다. 기준 목록
  전체가 Rust 해시 함수 결과와 같은지 골든 시험한다. 표의 ID는 기준 목록 값이고, 해시 함수는 커스텀 블록에 쓴다.
- 바이옴은 생성 데이터가 없는 판만 쓴다. 패킷 필드가 없는 값(밀도 등)이 0이 아니면 덤프가 실패한다.
- PMMP 상태 대부분은 기준 목록과 1:1이 아니다. 같은 Bedrock 상태로 가는 PMMP 상태의 차이(침대 색, 배너 색 등)는
  블록 엔티티에 둔다. 허용하는 겹침은 `gen/data.rs`의 `SHARED`(침대, 물 가마솥, 배너, 벽 배너)뿐이다.
- light dampening은 PMMP 빛 필터와 일부(잎, 발광석 등) 다르다. 그래서 비활성 블록에만 쓴다. BDS `isSolid`는 PMMP와
  뜻이 달라 쓰지 않고, 단단함은 충돌 상자로 정한다.
- BDS는 StartGame에 data-driven 블록 정의를 보낸다(색깔별 양털·콘크리트 계단과 반 블록 등). 모두 PMMP에 없는
  블록이라 Anchor는 보내지 않는다.

## 생성 표 (`cargo xtask gen`)

| 생성 파일 | 입력 | 내용 |
|---|---|---|
| `src/block/table.rs` | PMMP 덤프, 업그레이드 스키마 `0351`, 기준 목록 | Anchor 상태 12,468개와 별칭 4,884개 |
| `src/block/inert/table.rs` | 기준 목록, `block_physics.json` | 비활성 상태 4,739개(이름 261개)와 물리 |
| `src/block/drops.rs` | PMMP 덤프 | 결정적 드롭 골든 |
| `src/item/table.rs` | PMMP 덤프, 기준 목록 | PMMP 아이템 형태 1,613개 |
| `src/item/registry.rs` | 기준 목록 | ItemRegistry 2,076개 |
| `src/item/creative.rs` | PMMP 덤프 | 크리에이티브 107그룹, 1,633개 |
| `src/crafting/table.rs` | PMMP 덤프 | 모양·무형·화로·물약·smithing 레시피 |
| `src/effect/table.rs` | `tables.json` | 효과 27개, 물약 43개, 인챈트 |
| `src/world/biome/table.rs` | `biomes.json` | 바이옴 정의 |
| `src/import/upgrade/table.rs` | `xtask/data/upgrade/` | 가져오기 업그레이드 표 |

생성 파일 첫 줄은 `// @generated by cargo xtask gen; do not edit.`이다. 고칠 일이 있으면 생성기(`xtask/src/gen/`)를
고친다. 모든 표는 `static`이라 시작 비용과 할당이 없다.

Anchor 블록 상태는 PMMP 상태의 Bedrock 직렬화(중복 제거)에 최신 업그레이드 스키마를 적용한 것과, 그 스키마가 더한
속성의 나머지 값을 합친 것이다. 1.26.50은 PMMP 블록 115개 이름에 속성을 더했다(울타리·판유리·철창·트립와이어의
`minecraft:connection_*`, 계단의 `minecraft:corner`). BDS는 이 값을 서버에서 이웃으로 계산해 보내므로 Anchor도
서버에서 계산하고, 상태 표는 그 모든 값을 담는다. 순서는 (이름, 상태의 정규 JSON)이고 `BlockState`는 그 번호다.

별칭은 PMMP가 다르게 읽는 기준 상태(호박 방향 등)다. `State::read`는 정확한 조회 뒤 별칭 표를 본다. 그래서 가져온
월드, 디스크 팔레트, 아이템 블록 NBT가 PMMP와 같은 상태가 된다. 보내는 상태는 바뀌지 않는다.

gen은 다음 중 하나라도 있으면 실패한다. 데이터 오류가 실행 중 조용한 오동작이 되지 않게 하기 위해서다.
- 업그레이드 뒤 기준 목록에 없는 상태, 쓰이지 않는 업그레이드 항목
- 같은 Bedrock 상태로 가는 PMMP 상태끼리 값 충돌(`SHARED` 밖), 필요 없는 `SHARED` 항목
- 기준 목록에 없는 아이템 이름, 풀리지 않는 레시피·크리에이티브 아이템
- PMMP가 읽는 기준 상태에 Anchor 상태가 없음
- 블록 상태 버전이 `blocks.json`과 마지막 스키마에서 다름

## 알 수 없는 블록과 비활성 블록

- 저장된 블록 상태를 Anchor가 모르면 저장 원본(NBT)을 그대로 보존한다. 클라이언트가 보낸 알 수 없는 블록이나
  아이템은 그 요청만 거부한다.
- 모르는 상태의 `{name, states}` 해시가 기준 목록의 상태(Anchor 상태도 별칭도 아닌 것)이고 이름도 같으면 비활성
  블록(`block::Inert`)이다. 클라이언트에는 그 상태의 해시 ID, 즉 BDS가 보내는 값 그대로 보낸다. PMMP에 없는 블록을
  구현하는 것이 아니라 원래 블록을 그대로 보여 주는 것이다. 그 밖의 모르는 상태는 `minecraft:info_update`로 보낸다.
- 판정은 청크를 읽을 때 한 번(이분 검색)이고 팔레트 항목 `Entry::Inert`에 담는다. 틱마다 다시 하지 않는다. 저장 형식은
  그대로라 이미 가져온 월드도 다시 가져오지 않고 원래 블록으로 보인다.
- 서버가 아는 것은 BDS 값의 충돌 상자, 빛(방출, 차단; 차단이 있으면 하늘빛도 막음), 굳기뿐이다. 동작은 없다(흐름,
  성장, 랜덤 틱 없음). 이웃 규칙과 액체에는 기반암으로 읽힌다.
- 부수기 시간은 굳기 × 1.5 ÷ 들고 있는 도구 효율이다. 맞는 도구를 모르므로 들고 있는 도구를 맞는 도구로 보아 서버
  허용 시간이 클라이언트보다 길지 않게 한다. 부수면 공기가 되고 블록 엔티티가 지워지며 드롭은 없다.
- 놓기 규칙은 비활성 블록을 대역으로 읽는다. 칸 전체 충돌 상자 하나면 기반암(모든 면 완전 지지), 아니면 거미줄(지지와
  충돌 없음)이고, 둘 다 대체되지 않는다. 그래서 블록은 클릭한 면 옆 칸에 놓인다. 해초·켈프 칸의 대체 여부는 데이터에
  없어 대체하지 않는다(원본 보존 쪽).
- 이벤트는 Anchor 상태만 담는다. `BlockBreak`와 오른쪽 클릭 `PlayerInteract`는 `block::INFO_UPDATE`를 대신 이름으로
  보낸다(취소하면 블록이 남고 원래 블록으로 다시 보낸다). 왼쪽 클릭 `PlayerInteract`와 파티클 이벤트는 없다.
- 남은 한계: 폭발에 부서지지 않는다. 엔티티는 켈프·해초 칸을 물로 보지 않는다.
- 블록 엔티티 안의 블록과 무늬도 원본을 보존한다. 화분 식물(`PlantBlock`)을 Anchor가 모르면 원본 그대로 두고,
  비활성 블록이면 클라이언트에 그대로 보낸다(그 밖은 빈 화분으로 보임). 꺼내기, 바꿔 넣기, 드롭은 없다. PMMP는 이
  타일을 읽지 못한다. 모르는 배너 무늬 ID도 그대로 두고 보낸다(PMMP는 버림). 공개 API는 아는 것만 보인다.
- 블록 엔티티 안의 아이템은 Anchor가 모르면 PMMP처럼 빠진다. 가져온 월드에는 PMMP에 없는 바닐라 아이템이 흔하므로
  info 로그로 남긴다(슬롯 번호가 틀린 아이템은 warn).

## 업그레이드 스키마와 옛 데이터 (`xtask/data/upgrade/`)

월드 가져오기(`storage.md`)는 옛 블록 상태와 아이템 이름을 PMMP처럼 올린다.

| 파일 | 내용 |
|---|---|
| `blocks/` | 블록 스키마 36개(`nbt_upgrade_schema/`와 루트의 `0351_1.26.40_to_1.26.50.json`) |
| `items/` | 아이템 스키마와 레거시 ID 표 |
| `block_legacy_id_map.json`, `legacy_1.12.0.jsonl` | 레거시 숫자 ID와 1.12.0 `id_meta_to_nbt` 표(바이너리를 타입 있는 JSON 줄로 변환) |
| `golden/` | PMMP 업그레이더 실행 결과(`legacy.jsonl`, `states.jsonl`, `reference.jsonl`) |
| `sources.json` | 원본 커밋과 원본 파일마다의 SHA-256 |

생성 표는 블록 스키마 36개, 레거시 항목 3,311개(상태 2,812개로 올라감), 아이템 스키마 29개다. 블록 상태 버전은
생성 상수 `BLOCK_STATE_VERSION`(18,168,865 = 1.21.60.33) 하나다.

스키마 저장소의 결함과 조치:
- `0351`이 `nbt_upgrade_schema/`가 아니라 저장소 루트에 있어 PMMP 스캔에서 빠진다. 덤프가 따로 읽어 다른 스키마
  옆에 둔다. 이 스키마는 Anchor 상태 표의 1.26.50 속성 출처이기도 하다.
- `0321`부터 모든 스키마가 같은 출력 버전(1.21.60.33)을 써서, PMMP는 현재 상태에도 `0321`을 다시 적용해
  `red_mushroom_block` 10/15를 `mushroom_stem`으로 바꾼다. Anchor는 상태 버전 묶음의 첫 스키마를 이미 적용된 것으로
  건너뛰고, gen이 기준 상태 22,091개가 모두 그대로 돌아오는지 확인한다.
- 한 스키마에서 같은 ID가 이름 바꾸기와 평탄화에 함께 있으면 PMMP는 실행 중 던진다. gen이 거부한다. 순서에 따라
  결과가 달라지는 속성 이름 바꾸기와 127보다 큰 byte 값(PMMP는 부호 있게 읽음)도 gen이 거부한다.
- PMMP는 모르는 레거시 meta를 meta 0으로 바꾼다. Anchor는 원본 `{id, val}`을 알 수 없는 블록으로 보존한다.
- 상태 버전이 가장 새 스키마보다 높으면 PMMP는 아무것도 적용하지 않아 1.26 속성이 없는 블록(울타리, 계단 등)이
  알 수 없는 블록이 된다. Anchor는 가장 새 버전의 상태로 보고 그 버전의 스키마를 적용한다. 그 스키마는 없는 속성만
  더하고 옛 이름만 바꾸므로 이미 가진 상태는 그대로다.
- 아이템 스키마: 대문자 ID는 PHP가 찾지 못한다. gen이 소문자 ID, 숫자 meta, 1:1 레거시 ID를 검사한다. CRLF는 LF로 바꾼다.

규칙 해석기(`src/import/upgrade/apply.rs`)는 gen과 공유한다. gen이 같은 코드로 스키마를 검사하므로 규칙이 숨기는
결함이 생성 때 드러난다. 골든은 PMMP 업그레이더를 PM5 PHP로 실행한 결과이고 가져오기 시험이 이와 비교한다.

## `.cache/` 다시 만들기

`.cache/`는 git이 무시한다. 커밋된 데이터만으로 빌드, 시험, `verify`가 돈다(dimpl 사본 시험의 Perl만 예외).
`.cache/`가 필요한 것은 `dump`, `p6`, `vendor-test`, dimpl 사본 시험, 일부 xtask 시험이다. 아래 sha256은 지금
`.cache/`의 파일 값이다.

| 폴더 | 원본 | 고정 | 읽는 명령 |
|---|---|---|---|
| `pmmp` | `https://github.com/pmmp/PocketMine-MP.git` | `6a7cc02e9dff59b69241aa0bcffdb9903ce86beb` + `vendor/`(`composer.lock`) | `dump`, PHP 스크립트 전부 |
| `dragonfly` | `https://github.com/df-mc/dragonfly.git` | `4c7b5074be94fa83a1cd98e9c752083ad04a6e21` | `dump` |
| `cloudburst-data` | `https://github.com/CloudburstMC/Data.git` | `659ce1e2eee3a67045693f4fd5515c6ccf571953` | `dump`, `p6`(바이옴·jigsaw 대조), `xtask/tests/spawn.rs`(없으면 건너뜀) |
| `bedrock-protocol-docs` | `https://github.com/Mojang/bedrock-protocol-docs.git` | `475bd72ed89036af4eb18426774ef3b953de7603` | `dump`(필드 상한) |
| `block-upgrade-schema` | `https://github.com/pmmp/BedrockBlockUpgradeSchema.git` | `4e874a162c4d41a9db7df72980e97849d6942d5d` | `dump` |
| `item-upgrade-schema` | `https://github.com/pmmp/BedrockItemUpgradeSchema.git` | `7f551f625fee3be0c8d33be1fee295c7ec6a277e` | `dump` |
| `php-pm5/bin/php/php.exe` | pmmp/PHP-Binaries 릴리스 php-build-524, PHP 8.4 PM5 Windows x64 | zip `cf4023b8…9be4`(위 표), `php.exe` `80625be7d15a3f156bb5ae5f8fb454bcfbec043d800022d70fa59333512f484c`(PHP 8.4.16 ZTS) | `dump`, PHP 스크립트 |
| `bds/1.26.52.3/` | minecraft.net Bedrock Dedicated Server, Windows 1.26.52.3 | `bedrock_server.exe` `19c88569af2e4b7d984e999055a31cbcb0799dacf8bbbf7371eda42f5772a443` | `p6 bds`, `p6 game bds`, `p6 api bds`, `p6 import` |
| `tools/strawberry-perl-5.42.3.1/` | StrawberryPerl GitHub 릴리스 SP_54231_64bit, portable zip | zip `6a081a811781c30aca51dbc036afd93092af91e3297901f02c17043795a10690`, `perl/bin/perl.exe` `d4fa3a6ebe78f5f60e185cf7d7809a7dd8c541739ca4a7bc8687c75548473f42` | `verify`의 dimpl 사본 시험(OpenSSL 소스 빌드) |
| `str0m-test-data/` | `https://raw.githubusercontent.com/algesten/str0m/<커밋>/tests/data/<파일>` | `vendor/str0m/.cargo_vcs_info.json`의 커밋, 파일마다 `xtask/data/vendor/str0m-test-data.sha256` | `vendor-test str0m`(없으면 스스로 받음) |

다시 만드는 순서(저장소 루트에서):
1. git 체크아웃 여섯 개: 폴더마다 `git init .cache/<폴더>`, `git -C .cache/<폴더> remote add origin <원본>`,
   `git -C .cache/<폴더> fetch --depth 1 origin <커밋>`, `git -C .cache/<폴더> checkout --detach FETCH_HEAD`.
   `dump`는 `HEAD`가 고정 커밋인지 확인한다.
2. PM5 PHP: 릴리스 zip을 받아 sha256을 확인하고 `.cache/php-pm5/`에 푼다(`bin/php/php.exe`와 `bin/php/php.ini`가
   그 자리에 있어야 한다).
3. PMMP 의존성: `.cache/pmmp`에서 PM5 PHP로 composer를 돌려 `composer install --no-dev --ignore-platform-reqs`.
   `vendor/`가 없으면 `dump`가 이 명령을 알려 주며 멈춘다.
4. BDS: 받은 zip을 `.cache/bds/1.26.52.3/`에 푼다. 원본 설치본은 실행하지 않고 이 복사본만 쓴다. `p6`가
   `server.properties`와 `worlds`, 폼 시험 팩(`development_behavior_packs`)을 스스로 쓴다.
5. Strawberry Perl: portable zip을 받아 sha256을 확인하고 `.cache/tools/strawberry-perl-5.42.3.1/`에 푼다.
6. str0m 캡처: `cargo xtask vendor-test str0m`이 없는 파일을 받고, 있는 파일도 매번 sha256을 확인한다.
7. 확인: `cargo xtask dump --check`가 통과하면 복구한 입력이 커밋된 데이터를 바이트 그대로 다시 만든 것이다.

Go(`xtask/go`, `go.mod`가 모듈 판을 고정)는 `dump`, `p6`, `gen`의 추출기에 필요하고 PATH에 있어야 한다.
