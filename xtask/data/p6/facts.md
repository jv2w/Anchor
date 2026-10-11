# P6 사실 (공식 서버 1.26.52.3)

공식 서버 1.26.52.3(프로토콜 2193)이 봇에게 보내는 값과, Anchor가 각 값에 대해 하는 일이다. 값은 같은 시나리오를 두 번 이상 돌려 같게 나온 것만 적는다. 실행마다 다른 값은 "무작위"라고 적는다. 시나리오와 비교 방법은 `.claude/docs/testing.md`에 있다.

## 캡처 조건
- 게임 시나리오: `cargo xtask p6 game bds [craft|furnace|enchant|crops|trade]...` → `target/p6/bds-game/`. RakNet, zlib, 새 평지 월드.
- API 시나리오: `cargo xtask p6 api bds [시나리오]...` → `target/p6/bds-api/`(시나리오마다 `<시나리오>.txt` 보고). 같은 조건에 `allow-cheats=true`와 폼 스크립트 팩(`xtask/data/p6/forms-pack`)을 더한다.
- 스폰 창: `cargo xtask p6 bds` → `target/p6/bds-<압축>/`.
- 블록 시나리오(`p6 bds`·`p6 anchor`의 RakNet 실행, `blocks.txt`): 봇이 스폰한 뒤 첫 단계 전에 콘솔 `gamerule randomtickspeed 0`으로 랜덤 틱을 끈다(Anchor는 단계 작업이 `Context::set_random_tick_speed(0)`). 켜 두면 놓은 블록이 단계 사이에 스스로 바뀐다: Anchor(기본 3)에서 구리 창살이 `exposed_copper_bars`로 산화해 그 단계 상태가 공식 서버 캡처와 달랐다. 공식 서버도 기본 1로 랜덤 틱이 돌아 같은 일이 날 수 있다.
- 세션 시작 때 콘솔: `gamerule domobspawning false`, `gamerule randomtickspeed 0`, `gamerule dodaylightcycle false`, `gamerule doweathercycle false`, `kill @e[type=!player]`. 난이도는 normal.
- 시나리오마다 새 플레이어(`P6Craft` 등)가 자기 구역으로 `tp`한 뒤 논다. 좌표가 절대값이라 두 서버의 같은 평지를 그대로 비교한다.
- 월드 준비는 콘솔 명령이다: `tp`, `give`, `setblock`, `fill`, `xp`, `summon villager`, `effect`, `kill`. Anchor 쪽은 하네스(`xtask/src/p6/game/console.rs`)가 같은 명령을 공개 API로 옮긴다.

## 로딩 화면과 요청 ID
- 클라이언트가 `ServerBoundLoadingScreen`(Start, End)을 보내기 전에는 공식 서버가 블록 클릭(UseItem ClickBlock)을 모두 거부한다(주변 블록과 플레이어 상태를 다시 보냄). 봇은 초기화 직후 둘을 보낸다.
  - Anchor: 이 패킷을 받고 버린다. 거부 조건으로 쓰지 않는다.
- 요청 ID: 클라이언트처럼 -1부터 2씩 줄여도 공식 서버는 그대로 받는다. 같은 요청 안에서 바뀐 칸(만든 결과 칸 60:50, 같은 요청에서 비운 칸)의 StackNetworkID는 요청 ID다.

## 인벤토리 요청 (ItemStackRequest)
- 컨테이너 번호: 창이 열려 있으면 인벤토리 12(CombinedHotBarAndInventory), 커서 59, 제작 칸 13(2x2 28~31, 3x3 32~40), 만든 결과 60 칸 50, 화로 재료 25:0·연료 24:1·결과 26:2, 인챈트 입력 22:14·청금석 23:15, 거래 첫 재료 47:4. 응답에는 60이 나오지 않는다.
- 응답 모양: `Status` 0, 액션 순서대로 바뀐 칸(출발 → 도착, 소비 칸 → 결과 칸). 칸마다 `Slot` = `HotbarSlot`, 개수, StackNetworkID(0이면 빈 칸), `CustomName` "", `FilteredCustomName` 없음, `DurabilityCorrection` 0.
- 스택 네트워크 ID(공식 서버): 서버 전체 카운터 하나. 요청으로 스택 전체를 옮기면 ID가 따라간다. 일부를 옮기면 빈 도착 칸은 새 ID, 출발 칸은 그대로다. 개수만 바뀐 칸과 합쳐진 칸은 ID를 유지한다. 만든 결과, 인챈트 결과, 거래 결과는 새 ID다. 요청 밖의 변화(give, 아이템 사용으로 줄어듦, 줍기)는 새 ID다.
- 창 ID: 플레이어마다 2부터 열 때마다 1씩 는다(자기 인벤토리 창, 거래 창도 같은 카운터). 닫기는 클라이언트 ContainerClose(창, 열린 종류, false) → 공식 서버 ContainerClose(같은 창, 종류 247(= -9 NONE), ServerSide false).
- 블록 클릭 뒤 재동기화: 아이템을 쓰는 클릭(설치, 뼛가루)마다 공식 서버는 바뀐 인벤토리(InventoryContent 0·120·124·119, PlayerHotBar select false)와 블록 갱신(클릭 블록, 놓인 블록, 아래·위 블록) 뒤에 플레이어 전체 상태(UpdateAttributes 전체, SetActorData 전체, InventoryContent 4창, PlayerHotBar select true)와 놓인 블록을 한 번 더 보낸다. 클라이언트 예측 액션을 넣어도 같다.
- 콘솔 give: 공식 서버는 서버 → 클라이언트 InventoryTransaction(normal, 액션 둘: 컨테이너 창 0 칸 N 빈 칸 → 아이템 새 ID, 출처 3(creative) 칸 1 아이템 → 빈 칸), InventoryContent 4창, 소리 `pop`, Text(`commands.give.successRecipient`)를 보낸다.
- Anchor: 컨테이너 번호, 요청 ID 규칙, 요청 안의 스택 ID 규칙, 응답 칸 순서를 공식 서버와 같게 한다. 스택 ID 카운터는 플레이어마다다. 컨테이너가 자기 틱에서 개수만 바꾼 칸은 ID를 유지한다(공식 서버). 블록 클릭 뒤 전체 재동기화와 give의 InventoryTransaction은 따라 하지 않는다(비교는 결과 상태만 본다).

## 제작과 레시피 책
- 순서(`craft`): give 원목 4 → Interact(OpenInventory, 자기 런타임 ID) → ContainerOpen(창 2, 종류 255(-1 인벤토리), 위치 = 플레이어 눈 블록, 엔티티 -1) → 원목을 13:28에 Place → 판자 4번 → 판자를 28~31에 하나씩 → 작업대 → 막대(레시피 책) → ContainerClose → 작업대 설치 → 작업대 클릭 → ContainerOpen(창 3, 종류 1, 블록 위치) → 판자 32·33·34, 막대 36·39 → 곡괭이 → 닫기.
- 수동 제작 요청(상태 0): `[CraftRecipe(레시피 네트워크 ID, 1), CraftResultsDeprecated([결과], 1), Consume(1, 13:칸)…, Place(개수, 60:50 ID=요청 ID, 12:칸 ID 또는 0)]`. 응답은 소비한 제작 칸들과 도착 칸이다.
- 레시피 책(CraftRecipeAuto): 공식 서버가 받는 형태는 `[CraftRecipeAuto(ID, 1, 재료=[이름 디스크립터(oak_planks, meta 0) ×1, 같은 것 ×1]), CraftResultsDeprecated, Consume(1, 12:판자 칸) ×2, Place(4, 60:50, 12:빈 칸)]`. 레시피의 태그 디스크립터(`minecraft:planks` ×1 둘)를 그대로 보내면 상태 25(FailedToMatchExpectedAllowedAnywhereConsumedItem)다.
- 공식 서버는 자기 CraftingData의 레시피 네트워크 ID만 쓴다(1~4768, 3,312개, 빈 번호 있음). CraftResultsDeprecated의 결과 블록 런타임 ID를 돌로 바꿔도 받고, 설치할 때 HeldItem의 블록 런타임 ID를 돌로 바꿔도 작업대가 놓인다. 즉 공식 서버는 클라이언트가 보낸 아이템 형태를 검사하지 않고 서버 칸 내용과 스택 ID로 판정한다.
- 레시피 출력의 블록 아이템 형태: 작업대 출력 `BlockRuntimeID` 1752181952(크리에이티브 항목과 같은 바닐라 상태). 클라이언트는 이 형태를 예측해 들고 설치 요청에 쓴다.
- 작업대 설치: UseItem(ClickBlock, 바닥 블록 윗면 1, HeldItem=작업대, 예측 액션) → 재동기화, UpdateBlock 작업대 flags 3, 소리 `place`(ExtraData = 블록 런타임 ID).
- 해금: 게임 규칙 `recipesUnlock` true, `doLimitedCrafting` false. SetLocalPlayerAsInitialised 뒤 UnlockedRecipes(종류 1, 20개)가 온다. 20개는 문맥 1 레시피 전부(제작대 2개와 `deprecated` 화로 18개)다. 재료를 얻으면 종류 2로 새로 해금된 것을 보낸다(통나무를 받자 `minecraft:oak_planks` 등 3개, 판자를 만들자 16개).
- CraftingData의 `UnlockRequirement`: 모양 있는 1,217개 중 문맥 0(None)·재료 있음 1,022, 문맥 0·재료 없음 181, 문맥 2(물속) 11, 문맥 1(항상 해금) 2, 문맥 3(아이템 많음) 1. 모양 없는 2,068개 중 문맥 0·재료 있음 1,691(제작대, 석재 절단기, 화로 계열, 지도 제작대), 문맥 0·재료 없음 359, 문맥 1 18(블록 `deprecated`). 문맥 0·재료 없음 540개(예: `minecraft:barrel`, `minecraft:grindstone_from_crimson_planks2`, `minecraft:Torch_charcoal_recipeId`)는 재료로 해금되지 않고 서버의 UnlockedRecipes로만 해금된다.
- Anchor:
  - CraftResultsDeprecated는 무시한다. HeldItem은 네트워크 ID, 개수, 스택 ID로만 서버 칸과 맞추고 블록 런타임 ID 차이로 거부하지 않는다.
  - CraftRecipeAuto의 재료 목록은 믿지 않고 서버 레시피로 인벤토리에서 소비를 검사한다. 목록에는 상한 검사만 한다.
  - 게임 규칙은 공식 서버와 같게 보낸다. UnlockedRecipes는 보내지 않는다.
  - `recipe-book = true`(기본): 모든 레시피를 항상 해금으로 보낸다.
  - `recipe-book = false`: 모든 레시피를 문맥 0·재료 없음(공식 서버가 서버만 해금하는 레시피에 주는 모양)으로 보낸다. 클라이언트가 스스로 해금할 것이 없어 책이 빈다. 제작 칸 제작(CraftRecipe)은 해금을 보지 않으므로 그대로 되고, 레시피 책 빠른 제작(CraftRecipeAuto)은 거부한다.

## 화로
- 클릭 → ContainerOpen(창 2, 종류 2, 블록 위치, -1), InventoryContent(화로 창 빈 3칸)와 4창, ContainerSetData 키 0·1·2·3 값 0(열 때 한 번).
- 연료 Place(1, 12:석탄, 24:1) → 응답 [12:0 x0, 24:1 x1 같은 ID]. 재료 Place(2, 12:원석, 25:0) → 응답, 곧 InventorySlot(화로 창 칸 1 빈 칸), UpdateBlock `lit_furnace`(같은 방향), BlockActorData `{"LastFuel": 석탄 아이템 NBT(`WasPickedUp` 0 포함)}`. 열 때와 setblock 때는 BlockActorData를 보내지 않는다.
- 굽는 동안 매 틱 ContainerSetData 키 0(굽기 진행 0→199, 다 되면 0)과 키 1(남은 연료 틱, 1600에서 감소), 연료를 태울 때 키 2(연료 총 1600) 한 번. 한 개가 끝날 때마다 InventorySlot 칸 2(주괴, 처음 새 ID, 늘면 같은 ID)와 칸 0(원석 감소, 마지막은 빈 칸). 철 두 개에 약 20초.
- 꺼내기 Place(2, 26:2, 12:빈 칸) → 응답 [26:2 x0, 12:0 x2 주괴 ID 그대로]. 경험치 구슬 AddActor(1~2개, 무작위) → 플레이어가 주우면 LevelEvent 1051, UpdateAttributes player.experience, RemoveActor.
- Anchor: ContainerSetData 키 0·1·2, 열 때 0~3 초기값, BlockActorData `LastFuel`(`WasPickedUp` 포함). 경험치는 결과 아이템마다 정한 값으로 쌓았다가 꺼낼 때 구슬로 준다(공식 서버와 같음).

## 인챈트
- 열기: ContainerOpen(창 2, 종류 3, 블록 위치, -1)과 빈 PlayerEnchantOptions. 곡괭이를 22:14에 놓으면(청금석 없이도) 선택지 3개가 온다. 청금석을 23:15에 놓아도 다시 오지 않는다.
- 선택지: `Cost`(필요 레벨, 무작위; 레벨 30·유효 책장 15에서 예 6·13·30), `Enchantments{Slot, Enchantments [[착용], [손에 듦], [아이템 자신]]}`(각 {Type, Level}), `Name`(무작위 단어), `RecipeNetworkID`.
- 발동 목록: 0 착용(보호 계열, 가시, 호흡, 친수성, 차가운 걸음), 1 손에 듦(날카로움, 발화, 약탈, 화염, 섬세한 손길, 내구성, 행운, 바다의 행운), 2 아이템 자신(효율, 밀치기, 힘, 밀어내기, 무한, 미끼, 충성, 집전, 관통, 빠른 장전, 수선, 소실 저주). 곡괭이에서 내구성(17)은 1번, 효율(15)은 2번 목록이다(gophertunnel 문서와 같음).
- `Slot`(아이템 종류별): 투구 1, 흉갑 2, 레깅스 8, 부츠 4, 책 -1, 검 16, 활 32, 괭이 64, 도끼 512, 곡괭이 1024, 삽 2048, 낚싯대 4096, 삼지창 32768, 쇠뇌 65536. 가위와 부싯돌은 선택지가 없다. 삼지창은 공식 서버에서 선택지 3개다.
- 인챈트 요청(상태 0, CraftResultsDeprecated 없음): `[CraftRecipe(선택지 RecipeNetworkID, 1), Consume(1, 22:14), Consume(선택지 순서+1, 23:15), Place(1, 60:50 ID=요청 ID, 22:14 ID=요청 ID)]`. 청금석 개수는 선택지 순서 + 1이고 `Cost`는 레벨 조건이다. 응답 [22:14 x1 새 ID, 23:15 x2 같은 ID], 곧 빈 PlayerEnchantOptions, UpdateAttributes player.level 29(첫 선택지는 1레벨). 인챈트된 아이템 NBT는 보내지 않는다(클라이언트가 선택지로 안다).
- `RecipeNetworkID`와 거래 `netId`는 CraftingData 마지막 번호(4768) 다음부터 서버 전체에서 하나씩 는다(인챈트 4769~4771, 그 뒤 주민 오퍼 4772~). 같은 주민의 오퍼는 다시 열어도 같은 번호다.
- Anchor: `Slot`과 세 목록은 공식 서버 값이다. 선택지 ID는 CraftingData 다음부터 서버 카운터다. 청금석 = 순서 + 1, 레벨 = 순서 + 1. 삼지창 선택지는 없다(덤프에 삼지창 인챈트가 없음, `intended.txt`).

## 작물, 부수기, 드롭, 줍기
- 뼛가루: UseItem(ClickBlock, 작물 윗면, HeldItem=뼛가루, 예측 액션) → LevelEvent 2005(ParticleCropGrowth, 블록 위치, data 0), InventoryContent(뼛가루 -1, 새 스택 ID), UpdateBlock 밀 `growth` 증가(무작위 1~3단계), 재동기화(밭 `moisturized_amount` 0 포함).
- 서버 권위 부수기: PlayerAuthInput 한 틱에 블록 액션 `[StartBreak(위치, 면 1), PredictDestroyBlock(위치, 면 1)]`(InputFlagPerformBlockActions). 공식 서버: LevelEvent 3600(StartBlockCracking, data 65535), LevelEvent 2001(ParticlesDestroyBlock, 블록 중심, data = 부서진 블록 런타임 ID), UpdateBlock 공기, 드롭 AddItemActor(밀 1, 씨앗 0~3 무작위, 각자 SetActorData·MoveActorDelta).
- 드롭 이동: AddItemActor와 MoveActorDelta의 위치는 발 + 0.125, MoveActorDelta `Ticks` 0, 떨어지는 동안 매 틱 보내고 바뀌지 않은 축은 없음(null). 경험치 구슬과 주민의 MoveActorDelta도 `Ticks` 0이다.
- 줍기: 플레이어 상자 안에 들면 TakeItemActor(아이템 런타임 ID, 플레이어 런타임 ID) → 서버 → 클라이언트 InventoryTransaction(normal: 컨테이너 창 0 칸 N 이전 → 새 아이템 새 ID, 출처 2(world) 칸 1 아이템 → 빈 칸) → RemoveActor. InventorySlot은 보내지 않는다.
- 봇의 걷기는 서버 권위 이동이 CorrectPlayerMovePrediction으로 되돌린다(봇이 물리를 재현하지 않음). 그래서 봇은 `tp`로 드롭 위에 선다.
- 블록 액션 형식: gophertunnel 형식(모든 액션에 위치·면)의 StopBreak, AbortBreak를 공식 서버가 받는다. StartItemUseOn, StopItemUseOn을 PlayerAuthInput 블록 액션에 넣으면 공식 서버가 Disconnect 없이 연결을 끊는다.
- Anchor: 드롭 위치·`Ticks`·매 틱 전송은 공식 서버와 같다. 줍기 알림 방식은 다르게 보내도 된다(비교는 결과 상태). 블록 액션은 gophertunnel 형식으로 읽고, 모르는 액션이나 면이 오면 그 패킷의 블록 액션만 버린다.

## 거래와 주민
- 소환: 콘솔 `summon villager x y z 0 0 minecraft:become_farmer 이름`이 어른 농부 villager_v2를 만든다(Variant 1, SkinId 0, `villager_v2`라는 명령 이름은 문법 오류). 명령의 이벤트가 기본 생성 이벤트(직업 무작위, 아기 5%, 니트윗 포함, SkinId 무작위)를 대신한다. `minecraft:spawn_farmer`는 농부·어부·양치기·화살 제조인·석공 중 무작위다. 봇은 `trade`와 `tradeup`의 소환(`summon villager`, 레거시 `summon minecraft:villager`)에 모두 `become_farmer`를 붙여 직업을 고정한다: 직업이 무작위면 첫 등급에 봇이 낼 오퍼가 없거나(어부: 실 → 에메랄드, 에메랄드 + 대구 → 익힌 대구) 거래가 없어(아기, 니트윗) 단계가 실행마다 달라진다.
- 잠긴 오퍼: 공식 서버는 주민의 `TradeTier`보다 높은 `tier`의 오퍼 거래 요청도 상태 0으로 들어준다(TradeTier 0 어부의 tier 2 "에메랄드 10 → 인챈트 낚싯대", `traderExp` 10). 그 경험으로 주민이 바로 등급 1이 되고(다시 열면 UpdateTrade `TradeTier` 1), 등급을 올린 거래의 경험치 구슬 합은 11로 보통 거래(3~6)보다 많아 플레이어가 레벨 1이 된다. 공식 클라이언트는 잠긴 오퍼를 고를 수 없으므로 봇도 `TradeTier` 이하의 오퍼만 고른다. Anchor는 잠긴 오퍼 거래를 거절한다(`tick/trade.rs`).
- `summon minecraft:villager`도 AddActor 종류 `minecraft:villager_v2`로 온다. 공식 서버 1.26.52에는 레거시 actor가 없고 그 주민도 Trade2 창이 정상으로 열린다.
- 열기: InventoryTransaction UseItemOnEntity(Interact, 주민 런타임 ID) → ContainerOpen 없이 UpdateTrade(창 2부터, `WindowType` 15, `Size` 0, `TradeTier` 0, `VillagerUniqueID` 주민 고유 ID, `EntityUniqueID` -1, `DisplayName` "entity.villager.<직업>", `NewTradeUI` true, `DemandBasedPrices` true) → InventoryContent(거래 창, 빈 목록), Event EntityInteract(`EntityRuntimeID` = 플레이어 고유 ID, UsePlayerID true, InteractionType 7, InteractionEntityType 115, InteractedEntityID 주민 고유 ID, EntityVariant 직업 번호), 주민 Animate.
- 오퍼 NBT: 루트 `{Recipes: list, TierExpRequirements: [{"0":0},{"1":10},{"2":70},{"3":150},{"4":250}]}`(원소마다 키가 등급 문자열인 int 하나). 오퍼마다 `buyA`(compound: `Count` byte, `Damage` short(32767 = 아무 meta), `Name` string, `WasPickedUp` byte, 블록 아이템이면 `Block{name, states, version}`), `buyB`(같은 모양, 있을 때만), `buyCountA` int(= `buyA.Count`), `buyCountB` int, `demand` int, `maxUses` int, `netId` int, `priceMultiplierA`·`B` float(0.05, 비싼 것 0.2), `rewardExp` byte(1), `sell`(같은 모양 + 인챈트면 `tag{ench:[{id,lvl}]}`), `tier` int(0~4), `traderExp` int, `uses` int. 모든 등급의 오퍼가 처음부터 들어 있고 `TradeTier`가 열린 등급을 정한다.
- 거래 요청(상태 0): 지불 `[Place(buyCountA, 12:칸, 47:4 ID 0)]` → 응답 [12:칸, 47:4 새 ID]. 거래 `[CraftRecipe(오퍼 netId, 1), CraftResultsDeprecated([sell], 1), Consume(buyCountA, 47:4), Place(sell 개수, 60:50 ID=요청 ID, 12:칸)]` → 응답 [47:4 x0, 12:칸 결과 새 ID].
- `DemandBasedPrices`는 늘 true다. 새 주민의 `demand`는 0이고 거래 뒤에도 0이다. demand는 주민이 작업대에서 재입고할 때(몹 AI)만 바뀐다.
- 거래 뒤 공식 서버는 UpdateTrade를 다시 보내지 않는다(클라이언트가 스스로 반영). 창을 연 채 등급이 올라도(10번째 거래에서 0→1) 주민 SetActorData(101 TradeTier)만 보낸다. 다시 열면 거래한 오퍼만 `uses` +1이고 가격, demand, 배수는 그대로다. 보상으로 주민 위치에 경험치 구슬(무작위 개수)을 소환하고 주민 SetActorData가 여럿 온다(거래 경험, 애니메이션).
- 주민 메타데이터: AddActor `villager_v2`에 101 TradeTier 0, 102 MaxTradeTier 4, 103 TradeExperience 0, 104 SkinId(기본 생성 이벤트면 무작위, 0과 4를 봄; `become_farmer` 소환은 0). 거래 뒤 SetActorData는 바뀐 키만(예 `{103: 5}`).
- 직업 번호(공식 서버 `behavior_packs/vanilla` villager_v2): 0 무직, 1 농부, 2 어부, 3 양치기, 4 화살 제조인, 5 사서, 6 지도 제작자, 7 성직자, 8 갑옷 제조인, 9 무기 제조인, 10 도구 제조인, 11 도축업자, 12 가죽 세공인, 13 석공, 14 니트윗. 소환한 주민의 Variant와 거래 창 제목이 이 표와 맞는다(3 → `entity.villager.shepherd`, 7 → `entity.villager.cleric`).
- 닫기: 클라이언트 ContainerClose(창, 15, false) → 공식 서버 ContainerClose(창, 247, false).
- Anchor: 주민형 NPC의 actor 종류는 `minecraft:villager_v2` 하나, 창 종류 15, 닫기 응답 247, 오퍼 NBT는 공식 서버 키 전부, `netId`는 서버 카운터(처음 열 때 매기고 유지), `uses`만 늘리고 UpdateTrade를 다시 보내지 않는다(등급이 올라도). 메타데이터 101~104는 공식 서버 키이고 SkinId는 0으로 고정한다. 직업 번호 표(`trade.rs`)는 공식 서버와 같다. 주민 AI가 없어 demand는 늘 0이다. 거래 Event EntityInteract는 보내지 않는다(통계용, 클라이언트 동작에 영향 없음).

## 모루, 대장장이, 신호기 요청
- 모루 이름 바꾸기: FilterStrings `["새 이름"]`, `[CraftRecipeOptional{RecipeNetworkID 0, FilterStringIndex 0}, Consume(1, AnvilInput:1), Place(1, Created:50, 인벤토리)]`. 레벨 30→29.
- 대장장이(네더라이트 검): `[CraftRecipe{스미딩 레시피 ID, 1}, Consume(1, SmithingTableInput:51), Consume(1, SmithingTableMaterial:52), Consume(1, SmithingTableTemplate:53), Place(1, Created:50, 인벤토리)]`.
- 신호기: 지불 칸 BeaconPayment:27에 철 주괴를 먼저 놓고 `[BeaconPayment{1, 0}, Destroy(1, BeaconPayment:27)]`.
- 모두 상태 0이다. Anchor도 같은 모양을 받는다(`stations`).

## 스폰 창
- 순서: NetworkSettings → ServerToClientHandshake → PlayStatus(0) → ResourcePacksInfo → ResourcePackStack → LevelEventGeneric → PlayerList → SyncWorldClocks → JigsawStructureData → VoxelShapes → StartGame → SyncActorProperty ×13 → ItemRegistry → (RequestChunkRadius) → SetSpawnPosition → SyncWorldClocks → SetDifficulty → SetCommandsEnabled → UpdateAdventureSettings → UpdateAbilities → GameRulesChanged → PlayerList → UpdateAbilities → BiomeDefinitionList → AvailableActorIdentifiers → PlayerFog → CameraPresets → CameraAimAssistPresets → CameraSpline → UpdateAttributes → CreativeContent → TrimData → InventoryContent ×4 → PlayerHotBar → CraftingData → AvailableCommands → Respawn ×3 → (청크) → LevelSoundEvent → SetActorData → CurrentStructureFeature → UpdateAttributes → ChunkRadiusUpdated → (주변 몹 AddActor 등) → SetHealth → PlayStatus(3).
- LevelEventGeneric: EventID 9801(SleepingPlayers), 데이터 = 루트 태그 없는 네트워크 NBT 컴파운드 내용(int 셋과 끝 0): `ableToSleep` 1, `overworldPlayerCount` 1, `sleepingPlayerCount` 0(1인).
- PlayerList(두 번, 같은 내용): ActionType Add, 항목 하나(자기): UUID, EntityUniqueID, Username, XUID "", PlatformChatID "", BuildPlatform 1, Skin 전체, Teacher·Host·SubClient false, PlayerColour {237,237,237,255}.
- SyncWorldClocks: 첫째 PayloadType 0(SyncStates [{ClockID, Time, Paused false}]), 둘째 PayloadType 1(Clocks [{ID, Name "minecraft:overworld", Time, Paused, TimeMarkers 6개: sunrise 23000, night 13000, noon 6000, midnight 18000, day 1000, sunset 12000, 모두 Period 24000}]). 마커 ID는 고정값이다.
- VoxelShapes: NameMap 220, Shapes 287, CustomShapeCount 0, 인코드 23,178바이트. 기준 목록 `voxel_shapes.json`(`xtask/data/reference/`)과 개수, 이름 순서, 크기가 같다. Anchor는 그 파일로 보낸다.
- SyncActorProperty 13개: zombie_nautilus, wolf, sulfur_cube, chicken, creaking, pig, cat, egg, armadillo, happy_ghast, copper_golem, bee, cow의 속성 정의. Windows판 공식 서버의 순서와 내용이 기준 목록 `entity_properties.dat`와 같다. Anchor는 그 파일로 보낸다.
- AvailableActorIdentifiers: 8,907바이트, 기준 목록 `entity_identifiers.dat`과 바이트가 같다. Anchor는 그 파일을 그대로 보낸다.
- JigsawStructureData: 없으면 공식 클라이언트가 `disconnectionScreen.missingStructureData`로 끊는다. Anchor는 기준 목록 `jigsaw_structure_data.dat` 내용을 공식 서버와 같은 자리에 보낸다.
- CameraPresets(프리셋 6개: first_person, fixed_boom, follow_orbit, free, third_person, third_person_front), CameraAimAssistPresets(범주 bucket·empty_hand·default, 프리셋 하나), CameraSpline(빈 목록): Anchor는 Windows판 공식 서버 내용(`xtask/data/reference/bds_camera.json`)을 같은 자리에 보낸다.
- Windows판과 Linux판 공식 서버(둘 다 1.26.52.3)는 같은 내용을 보내지만 아래 목록의 순서가 다르다. 판마다 몇 번 돌려도 순서는 같다. 모두 클라이언트가 이름으로 찾는 목록이라 순서에 뜻이 없고, P6는 키 순서로 바꿔 비교한다(`xtask/src/p6/compare.rs`의 `in_key_order`, `bot cloudburst`의 `sortByIdentifier`).
  - SyncActorProperty 패킷의 엔티티 종류 순서(Linux: sulfur_cube, wolf, armadillo, pig, happy_ghast, cat, creaking, zombie_nautilus, chicken, bee, egg, cow, copper_golem). 엔티티 하나의 `properties` 목록 순서는 두 판이 같고 뜻이 있다: AddActor의 EntityProperties가 그 목록의 번호(`Index`)로 속성을 가리킨다(공식 서버 닭 AddActor `IntegerProperties` Index 0·1, gophertunnel `IntegerEntityProperty`). 그래서 이 목록은 순서대로 비교한다.
  - CameraAimAssistPresets 범주 순서(Linux: empty_hand, default, bucket)와 범주마다 `Blocks` 우선순위 목록 순서(예 bucket: Windows cauldron·lava·water, Linux cauldron·water·lava). 프리셋은 범주를 이름(`ItemSettings.Category`, `DefaultItemSettings`, `HandSettings`)으로 가리키고, 우선순위는 블록 이름마다 하나다.
  - JigsawStructureData `template_pools` 64개 순서. 네 최상위 목록(jigsaws, processors, structure_sets, template_pools)은 `description.identifier`로 이름 붙은 정의이고 서로 이름(`start_pool`, `structure`)으로 가리킨다. 정의 안의 목록(가중치 있는 `elements` 등)은 두 판의 순서가 같고 순서대로 비교한다.
  - Anchor: Windows판 순서(기준 목록 파일 순서)로 보낸다. Anchor의 AddActor는 EntityProperties를 비워 보내므로 속성 번호를 쓰지 않는다.
- SetDifficulty 2(normal, 설정 값). UpdateAdventureSettings {NoPvM false, NoMvP false, ImmutableWorld false, ShowNameTags false, AutoJump true}. UpdateAbilities(두 번 같은 값) {EntityUniqueID 플레이어, PlayerPermissions 1(member), CommandPermissions 0, Layers [{Type 1(base), Abilities 1048575, Values 63, FlySpeed 0.05, VerticalFlySpeed 1, WalkSpeed 0.1}]}.
- SetCommandsEnabled: `allow-cheats=false`면 false, true면 true. Anchor는 언제나 true다(치트 개념 없음).
- GameRulesChanged 39개(이름, 값, CanBeModifiedByPlayer true): commandBlockOutput, doDayLightCycle, doEntityDrops, doFireTick, recipesUnlock, doLimitedCrafting(false), doMobLoot, doMobSpawning, doTileDrops, doWeatherCycle, drowningDamage, fallDamage, fireDamage, keepInventory(false), mobGriefing, pvp, showCoordinates(false), playerWaypoints(int 1), locatorbar, showDaysPlayed(false), naturalRegeneration, tntExplodes, sendCommandFeedback, maxCommandChainLength(65535), doInsomnia, commandBlocksEnabled, randomTickSpeed(1), doImmediateRespawn(false), showDeathMessages, functionCommandLimit(10000), spawnRadius(10), showTags, freezeDamage, respawnBlocksExplode, showBorderEffect, showRecipeMessages, playersSleepingPercentage(100), projectilesCanBreakBlocks, tntExplosionDropDecay(false). 나머지 bool은 true.
- UpdateAttributes 첫째(Tick 0) 17개: player.hunger 20/0~20, saturation 5, exhaustion 0, level 0/0~24791, experience 0/0~1, health 20, follow_range 16/0~2048, knockback_resistance 0/-2~1, movement 0.1, underwater_movement 0.02, lava_movement 0.02, attack_damage 1/1~1, absorption 0/0~16, luck 0/-1024~1024, friction_modifier 1/0~256, bounciness 0/0~1, air_drag_modifier 1/0~256(Default = 값, 수정자 없음). 둘째는 같은 속성이 겹친 57항목이다(공식 서버 특성, Anchor는 따라 하지 않음).
- SetHealth 20(PlayStatus(3) 바로 앞). PlayerFog 빈 스택. Respawn 세 번: (State 0, y 32769.62), (State 0, y 32768), (State 1, 실제 눈 위치), EntityRuntimeID 0.
- 플레이어 SetActorData(Tick 0) 72개 키: 0 flags 844424930656256, 1 1, 2 0, 3 0, 4 이름, 5 -1, 6 0, 7 300, 22 0, 26 0, 27 1, 28 [0,0,0], 37 0, 39 0, 42 300, 43~46 0, 55 -1, 57~60 0, 70 0, 71 "", 72 "", 73 1, 75~77 0, 78 -1, 81 1, 82 0, 84 "", 88 -1, 91 0, 92 35184405643264, 100 "", 101~104 0, 106 3, 107 1, 108 8, 109 16, 110 "ambient", 111 1, 114~117 0, 118 {}, 119 0, 120 0, 124 1, 125 20, 126 614, 127 [0,0,0], 128 0, 129 0, 130 [0.6,1.8,0], 131 0, 132 "", 133 [0,0,0], 134 0, 135 0, 136~138 -1, 140 64.
- InventoryContent 4개: 창 0(36칸), 120(갑옷 5칸), 124(UI 54칸), 119(왼손 1칸), 모두 빈 칸. PlayerHotBar {0, 창 0, select true}.
- TrimData: 무늬 18개(ward, sentry, snout, dune, spire, tide, wild, rib, coast, shaper, eye, vex, silence, wayfinder, raiser, host, flow, bolt; ItemName `minecraft:<무늬>_armor_trim_smithing_template`), 재료 11개(quartz §h, iron §i, netherite §j, redstone §m, copper §n, gold §p, emerald §q, diamond §s, lapis §t, amethyst §u, resin §v; ItemName은 재료 아이템). Anchor는 같은 목록(`crafting::TRIM_*`)을 보낸다.
- MobEquipment, MobArmourEquipment는 스폰 창에 오지 않는다(주민·몹이 보일 때만). 스폰 창의 AddActor, RemoveActor, MoveActorDelta 등은 평지의 바닐라 몹(닭, 소, 양)이다. 참가 플레이어의 entity spawn 소리(LevelSoundEvent)도 온다. 이 패킷들은 유예표(`deferred.txt`)에 있다.
- SetLocalPlayerAsInitialised 뒤 UnlockedRecipes(종류 1, 20개)가 온다(창 밖, 레시피 책 절).
- 공기: 스폰 창 데이터에는 Breathing 플래그가 없다. 물속 첫 틱에 Breathing을 끄고 공기 -1/틱, -20이면 0으로 하고 익사 피해 2. 물 밖이면 바로 Breathing을 켜고 공기 +4/틱으로 최대까지. Anchor는 이를 따르고 Initialised 뒤 첫 틱에 키 0만 보낸다.
- Anchor 스폰 창은 위 순서와 값(크기까지)을 공식 서버와 같게 보내고, 비교는 zlib, Snappy, NetherNet 모두 같다.

## 명령
- 플레이어 명령은 CommandRequest(origin `player`, 새 UUID, RequestID "", PlayerUniqueID 0, Internal false, Version `"latest"`)로 보낸다. 공식 서버는 이 형태를 받는다.
- AvailableCommands(구성원): 공식 서버는 권한과 상관없이 82개 명령을 모두 보낸다(PermissionLevel 0: 4, 1: 60, 2: 7, 3: 1, 4: 10). 클라이언트가 자기 권한으로 숨긴다. 순서는 SetDifficulty 뒤 SetCommandsEnabled, CraftingData 뒤 AvailableCommands.
  - Anchor: 권한 있는 명령만 보낸다. 그래서 op·deop 뒤에는 AvailableCommands를 다시 보낸다. `help`는 명령으로 보내지 않는다(클라이언트 자체 명령, 공식 서버와 같음).
- 매개변수 형 번호: int 1, float 3, target 8, string 56, position 65, message 68(MessageRoot: say, me, tell, kick), raw_text 70, json 74. gophertunnel의 `CommandArgTypeMessage`는 67이다. Anchor는 관찰한 값 68을 쓴다.
- 공식 서버 명령 모양(권한 단계, 플래그, 오버로드): give 1 0 `target enum:Item int? int? json?` / effect 1 0 `target enum:ClearEffects enum:Effect?`, `target enum:Effect int? int? enum:Boolean?`, `target enum:Effect enum:AddInfiniteEffect int? enum:Boolean?` / gamemode 1 0 `enum:GameMode target?`, `int target?` / time 1 0 `enum:TimeModeAdd int`, `enum:TimeModeSet int`, `enum:TimeModeSet enum:TimeSpec`, `enum:TimeModeQuery enum:TimeQuery` / xp 1 0 `int target?`, `suffix:L target?`(접미사 매개변수는 Valid 비트 없이 접미사 번호) / say 1 192 `message` / kick 1 128 `target message` / op 2 1152 `target` / particle 1 0 `string position?` / tell 0 192 별칭 `tell w msg` `target message` / tp는 `teleport`의 별칭.
- 별칭이 없는 명령의 AliasesOffset은 0xFFFFFFFF. 별칭 enum은 `<이름>CommandAliases`이고 이름을 맨 앞에 넣는다(`teleport`: `["teleport","tp"]`, `allowlist`: `["allowlist","whitelist"]`). Anchor도 같다.
- Anchor의 기본 명령 21개(give, effect, enchant, gamemode, tp, kill, tell, clear, difficulty, kick, op, deop, me, say, list, stop, time, title, xp, spawnpoint, setworldspawn)는 인자 형, 이름, enum이 공식 서버와 같다. `particle`, `whitelist`, `transferserver`, `time start|stop`은 덤프 원본의 명령 모양을 따라 공식 서버와 다르다. 명령 목록 비교는 둘 다 가진 명령만 메모로 본다.
- op: 콘솔 `op <이름>` 뒤 공식 서버는 AvailableCommands를 다시 보내지 않고 Text(종류 10, rawtext `commands.op.message`)와 UpdateAbilities(CommandPermissions 1, PlayerPermissions 2, 층 하나 Abilities 1048575, Values 255, FlySpeed 0.05, WalkSpeed 0.1, VerticalFlySpeed 1)를 보낸다.
  - Anchor: op면 CommandPermissions 1, PlayerPermissions 2, Values에 OperatorCommands·Teleport(생존 255), deop면 0·1·63. 같은 상태로 다시 op하면 보내지 않는다. op인 플레이어는 들어올 때 StartGame `PlayerPermissions`부터 op 값이다.
- CommandOutput 공통: 원점은 요청 그대로(Origin 0 player, 같은 UUID, RequestID "", PlayerUniqueID 0), OutputType 3(AllOutput), DataSet 없음. 성공이면 SuccessCount 1과 메시지 Success true, 실패면 0과 false. 메시지는 번역 키와 매개변수다.
- 명령마다 공식 서버 값:
  - 권한 없음(구성원의 `time set 500`): `commands.generic.error.permissions` `["time"]`. 목록에 있는 명령이라 "모르는 명령"이 아니다.
  - `say hi`: CommandOutput이 없다. 모두에게 Text(종류 8 Announcement, SourceName `P6Commands`, Message `[P6Commands] hi`, 번역 없음).
  - `give @s stone 3`: `commands.give.success` `["Stone","3","P6Commands"]`. 받는 이에게 Text(종류 10, rawtext `commands.give.successRecipient` `["Stone","3"]`), InventoryTransaction, InventoryContent 4창, PlayerHotBar, 소리 `pop`.
  - `tp ~ ~1 ~`: `commands.tp.success.coordinates` `["P6Commands","-3.50","-59.00","-11.50"]`(발 좌표, 소수 둘째 자리). MovePlayer(Mode 2 Teleport, TeleportCause 3, 눈 높이 +1), Text(종류 10, `commands.tp.successVictim` `["-3.50, -59.00, -11.50"]`). 떠 있는 봇에게 곧 CorrectPlayerMovePrediction을 보낸다.
  - `gamemode creative`: `commands.gamemode.success.self` `["%createWorldScreen.gameMode.creative"]`. UpdatePlayerGameType(GameType 1, PlayerUniqueID 자신, Tick 서버 틱)만 보내고 SetPlayerGameType, UpdateAbilities는 보내지 않는다. Text(종류 10, `gameMode.changed` `["%createWorldScreen.gameMode.creative"]`).
  - `time set 1000`: `commands.time.set` `["1000"]`. SetTime이 아니라 SyncWorldClocks(SyncStates 하나, Time 1000, Paused false)로 알린다. 시각이 흐를 때도 몇 초마다 SyncWorldClocks를 보낸다.
  - `effect @s speed 10 1`: `commands.effect.success` `["%potion.moveSpeed","1","P6Commands","10"]`. MobEffect(Operation 1 Add, EffectType 1, Amplifier 1, Duration 200, Particles true, Ambient false), UpdateAttributes(이동 속도), SetActorData.
  - `xp 10`: `commands.xp.success` `["10","P6Commands"]`. UpdateAttributes(레벨 1, 진행 0.33).
  - `give @s no_such_item`: `commands.generic.syntax` `["/give @s ","no_such_item",""]`.
  - `no_such_command`: `commands.generic.unknown` `["no_such_command"]`.
  - `effect @s clear`: 효과 ID 순으로 지우고, 제거 MobEffect마다 그 효과의 증폭과 남은 시간을 싣는다.
- 문법 오류 `commands.generic.syntax` [앞, 틀린 낱말, 뒤]: 앞은 `/`부터 센 줄에서 틀린 낱말 앞 최대 10글자, 뒤는 낱말 뒤 최대 10글자(`/give @s no_such_item 3 and more words` → `["/give @s ","no_such_item"," 3 and mor"]`). 인자가 모자라면 낱말 ""과 줄 끝 앞 10글자(`/give @s` → `["/give @s","",""]`). 오버로드가 여럿이면 가장 멀리 맞은 곳이다(`/effect @s speed abc` → `abc`).
- 공식 서버는 명령마다 Event(SlashCommandExecuted: 명령 이름, 성공 수, 메시지 수·키)를 보낸다(원격 측정). Anchor는 보내지 않는다.
- Anchor가 공식 서버와 같은 것: 모르는 명령, 권한 없음(채팅으로 친 `/` 줄도), 문법 오류의 키와 매개변수(`command::Error::Syntax`), 게임 모드 이름 `%createWorldScreen.gameMode.<모드>`, `%potion.*` 이름, 명령이 손댄 플레이어에게 가는 Text 10(`gameMode.changed`는 자기 자신에게도, `commands.give.successRecipient`, `commands.tp.successVictim`), tp 좌표 소수 둘째 자리, gamemode 뒤 바뀐 플레이어에게만 UpdatePlayerGameType(SetPlayerGameType 없음, 하드코어 관전 전환도 같음; 남의 ID로 보내면 클라이언트가 무시함), `effect clear`의 순서와 값.
- Anchor가 일부러 다른 것(`intended.txt`):
  - `/say`: `§d%chat.type.announcement` [이름, 말] 번역과 빈 CommandOutput.
  - give 성공 메시지의 아이템 이름: Anchor는 ID(`minecraft:stone`)를 쓴다. 아이템 표시 이름 표가 없다.
  - gamemode 뒤 UpdateAbilities를 보낸다. API가 바꾼 비행, 충돌, 비행 속도, op 값을 실어야 하고 같은 값이라 클라이언트에 해가 없다.
  - `/xp abc`: 문자열 매개변수 하나(`10` 또는 `10L`)라 오버로드에 맞고 핸들러가 usage로 답한다.
  - 지금과 같은 게임 모드로 `/gamemode`: 실패한다(공식 서버는 성공).
  - UpdatePlayerGameType `Tick`: 공식 서버는 서버 틱(예 333), Anchor는 0이다(비교 필드 아님).
  - `/particle`: 번호 입자 모양(`<name> <position> <xd> <yd> <zd> [count] [data]`)이라 공식 서버 오버로드 `string position?`와 다르다(사실 기록 시나리오).

## 채팅과 문자열 상한
- 클라이언트 Text(Chat)에 공식 서버는 모두에게 Text(종류 1 Chat, SourceName `이름§r`, Message 원문, Parameters 없음, NeedsTranslation false)를 보낸다.
  - Anchor: PlayerChat의 format이 기본값(`None`)이면 공식 서버 모양이다. 리스너가 format을 주면 그 글을 Raw로 보낸다. 받는 이와 말은 이벤트를 따른다. 표시 이름이 채팅 출처 상한(256바이트)을 넘으면 줄을 잃지 않게 `chat.type.text` 번역(Text 종류 2 Translation, [이름, 메시지], NeedsTranslation true)으로 보낸다.
- 채팅 줄 상한은 바이트다: 공식 서버는 512바이트까지 돌려보낸다. ASCII 512 통과, 513 안 됨. 한글(3바이트) 170자(510바이트) 통과, 171자(513바이트) 안 됨. 넘는 줄은 연결을 끊지 않고 버린다. 65,538바이트(21,846자) 원시 Text도 버리고 연결을 유지한다.
  - Anchor: 채팅 줄 상한 512바이트.
- CommandRequest 줄: 507바이트까지 처리하고 508바이트부터 공식 서버가 Disconnect 없이 연결을 끊는다(`/tell @s …`와 600바이트 모르는 명령 모두). 한글 519바이트(179자)도 끊긴다. 단위는 바이트다(`line<바이트>`, `linekorean<바이트>` 시나리오).
  - Anchor: 디코더 단위(바이트)는 같다. 연결을 끊지 않고 512자·2,048바이트 상한에서 `TooLong`으로 답한다. 공식 클라이언트는 그렇게 긴 줄을 보내지 않으므로 답하는 쪽이 더 안전하다. 이 시나리오들은 사실 기록이다.

## 폼
- 공식 서버용 스크립트 팩(`@minecraft/server-ui` 2.0.0, 실험 토글 없음): `xtask/data/p6/forms-pack`. `/scriptevent p6:<종류>`로 모두에게 폼을 보낸다. `development_behavior_packs/p6_forms`와 `worlds/p6/world_behavior_packs.json`으로 켠다. ActionFormData와 ModalFormData 둘 다 `header`, `label`, `divider`가 있다.
- 모달: `{"type":"modal","title","content","button1","button2"}`. 답은 true/false, 닫으면 CancelReason.
- 메뉴: `{"type":"form","title","content","elements":[...]}`. 원소는 버튼 `{"type":"button","text","image":null}` 또는 `"image":{"type":"path","data":"textures/items/apple"}`, `{"type":"header","text"}`, `{"type":"label","text"}`, `{"type":"divider","text":""}`. `buttons` 키는 없다. 답은 버튼만 센 번호다.
- 사용자 정의: `{"type":"custom_form","title","icon":null,"content":[...]}`. 원소는 header·label·divider(메뉴와 같은 모양), `{"type":"input","text","placeholder","default"}`, `{"type":"toggle","text","default"}`, `{"type":"slider","text","min","max","step","default","timeout":100}`, `{"type":"dropdown","text","options","default"}`. 숫자는 `10.0`처럼 실수로 쓴다(default 정수는 `3`). 답은 원소마다 하나(header·label·divider는 null).
- Anchor: 같은 JSON을 보낸다(그림 없는 버튼 `"image":null`, `"icon":null`, 슬라이더 `"timeout":100`). 키 순서는 이름순이다(JSON 객체라 클라이언트는 순서를 보지 않는다).

## kick
- 콘솔 `kick <이름> <이유>`: Disconnect(Reason 55 Kicked, HideDisconnectionScreen false, Message = 이유 그대로, FilteredMessage ""). Anchor `Player::kick`과 같다.

## 입자와 레벨 이벤트 번호
- `/particle <이름> ~ ~1 ~`: SpawnParticleEffect(Dimension 0, EntityUniqueID -1, Position, ParticleName 그대로, MoLangVariables 없음)와 `commands.spawnParticleEmitter.success` [이름]. 번호 입자(LevelEvent)는 이 명령으로 나오지 않는다.
- 눈덩이가 땅에 맞으면 LevelEvent 2009(ParticleGenericSpawn) ×6, EventData 15, 위치 = 맞은 블록 윗면. 알은 LevelEvent 2009 ×6, EventData = (알 아이템 런타임 ID 393 << 16) | 14. 그 뒤 RemoveActor와 LevelSoundEvent.
- 잔류 물약 구름의 `DataParticle`(63)은 35다. 바람 돌격(wind charge)은 LevelEvent 3614(데이터 1)이고 2009가 아니다.
- 그래서 14 = 아이템 깨짐, 15 = 눈덩이 터짐, 35 = mob spell ambient다. gophertunnel 번호는 9번 이후 하나씩 어긋난다(9번 CandleFlame이 목록 끝 81에 있음; IconCrack 13, SnowballPoof 14, 35는 `Ink`).
- 공식 서버의 번호 입자 형식은 LevelEvent 2009 + 데이터 `id | (추가값 << 16)`이다. 다른 서버는 LevelEvent `0x4000 | id` + 데이터 = 추가값 형식을 쓴다.
- 구리: 도끼로 밀랍 구리를 긁으면 LevelEvent 2031(데이터 0), 산화 구리 긁기는 2032, 꿀벌집 밀랍칠은 2030. 함께 오는 원격 측정 Event `WaxedOrUnwaxedCopper`의 `CopperBlockID`는 레거시 블록 ID(밀랍 구리 599, exposed 600, weathered 601, oxidized 701, 구리 블록 595)이고 gophertunnel 상수(42505…, `UnWaxOxidised` 0xfa0a)가 아니다. 공식 서버는 긁기에 LevelSoundEvent(`copper.wax.off`) 없이 LevelEvent만 보낸다.
- 엔티티 플래그: 공식 서버 플래그는 키 0·92 두 int64(비트 0~127)로만 왔다. 128~130번 플래그와 `EntityDataFlagCount` 131 길이 비트셋을 공식 서버가 보내는 곳은 없었다.
- Anchor: 입자 API와 투사체 입자는 공식 서버 번호를 쓴다. 구리 Event는 보내지 않는다.

## 날씨와 번개
- 재현: `cargo xtask p6 api bds weather joinrain joinstorm`, 난이도별 번개는 `boltpeaceful bolteasy bolthard`.
- 날씨 이벤트는 LevelEvent(위치 0,0,0)다: 비 시작 3001, 천둥 시작 3002, 비 멈춤 3003, 천둥 멈춤 3004. 시작의 데이터는 65535, 멈춤은 0. GameRulesChanged나 다른 패킷은 없다.
- 공식 서버는 비·번개 수준을 약 5초에 걸쳐 올리고 내리며, 명령이 보내는 이벤트는 그때 수준에 따른다(수준이 다 오르기 전 `weather thunder`는 3001을 다시 보내고, 천둥 수준이 남은 동안 `weather clear`는 3004도 보낸다).
- 수준이 자리 잡은 뒤(단계마다 7초)는 바뀐 것만 보낸다: 맑음→비 3001, 비→비 없음, 비→천둥 3002, 천둥→천둥 없음, 천둥→비 3004, 비→맑음 3003, 맑음→천둥 3001·3002, 천둥→맑음 3003·3004, 맑음→맑음 없음. Anchor는 수준 변화가 없어 이 표를 쓴다(`src/tick/weather.rs`).
- 접속: StartGame `RainLevel`·`LightningLevel`이 그때 수준이다(자리 잡으면 비 1·0, 천둥 1·1). 스폰 창의 SetCommandsEnabled 바로 뒤에 비면 3001(데이터 = 비 수준 × 65535), 천둥이면 3002만(데이터 = 번개 수준 × 65535) 보낸다. 맑으면 없다. 수준이 다 오르기 전 접속은 0.85·55704처럼 중간 값이다. Anchor는 자리 잡은 값을 보낸다.
- `summon lightning_bolt x y z`(정수 좌표): AddActor `minecraft:lightning_bolt`가 블록 바닥 가운데(x+0.5, y, z+0.5)에 기본 메타데이터(플래그 0)로 온다. 같은 배치의 SetActorData는 플래그 0번(불붙음)이 켜진 실행과 키가 없는 실행이 섞인다. 다음 배치에 불 블록(UpdateBlock 또는 UpdateSubChunkBlocks, `minecraft:fire` age 0)과 LevelSoundEvent `thunder`, `explode`(EntityType `minecraft:lightning_bolt`, EntityUniqueID = 번개, ExtraData -1)가 온다. RemoveActor는 스트라이크 뒤 20~40틱 사이에 온다.
- 번개 2블록 옆의 생존 플레이어: 스트라이크 틱에 체력 20→15(5 피해, ActorEvent hurt 데이터 24 = 번개)와 불붙음 플래그, 그 뒤 20틱마다 불 피해 1. 불은 스트라이크 블록과 주변 1칸 안 무작위 몇 곳(4곳까지 봄)에 붙는다. 8블록 밖은 피해가 없다.
- 난이도: 피해 5와 불붙음은 peaceful, easy, normal, hard 모두 같다. 블록 불은 peaceful·easy에서 없고(블록 갱신 0개) normal·hard에서 붙는다. 자바판도 normal·hard만 불이다.
  - Anchor: `Lightning::fire`는 월드 난이도가 normal·hard일 때만 불을 놓는다.
- 천둥 동안 공식 서버는 플레이어 근처에 무작위 번개를 친다(7초씩 두 단계에 한 번 봄). Anchor는 자연 날씨와 자연 번개가 없다.
- 비가 오면 공식 서버는 불붙은 플레이어를 끈다(fizz 소리, LevelEvent 2029). Anchor는 비의 게임 효과를 구현하지 않는다.
- 비교: 봇의 `weather` 확인(이벤트 순서·데이터, 번개 종류·위치·소리·제거·불·피해·불붙음)과 `weather at join` 확인. 번개의 불붙음 플래그와 무작위 번개는 비교하지 않는다.

## 게임 규칙과 낮 주기
- `gamerule dodaylightcycle false` 뒤 들어오면 StartGame `DayCycleLockTime` = 그때 시각(예 6), `Time`은 계속 세는 월드 시각(예 4322), SyncWorldClocks 시각 6·`Paused` false이고 몇 초마다 같은 시각으로 다시 보낸다.
  - Anchor `daylight-cycle = false`: 시계를 멈춤(paused)으로 표시하지 않고 잠금 시각을 StartGame에 넣고 보정을 계속 보낸다.
- `falldamage`·`drowningdamage` false: GameRulesChanged에 바뀐 규칙 하나씩, 그 뒤 들어온 플레이어의 StartGame과 스폰 GameRulesChanged에 false.
  - Anchor: 스폰 GameRulesChanged는 설정 값이다(같음). 공식 서버는 StartGame GameRules에도 모든 규칙을 넣고 Anchor는 비운다(스폰 비교 필드 밖).

## 엔티티와 메타데이터
- 다른 플레이어의 이동: 공식 서버는 걷기, 제자리 돌기, 고개 들기 모두 MovePlayer mode 0으로 보낸다(MoveActorDelta 아님). tp는 mode 2. 리스폰 뒤 이동도 mode 0.
  - Anchor: 위치나 회전이 바뀐 틱에 `MoveActorAbsolute`(눈 위치, 땅 플래그, 회전 [pitch, yaw, yaw])로 보낸다. 클라이언트가 보간하므로 끊기지 않는다. MovePlayer는 본인 위치 보정과 순간이동(mode 2)에만 쓴다(`intended.txt`). 텔레포트의 땅 플래그와 mode 2는 입력 도착 시각에 따라 두 서버 모두 흔들려 비교하지 않는다.
- 다른 플레이어 AddPlayer: 키 집합은 그 플레이어 자신의 SetActorData 72키 + 10(u8 0), 38(f32 1), 53·54(f32 너비·높이), 56(Vec3 0), 74(u8 0), 139(i64 0) = 79키. 플래그는 그때 상태(갓 들어온 플레이어는 Breathing 없음, 이미 있던 플레이어는 있음). GameType 5, BuildPlatform·DeviceID는 그 클라이언트 값, 능력 층은 base(로딩 중인 플레이어에게는 Type 5 로딩 화면 층이 하나 더 있음). PlayerIndex(27)는 들어온 순서(1, 2). 플레이어 목록 색은 첫 플레이어 #EDEDED, 둘째 #54FFFF.
  - Anchor: 공식 서버 키 집합, 타입, 장치 값을 보낸다. 다른 점: 로딩 화면 층은 보내지 않고, 목록 색은 모두 #EDEDED다. 사람 NPC는 공식 서버에 대응이 없다.
- 메타데이터 타입: 136 int32, 137 int32, 138 int32, 139 int64(다른 플레이어 AddPlayer에만), 140 float32. 127(마지막 사망 위치)은 BlockPos, 133(침대 들어간 위치)은 Vec3. Anchor도 같다.
- 잔류형 재생 물약 구름 AddActor: 61 반경 3.0, 8 색, 63 입자 35, 95 지속 600, 98 줍기당 반경 -0.5, 99 줍기 수 0(int32), 97 0.00083333, 36 물약 ID 28(int16), 플래그 HasGravity+FireImmune, 그 밖에 모든 액터 공통 키.
  - Anchor: 9·62·96은 보내지 않고 63·95·98·99는 공식 서버 값이다. 97은 0으로 두고 서버가 반경을 줄여 보낸다. 36은 구름이 물약 ID를 지니지 않아 보내지 않는다.
- 위치 표시줄: 다른 플레이어가 보이게 되면 PlayerLocation{Type 0, 그 플레이어, 눈 위치}, LocatorBar 추가{Action 1, UpdateFlag 63, 색 = 그 플레이어의 목록 색, IconSize 0.2, `textures/ui/locator_bar_dot_3`, 위치 권한 서버, 보임}, 곧 갱신{Action 3, UpdateFlag 44, 위치 권한 클라이언트, IconSize 1, `locator_bar_dot_0`}. 죽으면 PlayerLocation{Type 1 숨김} + 갱신{UpdateFlag 2, Visible false}, 리스폰하면 갱신{UpdateFlag 2, Visible true}, 나가면 제거{Action 2, UpdateFlag 0}. GroupHandle은 공식 서버가 고른 UUID. Anchor도 같게 보낸다.
- 피해 원인(ActorEvent hurt 데이터): 공격 2, 투사체 3, 익사 9, 번개 24. Anchor는 공식 서버 번호를 싣는다.
- 리스폰한 플레이어 자신에게: Respawn(State 0, 위치 (0,32767,0)) → ActorEvent 18 → SetActorData{0 플래그(Breathing 포함), 53·54}. 리스폰 직후 Breathing은 켜져 있다. Anchor도 같다.
- 죽고 리스폰하는 플레이어의 상자 크기(53·54): 죽을 때 SetActorData{127, 129, 53 0.2, 54 0.2}, 리스폰 때 ActorEvent 18 뒤 SetActorData{53 0.6, 54 1.8}를 본인과 보는 플레이어 모두에게 보낸다. 클라이언트는 죽은 플레이어의 작은 상자를 이 키가 올 때까지 두므로, 빠지면 보는 플레이어에게 리스폰한 플레이어의 히트박스가 작고 이름표가 낮다. Anchor도 같은 값을 보낸다(죽을 때 53·54는 127·129와 다른 SetActorData에 싣는다).
- 다른 플레이어의 속성: 공식 서버는 플레이어 속성이 바뀌면 보는 플레이어에게도 UpdateAttributes를 보낸다. 맞으면 체력(예 19), 회복하면 20, 죽으면 체력 0, 리스폰하면 체력·배고픔·소모·포화·레벨·경험치 전부. 걷거나 때릴 때 소모(exhaustion)도 간다.
  - Anchor: 보는 플레이어에게는 체력만, 바뀔 때마다 보낸다(체력 값은 공식 서버와 같다). 배고픔·소모·포화·경험치는 본인에게만 보인다. 보는 사람에게도 보내면 그 값이 움직일 때마다 보는 사람마다 패킷이 하나씩 더 나간다.
- 그림: 블록 서쪽 면에 걸면 Direction 1, 위치는 그림 가운데(벽에서 1/32 앞, 예 `[13.96875, -58, 13]`). 공식 서버는 맞는 가장 큰 무늬 중 하나를 고른다. Anchor와 위치·방향이 같다.
- 폭죽(폭발 없음, Flight 1): 16 `{Fireworks:{Explosions:[], Flight:1}}`, 17 Vec3 `[0,1,0]`, 18 int64 -1, 5(owner) -1. 폭발이 없어도 터질 때 ActorEvent 25(데이터 0). Anchor도 같다(Flight는 비행 시간에서 계산).
- 엔드 크리스털(`summon ender_crystal`): 47 BlockTarget `[0,0,0]`, 플래그 FireImmune. Anchor와 47이 같다.
- 삼지창: 맞은 주민에게 ActorEvent 2 데이터 3(투사체). 삼지창은 수평 속도가 반대로(약 -0.01배) 튕기고 `item.trident.hit` 뒤 땅에 떨어진다. Anchor도 튕긴다.

## 아이템 사용
- 재현: `cargo xtask p6 api bds itemuse`(봇 `xtask/go/bot/itemuse.go`). Anchor 쪽은 확인 줄만 비교한다.
- 공식 클라이언트 순서(bedrock-protocol-docs changelog 776 `StartUsingItem`, gophertunnel `InputFlagStartUsingItem`): 사용 동작이 있는 아이템은 InventoryTransaction UseItem ClickAir와 같은 틱 PlayerAuthInput의 `StartUsingItem` 플래그로 시작한다. 서버는 동의하면 SetActorData 플래그 UsingItem(4)을 켜고, 아니면 끈다. 클라이언트는 이 답으로 동작을 잇거나 멈춘다. 활과 삼지창은 InventoryTransaction ReleaseItem으로 끝난다. 던지는 아이템은 ClickAir뿐이다.
- 공식 서버: 활·삼지창을 당기는 동안 UsingItem을 켜고 놓으면 끈다. 물약을 마시는 동안 켜고, 마시기는 공식 서버 자기 시계로 끝낸다(CompletedUsingItem).
  - Anchor: 먹기, 마시기, 활, 삼지창, 염소 뿔, 망원경에 UsingItem을 켠다. 마시기는 사용 시간이 지난 뒤의 두 번째 ClickAir로 끝낸다(효과와 병은 같음).
- 삼지창 던지기 소리: `item.trident.throw`(데이터 -1)를 던진 사람의 발 + 0.9(몸 가운데)에서 낸다. Anchor도 같다. 공식 서버는 그 뒤 삼지창 엔티티에 `disappeared`·`reappeared` 소리도 보낸다(화살도 같음). Anchor는 보내지 않는다.
- 투척 물약 지속 시간: 벽(발에서 1.5·2.5·3.5블록)에 깨진 야간 투시(3,600틱)는 2,565·1,755·945틱, 긴 것(9,600틱) 2.5블록은 4,680틱, 발밑은 3,600틱. 몸 높이 안(발 위 0.3~1.6)에서는 높이와 상관없이 같다.
  - Anchor: 비율 = min(1, 1.05 - 0.225 × 몸 가운데 세로선(발~상자 위)까지 거리). 머리 위 1블록쯤에서는 몇 틱 다르다(1.5블록·머리 위 0.97: 식 2,334, 공식 서버 2,339). 즉시 효과(치유, 피해)의 거리 식은 덤프 원본의 식 그대로다.
- 우클릭을 누르고 있기(공식 클라이언트, 모험 모드에서 사용자가 관찰. 봇으로는 볼 수 없음):
  - 블록 위에서 누르고 있으면 InventoryTransaction UseItem ClickBlock을 trigger `SimulationTick`, 예측 0으로 쉬지 않고 보낸다.
  - 허공에서 누르기 시작해 블록으로 시선을 옮기면 `PlayerInput` 클릭 없이 `SimulationTick` 클릭부터 온다.
  - 문을 빈손으로 누르면 누를 때마다 `PlayerInput`, 예측 1 클릭 하나다.
  - ClickAir(눈덩이)는 trigger `Unknown`이고, 엔티티 Interact도 누르고 있어도 되풀이되지 않는다.
  - Anchor: `PlayerInteract::is_first`는 누름마다 처음 쏘는 오른쪽 클릭만 참이다. `SimulationTick`이 아닌 클릭, 또는 직전 블록 클릭에서 `PRESS_GAP_TICKS`보다 늦은 클릭이 새 누름이다.
- 비행: 같은 겨냥에서 공식 서버 투척 물약은 3.5블록 벽 높이 1.6에 맞고 Anchor는 땅에 맞는다(던지는 각도·힘 차이로 보임). 삼지창은 1초 당겨 수평으로 공식 서버 약 24블록, Anchor 약 12블록이다. 범위 밖의 차이로 둔다.

## 웅크리기
- 재현: `cargo xtask p6 api bds sneak`(봇 `xtask/go/bot/sneak.go`). 확인 줄만 비교한다.
- 공식 서버: PlayerAuthInput의 `StartSneaking`·`StopSneaking`마다 본인에게 SetActorData{0 플래그(Sneaking), 54 높이 1.49·1.8}와 SetActorData{92 플래그}를 보내고, 두 패킷의 `Tick`은 그 토글을 실은 입력의 `Tick`이다(다음 틱에 멈춰도 답이 둘). 서버가 스스로 바꾼 값(예 Breathing)은 `Tick` 0이다. 클라이언트는 `Tick`이 있는 답을 그 입력의 프레임으로 되감아 적용한다(bedrock-protocol-docs `PlayerMovementOverview`). `Tick` 0이면 받은 때 바로 적용해, 이미 멈춘 웅크리기를 늦게 온 답이 다시 켠다(천장 밑 점프에서 클라이언트가 잠깐 웅크렸다 펴면 저절로 쉬프트가 눌린 것처럼 보임).
  - Anchor: 입력의 토글(받든 거절하든)에 답하는 본인 SetActorData에 그 틱의 마지막 토글 입력의 `Tick`을 싣고, 보는 플레이어에게는 0이다. 높이는 54가 아니라 130(충돌 상자)으로 보내고 플래그 두 단어를 한 패킷에 싣는다. 공식 서버는 웅크리면 92 단어의 Blocking(72)도 켠다. Anchor는 켜지 않는다.
- 공식 서버는 웅크림을 스스로 정한다: 쉬프트 키는 `SneakDown`이고(`Sneaking`, `PersistSneak`, `SneakToggleDown`, `Sneak*Raw`만으로는 웅크리지 않음), 키 없는 `StartSneaking`은 서 있을 공간이 있으면 거절한다(트인 곳, 2칸 천장 밑). 1.5칸 공간에서는 입력 없이도 웅크림을 켜고 `StopSneaking`을 거절한다. 클라이언트는 천장 밑 점프에서 키 없이 `StartSneaking`을 보내므로, 받아 주면 쉬프트가 저절로 눌린 것처럼 보인다.
  - Anchor: 키(`SneakDown`) 없는 `StartSneaking`을 서 있는 상자(0.6×1.8)가 블록에 막히지 않으면 거절하고 데이터를 다시 보낸다. 입력 없이 서버가 스스로 웅크림을 켜는 것과 `StopSneaking` 거절은 하지 않는다.

## 물약 색
- 투척 물약 LevelEvent 2002 색(ARGB): 야간 투시 C2FF66, 투명 F6F6F6, 점프 FDFF84, 화염 저항 FF9900, 신속 33EBFF, 느림 8BAFE0, 수중 호흡 98DAC0, 고통 A9656A, 독 87A363, 힘 FFC700, 시듦 736156, 저속 낙하 F3CFB9, 거북 도사 8D82E6(강화 8D85E6 → 저항 9146F0), 치유 F82423, 재생 CD5CAB, 나약함 484D48, 물 385DC6. 43~46(바람, 짜기, 스밈, 벌레)은 덤프에 없다.
- Anchor: 생성기가 공식 서버 색을 쓴다. 저속 낙하 물약(40, 41)은 덤프에 효과가 없어 물 색이다(`intended.txt`).

## 블록 충돌 상자
- 벽걸이 간판(`*_hanging_sign` `hanging=0`): facing 2·3 → `[0, 0.875, 0.375, 1, 1, 0.625]`, facing 4·5 → `[0.375, 0.875, 0, 0.625, 1, 1]`(기준 목록 출처 `blocks.json`의 `collisionShape`, 공식 서버에서 뽑은 값). 덤프 원본은 양쪽을 3/4씩 깎아 뒤집힌 상자(`[0, 0.875, 0.75, 1, 1, 0.25]`)를 만든다(3/8이 의도).
- Anchor: 생성기가 뒤집힌 축을 `min/2 ..= 1 - min/2`로 다시 만들어 공식 서버와 같다.

## 공식 서버 함정
- 재료에 태그 디스크립터 하나를 개수 2로 넣은 CraftRecipeAuto(`[Tag planks ×2]`)를 보내면 공식 서버가 응답하지 않고 멈춘다(`stop`도 듣지 않아 PID로 끝내야 함). 두 번 재현했다.
- 결제한 신호기 범위 안의 플레이어가 있으면 공식 서버가 패킷 송신과 콘솔 명령 처리를 멈추고, 그 플레이어가 나가야 풀린다(두 번 재현). `stations`는 끝에 신호기를 지운다.

## 열린 질문
- VoxelShapes 모양 내용은 공식 서버와 하나하나 대조하지 않았다(개수, 이름 순서, 인코드 크기만 같음).
- 창 없이 핫바·인벤토리 컨테이너(28·29)로 보내는 요청(아이템 버리기 등)은 시험하지 않았다. 클라이언트는 화면을 연 채로만 요청한다.
- 번호 입자 19 이상은 35 하나만 공식 서버에서 찾았다.
- 플레이어에게 보내는 tp(`tp <누구> <누구에게>`)의 successVictim 매개변수(도착 플레이어 이름)는 공식 서버에서 보지 않았다.
- `gamemode` 숫자 5·6을 공식 서버가 받는지 보지 않았다.
- 셋째 플레이어부터의 플레이어 목록 색은 보지 않았다.
- 투척 물약이 머리 위 높이에서 깨질 때 몇 틱 차이, 즉시 효과의 거리 식은 공식 서버로 재지 않았다.
- `cargo xtask p6 anchor snappy`처럼 한 압축만 돌리면 NetworkSettings를 zlib 공식 서버 캡처와 비교하는 문제가 있다. 두 압축을 함께 돌린다.

## 실제 클라이언트로만 확인
- 공식 클라이언트가 보내는 CraftRecipeAuto 재료 목록과 InventoryTransaction LegacyRequestID·LegacySetItemSlots 모양(공식 서버 재동기화의 원인일 수 있음).
- `recipe-book = false`에서 레시피 책이 비는지, `doLimitedCrafting` false에서 해금되지 않은 레시피를 제작 칸에서 만들 수 있는지.
- 공식 클라이언트 CommandRequest의 Version 값.
- 별칭 enum 순서, 플레이어별 `CommandName` enum이 명령 자동 완성에 맞게 보이는지.
- 번호 입자 형식: 클라이언트가 2009 형식(`id | 추가값 << 16`)과 `0x4000 | id` 형식을 둘 다 받는지.
- `EntityDataFlagCount` 131과 128~130번 플래그가 공식 클라이언트만 쓰는 값인지.
- 비·눈·천둥 하늘과 번개 모습·소리, 플래그 0 AddActor로 번개가 그려지는지.
- 멀리 있는(보이지 않는) 다른 플레이어의 위치 표시줄.
- 거래 때 Event EntityInteract를 보내지 않아도 클라이언트 동작에 차이가 없는지.
- 가져온 월드에서 Anchor가 모르는 화분 식물(벚나무 묘목 등 비활성 블록)이 보이는지. BlockActorData `PlantBlock`에 공식 서버가 저장한 원본 그대로를 보낸다. 봇 시나리오로는 놓을 수 없다(Anchor에 그 아이템이 없음).
- Anchor가 모르는 배너 무늬 ID를 BlockActorData `Patterns`로 받을 때 클라이언트가 그 층만 무시하는지.
- 천장이 낮은 곳에서 점프할 때 쉬프트 없이 웅크리지 않는지(웅크리기 답의 `Tick`).
- 서바이벌·크리에이티브에서 블록에 우클릭을 누르고 있을 때 `SimulationTick` 클릭 사이가 `PRESS_GAP_TICKS`(10틱)보다 벌어지는 일이 없는지(모험 모드에서만 봄).
