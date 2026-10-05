# M7 P6 사실 (BDS 1.26.52.3, 2026-10-03)

P 서브에이전트(단계 A)가 BDS에서 확인한 사실이다. 메인이 PLAN.md 10.7에 옮긴다. 출처는 `cargo xtask p6 game bds`(RakNet, zlib, 새 평지 월드)의 캡처(`target/p6/bds-game/`)이고, 같은 실행을 네 번 이상 반복해 `p6 game compare`가 SAME인 것까지 확인했다. 값이 실행마다 다른 것은 "변함"으로 적는다.

## 0. 시험 도구
- 봇 시나리오 `craft`, `furnace`, `enchant`, `crops`, `trade`(`xtask/go/bot/game.go`, `scenarios.go`). 봇이 공식 클라이언트처럼 서버 권위 인벤토리(ItemStackRequest), 아이템 사용·엔티티 상호작용(InventoryTransaction), 서버 권위 부수기(PlayerAuthInput 블록 액션)로 논다. 틱마다 PlayerAuthInput을 보내고, 클라이언트 쪽 모델(위치, 창별 칸과 스택 네트워크 ID, 열린 창, 블록, 엔티티)을 지닌다.
- 월드 준비는 콘솔 명령이다. 봇이 캡처에 `{"type":"command","line":...}`를 쓰면 하네스가 서버 콘솔에 넘긴다(`xtask/src/p6/game.rs`). 쓰는 명령: `tp`, `give`, `setblock`, `fill`, `xp`, `summon villager`, `effect`, `kill`, 세션 시작 때 `gamerule`(domobspawning·dodaylightcycle·doweathercycle false, randomtickspeed 0). Anchor 대조(단계 C)는 이 명령을 Anchor API로 옮기는 콘솔 함수 하나만 더하면 된다(`run_scenario`의 `console` 인자).
- 시나리오마다 새 플레이어(`P6Craft` 등)가 자기 구역(발 위치 x -12, y -60, z = -12·-4·4·12·20)으로 순간 이동한 뒤 논다. 절대 좌표가 같아 같은 평지의 두 서버를 그대로 비교한다.
- 비교(`xtask/src/p6/game/trace.rs`, `p6 game compare A B`): 봇의 단계(step)마다 정해진 줄을 정렬해 비교한다. 정해진 것: 명령, 요청 상태, ItemStackResponse(컨테이너·칸·개수·내구도·이름, 스택 ID는 처음 나온 순서 `#n`), 단계 끝의 아이템 상태(봇의 `state`), 위치별 마지막 블록 상태, ContainerOpen·Close(창 ID는 `w#n`), ContainerSetData 키 집합(전체), BlockActorData 키, LevelEvent 종류, Event 종류, 엔티티 종류(추가·제거·줍기), UpdateTrade 창 종류·크기·등급·플래그, 오퍼 NBT 중 모든 오퍼에 있는 키와 태그 형, 인챈트 선택지 수, 플레이어 레벨. 보이기만 하는 것: 인벤토리 패킷 자체(전달 방식), 소리, 채팅, 레시피 해금, SetActorData, 경험치 구슬·점수, ContainerSetData 값과 횟수, 드롭 위치, 오퍼·인챈트 내용. `setup` 단계(콘솔 명령 준비)는 명령만, `random` 단계(뼛가루 성장, 드롭 줍기, 무작위 직업 주민과 거래)는 이름·개수·스택 ID·칸을 `?`로 바꾼 모양만 비교한다.

## 1. 공통 (모든 시나리오)
- 로딩 화면: 클라이언트가 `ServerBoundLoadingScreen`(Start, End)을 보내기 전에는 BDS가 블록 클릭(UseItem ClickBlock)을 모두 거부한다(주변 블록과 플레이어 상태를 다시 보냄). 봇은 초기화 직후 둘을 보낸다. Anchor는 이 패킷을 받고 버린다(거부 조건으로 삼지 않음, PMMP·dragonfly와 같음).
- 요청 ID: 클라이언트처럼 -1부터 2씩 줄인다. BDS는 그대로 받는다. 같은 요청 안에서 바뀐 칸(만든 결과 칸 60:50, 같은 요청에서 비운 칸)의 StackNetworkID는 요청 ID다.
- 컨테이너 번호(ItemStackRequest와 응답): 창이 열려 있으면 인벤토리는 12(CombinedHotBarAndInventory), 커서 59, 제작 칸 13(2x2 28-31, 3x3 32-40), 만든 결과 60 칸 50, 화로 재료 25:0·연료 24:1·결과 26:2, 인챈트 입력 22:14·청금석 23:15, 거래 첫 재료 47:4. 응답에는 60이 나오지 않는다. 창 없이 28·29(핫바·인벤토리)로 보내는 경우는 시험하지 않았다(클라이언트는 화면을 연 채로만 요청한다).
- 응답 모양: `Status` 0, 액션 순서대로 바뀐 칸(출발 → 도착, 소비 칸 → 결과 칸), 칸마다 `Slot`=`HotbarSlot`, 개수, StackNetworkID(0이면 빈 칸), `CustomName` "", `FilteredCustomName` 없음, `DurabilityCorrection` 0.
- 스택 네트워크 ID 규칙(BDS): 서버 전체 하나의 카운터. 요청으로 스택 전체를 옮기면 ID가 따라간다. 일부를 옮기면 빈 도착 칸은 새 ID, 출발 칸은 그대로. 요청으로 개수만 바뀐 칸과 합쳐진 칸은 ID 유지. 만든 결과·인챈트 결과·거래 결과는 새 ID. 요청 밖의 변화(give, 아이템 사용으로 줄어듦, 줍기)는 새 ID.
- 창 ID: 플레이어마다 2부터 열 때마다 1씩(자기 인벤토리 창 포함, 거래도 같은 카운터). 닫기: 클라이언트 ContainerClose(창, 열린 종류, false) → BDS ContainerClose(같은 창, 종류 247(= -9 NONE), ServerSide false).
- 블록 클릭 뒤 재동기화: 아이템을 쓰는 클릭(설치, 뼛가루)마다 BDS는 바뀐 인벤토리(InventoryContent 0·120·124·119, PlayerHotBar select false)와 블록 갱신(클릭 블록, 놓인 블록, 아래·위 블록) 뒤에, 플레이어 전체 상태(UpdateAttributes 전체, SetActorData 전체, InventoryContent 4창, PlayerHotBar select true)와 놓인 블록을 한 번 더 보낸다. 클라이언트 예측 액션을 넣어도 같다. Anchor가 따라 할 필요는 없다(비교는 결과 상태만 봄).
- give(콘솔): BDS는 서버 → 클라이언트 InventoryTransaction(normal, 액션 두 개: 컨테이너 창 0 칸 N 빈 칸 → 아이템 새 ID, 출처 3(creative) 칸 1 아이템 → 빈 칸)과 InventoryContent 4창, 소리 `pop`, Text(`commands.give.successRecipient`)를 보낸다.
- Event 패킷(10.1 검증 필요): 다섯 시나리오와 spawn·blocks 전체에서 BDS가 보낸 Event는 거래 창을 열 때의 EntityInteract(종류 1) 하나뿐이다. 종류 8, 9, 10, 12, 14, 20, 21, 22는 한 번도 오지 않았다. → Anchor도 보내지 않는다(10.1 닫음). EntityInteract는 gophertunnel 서수로 디코드된다(ordinal 일치).

## 2. craft (제작)
- 순서: give 원목 4 → Interact(OpenInventory, 자기 런타임 ID) → BDS ContainerOpen(창 2, 종류 255(-1 인벤토리), 위치 = 플레이어 눈 블록, 엔티티 -1) → 원목 4를 제작 칸 13:28로 Place → 판자 4번 → 판자를 28-31에 한 개씩 Place 4번 → 작업대 → 반복 → 막대(레시피 책) → ContainerClose → 작업대 설치 2번 → 작업대 클릭 → ContainerOpen(창 3, 종류 1, 블록 위치) → 판자 32·33·34, 막대 36·39 → 곡괭이 → 닫기.
- 수동 제작 요청(모두 상태 0): `[CraftRecipe(레시피 네트워크 ID, 1), CraftResultsDeprecated([결과], 1), Consume(1, 13:칸)…, Place(개수, 60:50 ID=요청 ID, 12:칸 ID 또는 0)]`. 응답은 소비한 제작 칸들과 도착 칸.
- 레시피 책(CraftRecipeAuto): BDS가 받은 형태는 `[CraftRecipeAuto(ID, 1, 재료=[이름 디스크립터(oak_planks, meta 0) ×1, 같은 것 ×1]), CraftResultsDeprecated, Consume(1, 12:판자 칸) ×2, Place(4, 60:50, 12:빈 칸)]`. 레시피의 태그 디스크립터(`minecraft:planks` ×1 두 개)를 그대로 보내면 상태 25(FailedToMatchExpectedAllowedAnywhereConsumedItem). 공식 클라이언트의 정확한 재료 목록은 관찰할 수 없다(검증 필요(M9): 공식 클라이언트).
- BDS 결함: 재료에 태그 디스크립터 한 개를 개수 2로 넣은 CraftRecipeAuto(`[Tag planks ×2]`)를 보내면 BDS가 응답하지 않고 멈춘다(`stop`도 듣지 않아 PID로 종료). 두 번 재현. Anchor는 재료 목록을 믿지 않고(서버 레시피로 판정) 상한 검사만 한다.
- 10.4 검증 필요(닫음): BDS는 자기 CraftingData의 레시피 네트워크 ID만 쓴다(1~4768, 3,312개, 빈 번호 있음). CraftResultsDeprecated의 결과 아이템 블록 런타임 ID를 돌(stone)로 바꿔도 받는다. 설치할 때 HeldItem의 블록 런타임 ID를 돌로 바꿔도 작업대가 놓인다. → BDS는 클라이언트가 보낸 아이템 형태(결과 항목, HeldItem 블록 ID)를 검사하지 않고 서버 칸 내용과 스택 ID로 판정한다. Anchor도 같게: CraftResultsDeprecated는 무시하고, HeldItem은 네트워크 ID·개수·스택 ID로만 서버 칸과 맞추고 블록 런타임 ID 차이로 거부하지 않는다(10.4의 "형태가 다르면 None" 규칙은 요청 처리에 쓰지 않는다).
- 레시피 출력의 블록 아이템 형태: 작업대 출력 `BlockRuntimeID` 1752181952(= 크리에이티브 항목과 같은 바닐라 상태). 클라이언트는 이 형태를 예측해 들고 설치 요청에 쓴다.
- 작업대 설치: UseItem(ClickBlock, 바닥 블록 위면 1, HeldItem=작업대, 예측 액션) → 1절 재동기화, UpdateBlock 작업대 flags 3, 소리 `place`(ExtraData = 블록 런타임 ID). 제작 뒤 UnlockedRecipes(종류 2, 새로 해금된 레시피 ID 문자열)가 온다.

## 3. furnace (화로)
- 클릭 → ContainerOpen(창 2, 종류 2, 블록 위치, -1), InventoryContent(화로 창 빈 3칸)와 4창, ContainerSetData 키 0·1·2·3 값 0(열 때 한 번).
- 연료 Place(1, 12:석탄, 24:1) → 응답 [12:0 x0, 24:1 x1 같은 ID]. 재료 Place(2, 12:원석, 25:0) → 응답, 곧 InventorySlot(화로 창 칸 1 빈 칸), UpdateBlock `lit_furnace`(같은 방향), BlockActorData `{"LastFuel": 석탄 아이템 NBT(`WasPickedUp` 0 포함)}`. 열 때와 setblock 때는 BlockActorData를 보내지 않는다.
- 굽는 동안 매 틱 ContainerSetData 키 0(굽기 진행 0→199, 다 되면 0)과 키 1(남은 연료 틱, 1600에서 감소), 연료를 태울 때 키 2(연료 총 1600) 한 번. 한 개가 끝날 때마다 InventorySlot 칸 2(주괴, 처음 새 ID, 늘면 같은 ID)와 칸 0(원석 감소, 마지막은 빈 칸). 철 두 개 약 20초.
- 꺼내기 Place(2, 26:2, 12:빈 칸) → 응답 [26:2 x0, 12:0 x2 주괴 ID 그대로]. 경험치: 경험치 구슬 AddActor(1~2개, 무작위) → 플레이어가 주우면 LevelEvent 1051(구슬 줍는 소리), UpdateAttributes player.experience, RemoveActor.
- PMMP와 다른 점: PMMP는 바뀐 키(0·1·2)만 보내고 키 3·열 때 초기값을 보내지 않으며 화로 경험치를 주지 않는다(dragonfly는 구슬). BDS 블록 엔티티 데이터는 `LastFuel`뿐(PMMP 스폰 NBT에는 없음).

## 4. enchant (인챈트)
- 열기: ContainerOpen(창 2, 종류 3, 블록 위치, -1)과 함께 빈 PlayerEnchantOptions. 곡괭이를 22:14에 놓으면(청금석 없이도) 선택지 3개가 온다. 청금석을 23:15에 놓아도 다시 오지 않는다.
- 선택지 모양: `Cost`(필요 레벨, 무작위), `Enchantments{Slot 1024, Enchantments [[활성], [장착], [손에 듦]]}`(각 {Type, Level}), `Name`(무작위 단어), `RecipeNetworkID`(서버 카운터, 아래). 레벨 30과 책장 30개(유효 15)에서 Cost 예: 6·13·30(변함).
- 선택지 모양 규칙(G, 2026-10-04 캡처): `Slot`은 입력 아이템의 인챈트 슬롯 플래그(PMMP `ItemFlags` 값, 곡괭이 0x400 = 1024). 인챈트는 발동 방식별 목록에 든다: 0 착용(갑옷: 보호 계열, 가시, 호흡, 친수성, 차가운 걸음), 1 손에 든 아이템(날카로움, 발화, 섬세한 손길, 내구성, 행운, 화염), 2 아이템 자신(밀치기, 효율, 힘, 밀어내기, 무한, 수선, 소실 저주). 곡괭이에서 내구성(17)은 1번, 효율(15)은 2번 목록으로 왔다(gophertunnel 문서의 목록과 같음). 갑옷 플래그, 책의 `Slot`, 신속한 잠행의 목록은 검증 필요(M9).
- 인챈트 요청(상태 0): `[CraftRecipe(선택지 RecipeNetworkID, 1), Consume(1, 22:14), Consume(선택지 순서+1, 23:15), Place(1, 60:50 ID=요청 ID, 22:14 ID=요청 ID)]`. 청금석 개수는 선택지 순서 + 1(`Cost`는 레벨 조건). 응답 [22:14 x1 새 ID, 23:15 x2 같은 ID], 곧 빈 PlayerEnchantOptions, UpdateAttributes player.level 29(첫 선택지는 1레벨). 인챈트된 아이템의 NBT는 보내지 않는다(클라이언트가 선택지로 안다). CraftResultsDeprecated 없이 받는다.
- `RecipeNetworkID`·거래 `netId`는 CraftingData 마지막 번호(4768) 다음부터 서버 전체에서 하나씩 늘어나는 번호다(인챈트 4769~4771, 그 뒤 주민 오퍼 4772~). 같은 주민의 오퍼는 다시 열어도 같은 번호.

## 5. crops (작물)
- 뼛가루: UseItem(ClickBlock, 작물 블록 위면, HeldItem=뼛가루, 예측 액션) → LevelEvent 2005(ParticleCropGrowth, 블록 위치, data 0), InventoryContent(뼛가루 -1, 새 스택 ID), UpdateBlock 밀 `growth` 증가(무작위 1~3단계, 이번 실행 3~4번에 7), 1절 재동기화(밭 `moisturized_amount` 0 포함).
- 부수기(서버 권위): PlayerAuthInput 한 틱에 블록 액션 `[StartBreak(위치, 면 1), PredictDestroyBlock(위치, 면 1)]`(InputFlagPerformBlockActions). BDS: LevelEvent 3600(StartBlockCracking, data 65535), LevelEvent 2001(ParticlesDestroyBlock, 블록 중심, data = 부서진 블록 런타임 ID), UpdateBlock 공기, 드롭 AddItemActor(밀 1, 씨앗 0~3, 무작위, 각자 SetActorData·MoveActorDelta).
- 드롭 이동(G, 2026-10-04 캡처): AddItemActor와 MoveActorDelta의 위치는 발 + 0.125, MoveActorDelta `Ticks` 0, 떨어지는 동안 매 틱 보내고 바뀌지 않은 축은 없음(null). 경험치 구슬·주민의 MoveActorDelta도 `Ticks` 0.
- 줍기: 플레이어 상자 안에 들면 TakeItemActor(아이템 런타임 ID, 플레이어 런타임 ID) → 서버 → 클라이언트 InventoryTransaction(normal: 컨테이너 창 0 칸 N 이전 → 새 아이템 새 ID, 출처 2(world) 칸 1 아이템 → 빈 칸) → RemoveActor. InventorySlot은 보내지 않는다. 봇의 걷기는 서버 권위 이동이 되돌린다(CorrectPlayerMovePrediction, 물리를 재현하지 않으면) — 봇은 `tp`로 드롭 위에 선다.
- PlayerAuthInput 블록 액션 형식: gophertunnel 형식(모든 액션에 위치·면)의 StopBreak·AbortBreak를 BDS가 받는다(연결 유지). PMMP 프로토콜 라이브러리는 StopBreak에 위치를 읽지 않는다(1.26.30 기준이라 낡음). StartItemUseOn·StopItemUseOn을 PlayerAuthInput 블록 액션에 넣으면 BDS가 Disconnect 없이 연결을 끊는다(PMMP도 디코드 오류). → Anchor는 gophertunnel 형식대로 읽고, 허용하지 않는 액션 종류가 오면 그 패킷만 버린다(10.2 규칙).

## 6. trade (거래, 8.6 검증 필요 다섯 개)
- 소환: 콘솔 `summon villager x y z 0 0 minecraft:spawn_farmer 이름`이 villager_v2를 만든다(`villager_v2`라는 명령 이름은 문법 오류). 생성 이벤트는 직업을 정하지 못하고 직업은 무작위다(무직·니트윗이면 거래 없음 → 봇은 최대 네 번 다시 소환).
- 열기: InventoryTransaction UseItemOnEntity(Interact, 주민 런타임 ID) → ContainerOpen 없이 UpdateTrade(창 2부터, `WindowType` 15, `Size` 0, `TradeTier` 0, `VillagerUniqueID` 주민 고유 ID, `EntityUniqueID` -1, `DisplayName` "entity.villager.<직업>", `NewTradeUI` true, `DemandBasedPrices` true) → InventoryContent(거래 창, 빈 목록), Event EntityInteract(`EntityRuntimeID`=플레이어 고유 ID, UsePlayerID true, InteractionType 7, InteractionEntityType 115, InteractedEntityID 주민 고유 ID, EntityVariant 직업 번호), 주민 Animate.
- 오퍼 NBT(검증 필요 1 닫음): 루트 `{Recipes: list, TierExpRequirements: [{"0":0},{"1":10},{"2":70},{"3":150},{"4":250}]}`(각 원소는 키가 등급 문자열인 int 하나). 오퍼마다 `buyA`(compound: `Count` byte, `Damage` short(32767 = 아무 meta), `Name` string, `WasPickedUp` byte, 블록 아이템이면 `Block{name, states, version}`), `buyB`(같은 모양, 있을 때만), `buyCountA` int(= `buyA.Count`), `buyCountB` int, `demand` int, `maxUses` int, `netId` int, `priceMultiplierA`·`B` float(0.05, 비싼 것 0.2), `rewardExp` byte(1), `sell`(같은 모양 + 인챈트면 `tag{ench:[{id,lvl}]}`), `tier` int(0~4), `traderExp` int, `uses` int. 모든 등급의 오퍼가 처음부터 들어 있고 `TradeTier`가 열린 등급을 정한다.
- 거래 요청(검증 필요 2 닫음, 모두 상태 0): 지불 `[Place(buyCountA, 12:칸, 47:4 ID 0)]` → 응답 [12:칸, 47:4 새 ID]; 거래 `[CraftRecipe(오퍼 netId, 1), CraftResultsDeprecated([sell], 1), Consume(buyCountA, 47:4), Place(sell 개수, 60:50 ID=요청 ID, 12:칸)]` → 응답 [47:4 x0, 12:칸 결과 새 ID]. `netId` = CraftRecipe의 RecipeNetworkID.
- 주민 메타데이터(G, 2026-10-04 캡처): AddActor `villager_v2`에 101 TradeTier 0, 102 MaxTradeTier 4, 103 TradeExperience 0, 104 SkinId(이번 실행 0, F2 실행에서 4도 봄: 무작위). 거래 뒤 SetActorData에 바뀐 키만(예: `{103: 5}`).
- 레거시 villager(검증 필요 3 닫음): `summon minecraft:villager`도 AddActor 종류 `minecraft:villager_v2`로 온다(소환 때 바뀜). BDS 1.26.52에는 레거시 actor가 없고 그 주민도 Trade2 창이 정상으로 열린다. → Anchor 주민형 NPC의 네트워크 actor 종류는 `minecraft:villager_v2` 하나.
- DemandBasedPrices(검증 필요 4 닫음): 늘 true. 새 주민의 `demand` 0, 거래 한 번 뒤에도 0.
- 거래 뒤 갱신(검증 필요 5 닫음): 거래 뒤 BDS는 UpdateTrade를 다시 보내지 않는다(클라이언트가 스스로 반영). 다시 열면 거래한 오퍼만 `uses` +1이고 나머지(가격, demand, 배수)는 그대로다. 보상: 주민 위치에 경험치 구슬(rewardExp에 따라, 무작위 개수)을 소환하고 플레이어가 줍는다. 주민 쪽 SetActorData가 여럿 온다(거래 경험·애니메이션).
- 닫기: 클라이언트 ContainerClose(창, 15, false) → BDS ContainerClose(창, 247, false).

## 7. 스폰 창의 M7 유예 패킷 (BDS 순서와 값)
- 순서: NetworkSettings → ServerToClientHandshake → PlayStatus(0) → ResourcePacksInfo → ResourcePackStack → LevelEventGeneric → PlayerList → SyncWorldClocks → JigsawStructureData → VoxelShapes → StartGame → SyncActorProperty ×13 → ItemRegistry → (RequestChunkRadius) → SetSpawnPosition → SyncWorldClocks → SetDifficulty → SetCommandsEnabled → UpdateAdventureSettings → UpdateAbilities → GameRulesChanged → PlayerList → UpdateAbilities → BiomeDefinitionList → AvailableActorIdentifiers → PlayerFog → CameraPresets → CameraAimAssistPresets → CameraSpline → UpdateAttributes → CreativeContent → TrimData → InventoryContent ×4 → PlayerHotBar → CraftingData → AvailableCommands → Respawn ×3 → (청크) → LevelSoundEvent → SetActorData → CurrentStructureFeature → UpdateAttributes → ChunkRadiusUpdated → (주변 몹 AddActor 등) → SetHealth → PlayStatus(3). SetLocalPlayerAsInitialised 뒤 UnlockedRecipes(종류 1, 20개)가 온다(창 밖).
- LevelEventGeneric: EventID 9801(SleepingPlayers), 데이터 = 네트워크 NBT 컴파운드 내용(루트 태그 없이 int 셋과 끝 0): `ableToSleep` 1, `overworldPlayerCount` 1, `sleepingPlayerCount` 0(1인).
- PlayerList(두 번, 같은 내용): ActionType Add, 항목 하나(자기): UUID, EntityUniqueID(플레이어 고유 ID), Username, XUID "", PlatformChatID "", BuildPlatform 1, Skin 전체, Teacher·Host·SubClient false, PlayerColour {237,237,237,255}.
- SyncWorldClocks: 첫 번째 PayloadType 0(SyncStates [{ClockID, Time, Paused false}]), 두 번째 PayloadType 1(Clocks [{ID, Name "minecraft:overworld", Time, Paused, TimeMarkers 6개: sunrise 23000, night 13000, noon 6000, midnight 18000, day 1000, sunset 12000, 모두 Period 24000}]). 마커 ID는 고정값(실행마다 같음).
- VoxelShapes: NameMap 220, Shapes 287, CustomShapeCount 0(CloudburstMC `voxel_shapes.json`과 개수 같음, 내용 대조는 검증 필요(M7 단계 C)).
- SyncActorProperty 13개: zombie_nautilus, wolf, sulfur_cube, chicken, creaking, pig, cat, egg, armadillo, happy_ghast, copper_golem, bee, cow의 속성 정의(바닐라 몹). 순서와 내용이 CloudburstMC `entity_properties.nbt`와 같다 → 2026-10-04부터 그 파일로 보낸다.
- AvailableActorIdentifiers: 8,907바이트, CloudburstMC `entity_identifiers.dat`과 바이트가 같다 → 그 파일을 그대로 보낸다.
- SetDifficulty 2(normal, 설정 값). UpdateAdventureSettings {NoPvM false, NoMvP false, ImmutableWorld false, ShowNameTags false, AutoJump true}. UpdateAbilities(두 번 같은 값) {EntityUniqueID 플레이어, PlayerPermissions 1(member), CommandPermissions 0, Layers [{Type 1(base), Abilities 1048575, Values 63, FlySpeed 0.05, VerticalFlySpeed 1, WalkSpeed 0.1}]}.
- GameRulesChanged 39개(이름, 값, CanBeModifiedByPlayer true): commandBlockOutput, doDayLightCycle, doEntityDrops, doFireTick, recipesUnlock, doLimitedCrafting(false), doMobLoot, doMobSpawning, doTileDrops, doWeatherCycle, drowningDamage, fallDamage, fireDamage, keepInventory(false), mobGriefing, pvp, showCoordinates(false), playerWaypoints(int 1), locatorbar, showDaysPlayed(false), naturalRegeneration, tntExplodes, sendCommandFeedback, maxCommandChainLength(65535), doInsomnia, commandBlocksEnabled, randomTickSpeed(1), doImmediateRespawn(false), showDeathMessages, functionCommandLimit(10000), spawnRadius(10), showTags, freezeDamage, respawnBlocksExplode, showBorderEffect, showRecipeMessages, playersSleepingPercentage(100), projectilesCanBreakBlocks, tntExplosionDropDecay(false). 나머지 bool은 true.
- UpdateAttributes 첫 번째(Tick 0): 17개(player.hunger 20/0~20, saturation 5, exhaustion 0, level 0/0~24791, experience 0/0~1, health 20, follow_range 16/0~2048, knockback_resistance 0/-2~1, movement 0.1, underwater_movement 0.02, lava_movement 0.02, attack_damage 1/1~1, absorption 0/0~16, luck 0/-1024~1024, friction_modifier 1/0~256, bounciness 0/0~1, air_drag_modifier 1/0~256; Default = 값, 수정자 없음). 두 번째는 57항목으로 같은 속성이 여러 번 겹쳐 있다(BDS 특성, 따라 하지 않음).
- SetHealth 20(PlayStatus(3) 바로 앞). PlayerFog 빈 스택. Respawn 세 번: (State 0, y 32769.62), (State 0, y 32768), (State 1, 실제 눈 위치), EntityRuntimeID 0.
- SetActorData(플레이어, Tick 0) 72개 키: 0 flags 844424930656256, 1 1, 2 0, 3 0, 4 이름, 5 -1, 6 0, 7 300, 22 0, 26 0, 27 1, 28 [0,0,0], 37 0, 39 0, 42 300, 43~46 0, 55 -1, 57~60 0, 70 0, 71 "", 72 "", 73 1, 75~77 0, 78 -1, 81 1, 82 0, 84 "", 88 -1, 91 0, 92 35184405643264, 100 "", 101~104 0, 106 3, 107 1, 108 8, 109 16, 110 "ambient", 111 1, 114~117 0, 118 {}, 119 0, 120 0, 124 1, 125 20, 126 614, 127 [0,0,0], 128 0, 129 0, 130 [0.6,1.8,0], 131 0, 132 "", 133 [0,0,0], 134 0, 135 0, 136~138 -1, 140 64. 키 이름은 단계 B가 gophertunnel 상수로 붙인다.
- InventoryContent 4개(Container 0): 창 0(36칸), 120(갑옷 5칸), 124(UI 54칸), 119(왼손 1칸), 모두 빈 칸. PlayerHotBar {0, 창 0, select true}.
- TrimData: 무늬 18개(ward, sentry, snout, dune, spire, tide, wild, rib, coast, shaper, eye, vex, silence, wayfinder, raiser, host, flow, bolt; ItemName = `minecraft:<무늬>_armor_trim_smithing_template`), 재료 11개(quartz §h, iron §i, netherite §j, redstone §m, copper §n, gold §p, emerald §q, diamond §s, lapis §t, amethyst §u, resin §v; ItemName은 재료 아이템).
- MobEquipment·MobArmourEquipment: 스폰 창에는 오지 않는다(주민·몹이 보일 때만).
- 스폰 창의 AddActor·RemoveActor·MoveActorDelta 등은 BDS 평지의 바닐라 몹(닭, 소, 양)이다.

## 8. Anchor에 주는 결정 (제안)
- ItemStackRequest 처리: 1절 컨테이너 번호, 요청 ID 규칙(같은 요청에서 바뀐 칸은 요청 ID), 스택 ID 규칙(1절)을 BDS와 같게. 응답 칸 순서는 액션 순서. CraftResultsDeprecated는 무시. CraftRecipeAuto는 재료 목록을 믿지 않고 서버 레시피로 인벤토리에서 소비를 검사.
- 화로: ContainerSetData 키 0·1·2(PMMP와 같은 뜻), 열 때 0~3 초기값. 경험치는 dragonfly처럼 꺼낼 때 구슬(PMMP에 없음, 1절 범위 기준은 PMMP지만 BDS·dragonfly가 같음 → 단계 C가 정함).
- 인챈트: 선택지 RecipeNetworkID와 거래 netId는 CraftingData 마지막 번호 다음부터 서버 카운터. 청금석 = 순서+1, 레벨 = 순서+1.
- 거래: 8.6 그대로 + 위 오퍼 키 전체, `uses`만 늘리고 UpdateTrade 재전송 없음, actor 종류 villager_v2, 창 종류 15, 닫기 응답 종류 247.
- 줍기 알림은 InventoryTransaction(normal)으로 보내도 되고 InventorySlot이어도 된다(비교는 결과 상태). 블록 클릭 뒤 전체 재동기화는 따라 하지 않는다.
- Event: 8, 9, 10, 12, 14, 20, 21, 22는 보내지 않는다(10.1 닫음). 거래 때 EntityInteract는 선택(검증 필요(M8): 업적·통계용으로 보이며 클라이언트 동작에 영향 없음).

## 9. 남은 검증 필요
- 공식 클라이언트의 CraftRecipeAuto 재료 목록과 InventoryTransaction LegacyRequestID·LegacySetItemSlots 모양(BDS 재동기화의 원인일 수 있음)은 봇으로 관찰할 수 없다.
- 창 없이 핫바·인벤토리 컨테이너(28·29)로 보내는 요청(아이템 버리기 등)은 시험하지 않았다.
- VoxelShapes 내용과 CloudburstMC 대조, TrimData를 덤프와 대조(단계 C).
