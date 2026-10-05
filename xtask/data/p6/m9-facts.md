# M9 P6 사실 (BDS 1.26.52.3)

M9 서브에이전트가 BDS에서 확인한 사실이다. 메인이 PLAN.md 10.9에 옮긴다. 에이전트마다 절을 따로 둔다.

## SWITCH-B: 레시피 책 (`recipe-book = false`)
- 출처: `cargo xtask p6 game bds`의 `craft` 캡처(2026-10-04, RakNet, `target/p6/bds-game/craft.items.jsonl`의 CraftingData, `craft.jsonl`의 UnlockedRecipes). 새 BDS 실행 없이 이 캡처에서 셌다.
- BDS 게임 규칙: `recipesUnlock` true, `doLimitedCrafting` false(m7-facts 7). Anchor도 같게 보낸다.
- BDS CraftingData의 해금 조건(`UnlockRequirement`): 모양 있는 1,217개 중 문맥 0(None)·재료 있음 1,022, 문맥 0·재료 없음 181, 문맥 2(물속) 11, 문맥 1(항상 해금) 2, 문맥 3(아이템 많음) 1. 모양 없는 2,068개 중 문맥 0·재료 있음 1,691(제작대·석재 절단기·화로 계열·지도 제작대), 문맥 0·재료 없음 359, 문맥 1 18(블록 `deprecated`). 문맥 0·재료 없음 540개(예: `minecraft:barrel`, `minecraft:grindstone_from_crimson_planks2`, `minecraft:Torch_charcoal_recipeId`)는 재료로 해금되지 않고 서버의 UnlockedRecipes로만 해금된다.
- BDS는 SetLocalPlayerAsInitialised 뒤 UnlockedRecipes(종류 1, 20개)를 보낸다. 20개는 문맥 1 레시피 전부(제작대 2개와 `deprecated` 화로 18개)다. 재료를 얻으면 종류 2로 새로 해금된 것을 보낸다(통나무를 받자 `minecraft:oak_planks` 등 3개, 판자를 만들자 16개).
- 결정: `recipe-book = false`면 Anchor는 모든 레시피를 문맥 0·재료 없음(BDS가 서버만 해금하는 레시피에 주는 모양)으로 보내고, UnlockedRecipes는 보내지 않는다(원래 보내지 않음). 그러면 클라이언트가 스스로 해금할 것이 없어 책이 빈다. 제작 칸 제작(CraftRecipe)은 서버가 해금 여부를 보지 않으므로 그대로 되고, 레시피 책 빠른 제작(CraftRecipeAuto)은 거부한다.
- 실제 접속 확인 대기: 공식 클라이언트의 레시피 책이 이 모양에서 비는지, `doLimitedCrafting` false에서 해금되지 않은 레시피를 제작 칸에서 만들 수 있는지(봇은 클라이언트 화면을 볼 수 없다).

## WEATHER: 날씨와 번개
- 출처: `cargo xtask p6 api bds weather joinrain joinstorm`(2026-10-05, RakNet, `target/p6/bds-api/`) 다섯 번. 세션 설정은 `game::SESSION_SETUP`(`doweathercycle` false), 난이도 normal.
- 날씨 이벤트는 LevelEvent(위치 0,0,0)다: 비 시작 3001, 천둥 시작 3002, 비 멈춤 3003, 천둥 멈춤 3004. 시작의 데이터는 65535, 멈춤은 0이다. GameRulesChanged와 다른 패킷은 없다.
- BDS는 비·번개 수준을 약 5초에 걸쳐 올리고 내리며, 명령이 보내는 이벤트는 그때 수준에 따른다(수준이 다 오르기 전 `weather thunder`는 3001을 다시 보내고, 천둥 수준이 남은 동안 `weather clear`는 3004도 보낸다). 수준이 자리 잡은 뒤(각 단계 7초)는 바뀐 것만 보낸다: 맑음→비 3001, 비→비 없음, 비→천둥 3002, 천둥→천둥 없음, 천둥→비 3004, 비→맑음 3003, 맑음→천둥 3001·3002, 천둥→맑음 3003·3004, 맑음→맑음 없음. Anchor는 수준 변화가 없어 이 표를 쓴다(`src/tick/weather.rs`).
- 접속: StartGame `RainLevel`·`LightningLevel`이 그때 수준이고(자리 잡으면 비 1·0, 천둥 1·1), 스폰 창의 SetCommandsEnabled 바로 뒤에 비면 3001(데이터 = 비 수준 × 65535), 천둥이면 3002만(데이터 = 번개 수준 × 65535) 보낸다. 맑으면 없다. 수준이 다 오르기 전 접속은 0.85·55704처럼 중간 값이다.
- `summon lightning_bolt x y z`(정수 좌표): AddActor `minecraft:lightning_bolt`가 블록 바닥 가운데(x+0.5, y, z+0.5)에 기본 메타데이터(플래그 0)로 오고, 같은 배치에서 SetActorData가 오는데 플래그 0번(불붙음)이 켜진 실행과 키가 없는 실행이 섞인다. 다음 배치에 불 블록(UpdateBlock 또는 UpdateSubChunkBlocks, `minecraft:fire` age 0)과 LevelSoundEvent `thunder`, `explode`(EntityType `minecraft:lightning_bolt`, EntityUniqueID = 번개, ExtraData -1)가 오고, RemoveActor는 스트라이크 뒤 20~40틱 사이에 온다.
- 번개 2블록 옆의 봇(생존): 스트라이크 틱에 체력 20→15(5 피해, ActorEvent hurt 데이터 24 = 번개 원인; Anchor는 원래 데이터 0을 보낸다)와 불붙음 플래그, 그 뒤 20틱마다 1 불 피해. 불은 스트라이크 블록과 주변 1칸 안 무작위 몇 곳(4곳까지 봄)에 붙는다. 8블록 밖은 피해 없다.
- 난이도(M9 후속 R6, `cargo xtask p6 api bds boltpeaceful bolteasy bolthard`, 2026-10-05, 한 번): 각 시나리오가 `difficulty <난이도>` 뒤 봇 2블록 옆에 번개를 친다. 피해와 불붙음은 peaceful·easy·hard 모두 5 피해·불붙음(normal과 같음). 불은 peaceful·easy에서 없고(블록 갱신 패킷 0개) hard에서 맞은 블록에 붙는다(normal과 같음). dragonfly(`FireSpreadIncrease() >= 10`)와 자바판도 normal·hard만 불. → Anchor `Lightning::fire`는 월드 난이도가 normal·hard일 때만 불을 놓는다(`p6 api anchor bolt*` SAME).
- 천둥 동안 BDS는 플레이어 근처에 무작위 번개를 친다(7초씩 두 단계에 한 번 봄). Anchor는 자연 번개가 없다(자연 날씨 없음, 사용자 결정).
- 비가 오면 BDS는 불붙은 봇을 끈다(fizz 소리, LevelEvent 2029). Anchor는 비의 게임 효과(불 끄기 등)를 구현하지 않았다(PMMP에도 없음). 검증 필요(M9): 범위에 넣을지.
- 비교: 봇의 `weather` 확인(이벤트 순서·데이터, 번개 종류·위치·소리·제거·불·피해·불붙음)과 `weather at join` 확인이 같다(`p6 api anchor weather joinrain joinstorm` SAME). 번개의 불붙음 플래그와 무작위 번개는 비교하지 않는다.
- 실제 접속 확인 대기: 공식 클라이언트에서 비·눈·천둥 하늘과 번개 모습·소리가 보이는지, 플래그 0 AddActor로 번개가 그려지는지.

## STATIONS: 숫돌 (사용자 예외)
- 출처: `cargo xtask p6 game bds grindstone`(2026-10-05, RakNet, `target/p6/bds-game/`) 열 번(요청 모양 조사 포함), 마지막 모양으로 BDS 두 번·Anchor 두 번. 스폰 캡처(`spawn.items.jsonl`)의 ItemRegistry·CreativeContent·CraftingData. BDS 비헤이비어 팩의 레시피 원문(`vanilla/__brarchive/recipes.brarchive`, `grindstone.json`)은 읽기만 했다.
- 데이터: PMMP 덤프에 숫돌이 없다(`deserialized.json`이 기준 상태 16개를 모두 거부). 상태는 기준 목록의 16개(`attachment` standing·hanging·side·multiple × `direction` 0~3)이고 네트워크 해시는 기준 목록 그대로다. 아이템 런타임 ID -195, 아이템이 보내는 블록은 standing·direction 0(해시 1003295416). 크리에이티브는 `items` 분류의 이름 없는 묶음, `minecraft:damaged_anvil` 다음·`minecraft:enchanting_table` 앞(인덱스 1455). 레시피는 모양 있는 4개: `|_|`·`# #`(막대, `minecraft:normal_stone_slab` 아무 메타, 판자)이고 판자 칸이 태그 `minecraft:planks` 1개와 진홍·맹그로브·뒤틀린 판자 각 1개. BDS가 함께 보내는 블록 `deprecated` 1×1 레시피 9개(`grindstone_from_*_planks2~4`: 숫돌 → 숫돌)는 PMMP 데이터처럼 뺀다. Anchor는 이것을 `xtask/data/exceptions.json`(gen이 PMMP 덤프 뒤에 더함)에 둔다.
- 놓기: 동쪽을 보는 봇이 바닥 윗면을 누르면 standing·direction 1(서쪽 = 봇 쪽), 블록의 서쪽 면을 누르면 side·direction 1(누른 면 쪽). 방향 값은 남·서·북·동 순서(0~3).
- 받침: 벽(옆) 블록이나 바닥 블록을 `setblock ... air`로 없애면 그 숫돌은 부서지고(LevelEvent 2001, UpdateBlock 공기) 숫돌 아이템 하나가 떨어진다(AddItemActor). 손으로는 거두지 못하는 블록이지만 받침이 없어 부서질 때는 떨어뜨린다(dragonfly와 같음). 충돌 없는 블록이 받침인지는 BDS로 보지 않았고 dragonfly(충돌 없는 블록은 받침 아님)를 따른다.
- 창: 누르면 ContainerOpen 종류 26, 블록 엔티티와 BlockActorData는 없다. 입력 칸은 컨테이너 50(GrindstoneInput) UI 16, 추가 칸은 51(GrindstoneAdditional) UI 17(dragonfly). 칼을 넣고 빼는 Place 요청은 상태 0.
- 갈기 요청: 봇이 만들 수 있는 모양을 모두 BDS가 상태 9(InvalidCraftResult)로 거부했다. 시도: CraftGrindstoneRecipe(레시피 ID 0, 수선 다중 레시피 4766, `...00c8` 4757, 1~약 480 전부, 비용 0~80·23·12) + CraftResultsDeprecated(결과 NBT: 저주만 남김, `RepairCost` 0·1, `Damage` 없음, 저주 없음, NBT 없음) + Consume + 만든 칸에서 Place/Take(컨테이너 60, 또는 52 칸 50·18·2), 결과·Consume을 뺀 것, CraftResultsDeprecated를 먼저 둔 것(상태 3). NumberOfCrafts 0이나 비용 -1은 BDS가 연결을 끊는다. 그래서 갈기 결과·경험치는 BDS로 확인하지 못했고 dragonfly `handleGrindstoneCraft`(저주 아닌 인챈트 제거, 둘이면 내구도 합 + 최대의 5%, 저주는 둘 다의 것, `RepairCost` 지움, 경험치 = 인챈트 최소 힘 합의 절반 올림 + 그만큼까지 무작위, 구슬은 플레이어 눈 위치)를 따르되, dragonfly와 다른 점 네 가지를 기록해 둔다: 경험치는 두 아이템 모두의 인챈트로 셈(자바판 `GrindstoneMenu`), 인챈트 레벨은 그 인챈트의 최대 레벨(`max_level`)로 잘라 셈, 지울 인챈트가 없는 아이템 하나는 거부, 인챈트가 다 지워진 인챈트된 책은 책이 됨(자바판 동작). 모두 `src/tick/inventory/station/grindstone.rs`. Anchor는 레시피 ID와 비용을 보지 않고(dragonfly와 같음) 칸에서 결과를 낸다.
- 비교: 놓기·받침·창 열기·칼 넣고 빼기가 BDS와 같다(`p6 game anchor grindstone` SAME). 갈기 요청은 비교하지 않는다.
- 실제 접속 확인 대기: 공식 클라이언트의 숫돌 요청 모양(레시피 ID, 비용, 결과 NBT), 그 요청에 대한 BDS 결과(인챈트된 책 포함)와 경험치 구슬(양·위치). 이것으로 위 dragonfly 규칙을 BDS와 맞춘다.

## P6V: BDS로 닫은 검증 필요 (10.9 표 11~21·34·35 후속·38, 물약 색, SWITCH-A)
- 출처: `cargo xtask p6 api bds`(2026-10-05, RakNet, zlib, 새 평지 월드, `allow-cheats=true`, 세션 설정 `game::SESSION_SETUP`) 전체 실행과 시나리오별 재실행. 봇 시나리오는 `xtask/go/bot/verify.go`(`enchants`, `stations`, `potions`, `entities`, `villagers`, `numbers`, `syntax`, `limits`, `limitchat`, `line<바이트>`, `others`+`mover`, `rules`, `rulesjoin`, `tradeup`). 캡처는 `target/p6/bds-api/`. 같은 값이 두 번 이상의 실행에서 같을 때만 적었다.
- 하네스 결함 고침: 봇 판정이 게임 시나리오 목록을 손으로 적어, 목록에 없는 시나리오(weather 등)는 스크립트가 실패해도 통과했다 → `isGame`으로 판정(STATIONS와 같은 수정).
- BDS 함정: 결제한 신호기 범위 안의 플레이어에게 BDS가 패킷 송신과 콘솔 명령 처리를 멈추고, 그 플레이어가 나가야 풀린다(재현 2번). `stations`는 끝에 신호기를 지운다.

### 11 벽걸이 간판 충돌 상자
- BDS 상자(CloudburstMC `blocks.json` `collisionShape`, BDS에서 뽑은 값): `*_hanging_sign` `hanging=0`, facing 2·3 → `[0, 0.875, 0.375, 1, 1, 0.625]`, facing 4·5 → `[0.375, 0.875, 0, 0.625, 1, 1]`. PMMP는 양쪽을 3/4씩 깎아 뒤집힌 상자(`[0, 0.875, 0.75, 1, 1, 0.25]`)를 만든다(3/8이 의도). → gen이 뒤집힌 축을 `min/2 ..= 1 - min/2`로 다시 만들어 BDS와 같다.

### 12 입자 번호, `UnWaxOxidised`, `EntityDataFlagCount`
- 번호 입자: 바람 돌격(wind charge)은 LevelEvent 3614(데이터 1)이고 2009가 아니다. 19 이상 번호를 BDS가 내는 곳은 물약 구름의 `DataParticle`(63) = 35 하나를 찾았다(PMMP 번호로 `MOB_SPELL_AMBIENT`, gophertunnel 번호로는 `Ink`). 14·15(M8)와 함께 PMMP 번호가 BDS 번호다 → Anchor 입자 API를 PMMP 번호로 바꿈(투사체의 14·15와 한 곳).
- 구리: 도끼로 밀랍 구리를 긁으면 LevelEvent 2031(데이터 0), 산화 구리는 2032, 꿀벌집으로 밀랍칠은 2030. 함께 오는 텔레메트리 Event `WaxedOrUnwaxedCopper`의 `CopperBlockID`는 레거시 블록 ID(밀랍 구리 599, exposed 600, weathered 601, oxidized 701, 구리 블록 595)이고 gophertunnel 상수(42505…, `UnWaxOxidised` 0xfa0a)가 아니다. Anchor는 이 Event를 보내지 않는다(M7 결정) → 영향 없음. PMMP는 긁기에 LevelSoundEvent(`copper.wax.off`)를 쓰고 BDS는 LevelEvent만 보낸다(소리 경로는 PMMPC 담당 영역, 남은 차이).
- `EntityDataFlagCount` 131: BDS 플래그는 키 0·92 두 int64(비트 0~127)로만 왔다. 128~130번 플래그와 131 길이의 비트셋을 BDS가 보내는 곳은 없었다 → 실제 접속 확인 대기(공식 클라이언트만 쓰는 값인지).

### 13 인챈트 선택지 `Slot`
- 투구 1, 흉갑 2, 레깅스 8, 부츠 4(PMMP `ItemFlags`는 레깅스 4·부츠 8), 책 -1, 검 16, 활 32, 괭이 64, 도끼 512, 곡괭이 1024, 삽 2048, 낚싯대 4096, 삼지창 32768, 쇠뇌 65536. 가위·부싯돌은 선택지 없음. 목록 번호(0 착용, 1 손, 2 자신)는 Anchor 표와 같다(보호 계열·가시·호흡 0, 날카로움·발화·약탈·화염·섬세한 손길·내구성·바다의 행운 1, 효율·밀치기·힘·밀어내기·무한·미끼·충성·집전·관통·빠른 장전 2). → Anchor `Slot`을 BDS 값으로.
- 삼지창: BDS는 선택지 3개, Anchor는 없음(PMMP에 삼지창 인챈트가 없다: intended).

### 14 다른 플레이어의 이동
- BDS는 걷기·제자리 돌기·고개 들기 모두 다른 플레이어에게 MovePlayer mode 0으로 보낸다(MoveActorDelta 아님). tp는 mode 2. Anchor와 같다(`moves of the other player` SAME). 리스폰 뒤 이동도 mode 0.

### 15 거래 등급 상승과 demand
- 창을 연 채 거래로 등급이 오르면(10번째 거래에서 0→1) BDS는 UpdateTrade를 다시 보내지 않고 주민 SetActorData(101 TradeTier)만 보낸다. Anchor는 다시 보내던 것을 고침(`tradeup` SAME).
- demand≠0: BDS의 demand는 주민이 작업대에서 재입고할 때만 바뀌고(몹 AI), 거래만으로는 0이다(M7). Anchor는 주민 AI가 없어 늘 0 → 가격 표시는 확인할 경우가 없다(범위 밖).

### 16 신호기·모루·대장장이 요청 모양(모두 상태 0)
- 모루 이름 바꾸기: FilterStrings `["새 이름"]`, `[CraftRecipeOptional{RecipeNetworkID 0, FilterStringIndex 0}, Consume(1, AnvilInput:1), Place(1, Created:50, 인벤토리)]`. 레벨 30→29.
- 대장장이(네더라이트 검): `[CraftRecipe{스미딩 레시피 ID, 1}, Consume(1, SmithingTableInput:51), Consume(1, SmithingTableMaterial:52), Consume(1, SmithingTableTemplate:53), Place(1, Created:50, 인벤토리)]`.
- 신호기: 지불 칸 BeaconPayment:27에 철 주괴를 먼저 놓고 `[BeaconPayment{1, 0}, Destroy(1, BeaconPayment:27)]`.
- Anchor도 같은 모양을 받는다(`stations` SAME).

### 17 메타데이터 키 136~140, 구름
- 타입: 136 int32, 137 int32, 138 int32(Anchor는 int64였음 → 고침), 139 int64(다른 플레이어 AddPlayer에만), 140 float32. 127(마지막 사망 위치)은 BlockPos, 133(침대 들어간 위치)은 Vec3로, Anchor는 둘이 바뀌어 있었다 → 고침.
- 잔류형 재생 물약 구름 AddActor: 61 반경 3.0, 8 색, 63 입자 35, 95 지속 600, 98 줍기당 반경 −0.5, 99 줍기 수 0(int32), 97 0.00083333, 36 물약 ID 28(int16), 플래그 HasGravity+FireImmune, 그 밖에 모든 액터 공통 키. Anchor는 9·62·96을 안 보내고 63·95·98·99를 BDS 값으로(97은 0으로 두고 서버가 반경을 줄여 보냄, 36은 구름이 물약 ID를 지니지 않아 남은 차이).

### 18 다른 플레이어 AddPlayer
- 키 집합: 그 플레이어 자신의 SetActorData 72키 + 10(u8 0), 38(f32 1), 53·54(f32 너비·높이), 56(Vec3 0), 74(u8 0), 139(i64 0) = 79키. 플래그는 그때 상태(갓 들어온 플레이어는 Breathing 없음, 이미 있던 플레이어는 있음). GameType 5, BuildPlatform·DeviceID는 그 클라이언트 값, 능력 층은 base(로딩 중인 플레이어에게는 Type 5 로딩 화면 층 하나 더: 남은 차이). PlayerIndex(27)는 들어온 순서(1, 2), 플레이어 목록 색은 첫 플레이어 #EDEDED, 둘째 #54FFFF(셋째부터는 보지 못함: Anchor는 모두 #EDEDED, 남은 차이).
- Anchor는 키 9개만 보내던 것을 BDS 키 집합·타입·장치 값으로 고침(`others`·`mover` SAME). 사람 NPC는 BDS에 없어 대조 못 함(PMMP 모양 유지).

### 19 명령 출력 번역과 문법 오류
- `%createWorldScreen.gameMode.*`(넷 다), `%potion.regeneration`·`%potion.jump`·`%potion.nightVision`: Anchor와 같다.
- 문법 오류 `commands.generic.syntax` [앞, 틀린 낱말, 뒤]: 앞은 `/`부터 센 줄에서 틀린 낱말 앞 최대 10글자, 뒤는 낱말 뒤 최대 10글자(`/give @s no_such_item 3 and more words` → `["/give @s ","no_such_item"," 3 and mor"]`). 인자가 모자라면 낱말 ""과 줄 끝 앞 10글자(`/give @s` → `["/give @s","",""]`). 오버로드가 여럿이면 가장 멀리 맞은 곳(`/effect @s speed abc` → `abc`). → Anchor 파서가 위치를 내고 `command::Error::Syntax`로 같게 보냄. 남은 차이(intended): `/xp abc`(Anchor는 PMMP처럼 문자열 하나라 핸들러가 usage), 같은 게임 모드로 `/gamemode`(PMMP 실패 유지), 게임 모드 뒤 UpdateAbilities.
- `/effect @s clear`: BDS는 효과 ID 순으로 지우고 각 MobEffect(제거)에 그 효과의 증폭과 남은 시간을 싣는다 → Anchor 고침(단위 시험; 남은 시간은 실행마다 1~2틱 달라 P6에서는 비교하지 않음).

### 20 그림·폭죽·크리스털·삼지창
- 그림: 블록 서쪽 면에 걸면 Direction 1, 위치는 그림 가운데(벽에서 1/32 앞, 예 `[13.96875, -58, 13]`), BDS는 맞는 가장 큰 무늬 중 하나(이번 `orb`). Anchor와 위치·방향 같음.
- 폭죽(폭발 없음, Flight 1): 16 `{Fireworks:{Explosions:[], Flight:1}}`, 17 Vec3 `[0,1,0]`, 18 int64 −1, 5(owner) −1. 터질 때 폭발이 없어도 ActorEvent 25(데이터 0). → Anchor 고침(Flight는 비행 시간에서 계산, 소유자 −1, 폭발 없어도 25).
- 엔드 크리스털(`summon ender_crystal`): 47 BlockTarget `[0,0,0]`, 플래그 FireImmune. Anchor와 47 같음.
- 삼지창: 맞은 주민에게 ActorEvent 2 데이터 3(투사체), 삼지창은 수평 속도가 반대로(약 −0.01배) 튕기고 `item.trident.hit` 뒤 땅에 떨어짐. Anchor도 튕김(`trident bounced` SAME). Anchor는 던질 때 `item.trident.throw` 소리를 안 보냄(남은 차이, 소리 영역).

### 21 주민 직업 번호
- BDS 데이터(`behavior_packs/vanilla` villager_v2): 0 무직, 1 농부, 2 어부, 3 양치기, 4 화살 제조인, 5 사서, 6 지도 제작자, 7 성직자, 8 갑옷 제조인, 9 무기 제조인, 10 도구 제조인, 11 도축업자, 12 가죽 세공인, 13 석공, 14 니트윗. 소환한 주민의 Variant와 거래 창 제목이 이 표와 맞는다(예 3 → `entity.villager.shepherd`, 7 → `entity.villager.cleric`). Anchor 표(`trade.rs`)와 같다.

### 34 위치 표시줄
- 다른 플레이어가 보이게 되면: PlayerLocation{Type 0, 그 플레이어, 눈 위치}, LocatorBar 추가{Action 1, UpdateFlag 63, 색 = 그 플레이어의 목록 색, IconSize 0.2, `textures/ui/locator_bar_dot_3`, 위치 권한 서버, 보임}, 곧 갱신{Action 3, UpdateFlag 44, 위치 권한 클라이언트, IconSize 1, `locator_bar_dot_0`}. 죽으면 PlayerLocation{Type 1 숨김} + 갱신{UpdateFlag 2, Visible false}, 리스폰하면 갱신{UpdateFlag 2, Visible true}, 나가면 제거{Action 2, UpdateFlag 0}. GroupHandle은 BDS가 고른 UUID. → Anchor 구현(`others` SAME). 멀리 있는(보이지 않는) 플레이어의 표시는 보지 못함(실제 접속 확인 대기).

### 35 후속(공기·피해)
- 익사 피해 ActorEvent hurt 데이터 9, 공격 2, 투사체 3, 번개 24 → Anchor가 BDS 피해 원인 번호(PMMP `CAUSE_*` + 1 체계, 번개 24)를 싣게 고침.
- 리스폰한 플레이어 자신에게: Respawn(State 0 위치 (0,32767,0)) → ActorEvent 18 → SetActorData{0 플래그(Breathing 포함), 53·54}. 리스폰 직후 Breathing은 켜져 있다(Anchor 같음).

### 38 문자열 상한 단위
- 채팅(Text Chat): BDS는 512바이트까지 모두에게 돌려보낸다. ASCII 512 통과·513 안 됨, 한글(3바이트) 170자(510바이트) 통과·171자(513바이트) 안 됨 → 바이트 단위. 넘는 줄은 연결을 끊지 않고 버린다. 65,538바이트(21,846자) 원시 Text도 버리고 연결 유지. → Anchor 채팅 줄 상한을 PMMP 512자·2,048바이트에서 BDS 512바이트로.
- CommandRequest: 줄 507바이트까지 처리, 508바이트부터 BDS가 Disconnect 없이 연결을 끊는다(`/tell @s …`와 모르는 명령 600바이트 모두). 한글 519바이트(179자)도 끊김 → 바이트 단위. 디코더 단위(바이트)는 Anchor와 같다. Anchor는 연결을 끊지 않고 PMMP 상한(512자·2,048바이트)에서 `TooLong`으로 답한다(결정: 공식 클라이언트는 그렇게 긴 줄을 보내지 않으므로 대답이 더 안전; facts-only 시나리오로 기록).

### 물약 색
- 투척 물약 LevelEvent 2002 색(ARGB): 야간 투시 C2FF66, 투명 F6F6F6, 점프 FDFF84, 화염 저항 FF9900, 신속 33EBFF, 느림 8BAFE0, 수중 호흡 98DAC0, 고통 A9656A, 독 87A363, 힘 FFC700, 시듦 736156, 저속 낙하 F3CFB9, 거북 도사 8D82E6(강화 8D85E6 → 저항 9146F0), 치유 F82423·재생 CD5CAB·나약함 484D48·물 385DC6(PMMP와 같음). 43~46(바람·짜기·스밈·벌레)은 PMMP에 없음. → gen이 BDS 색으로(12개), 저속 낙하는 PMMP에 효과가 없어 남은 차이(intended).

### SWITCH-A: 낮 주기·낙하·익사 규칙
- `gamerule dodaylightcycle false`(세션 설정) 뒤 들어오면 StartGame `DayCycleLockTime` = 그때 시각(이번 6), `Time`은 계속 세는 월드 시각(4322), SyncWorldClocks 시각 6·`Paused` false, 몇 초마다 같은 시각으로 다시 보냄. → Anchor `daylight-cycle = false`: 시계를 멈춤 표시(paused)하지 않고 잠금 시각을 StartGame에 넣고 보정을 계속 보냄.
- `falldamage`·`drowningdamage` false: GameRulesChanged에 바뀐 규칙 하나씩, 그 뒤 들어온 플레이어의 StartGame·스폰 GameRulesChanged에 false. Anchor 스폰 GameRulesChanged는 설정 값(같음). BDS는 StartGame GameRules에도 모든 규칙을 넣고 Anchor는 비운다(남은 차이, 스폰 비교 필드 밖).
