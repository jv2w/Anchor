# M8 P6 사실 (BDS 1.26.52.3, 2026-10-04)

C5 서브에이전트가 BDS에서 확인한 사실이다. 메인이 PLAN.md 10.8에 옮긴다. 출처는 `cargo xtask p6 api bds`(RakNet, zlib, 새 평지 월드, `allow-cheats=true`, 폼 스크립트 팩)의 캡처(`target/p6/bds-api/`, 시나리오마다 `<시나리오>.txt` 보고)와 `p6 bds`의 스폰 캡처다. 같은 실행을 세 번 돌려 값이 같은 것만 적었다.

## 0. 시험 도구
- 봇 시나리오(`xtask/go/bot/api.go`, 게임 시나리오 틀 위): `commands`, `particles`, `kick`, `forms`, `script`. 플레이어 명령은 공식 클라이언트처럼 CommandRequest(origin `player`, 새 UUID, RequestID "", PlayerUniqueID 0, Internal false, Version `"latest"`)로 보낸다. BDS는 이 형태를 받는다.
- 봇은 명령마다 `output` 줄(CommandOutput 전체, 원점 UUID가 요청과 같은지, 받은 패킷 이름, 바뀐 상태: 돌 개수, 플레이어의 첫 이동, 레벨·경험치, time 명령이면 첫 월드 시각)과 `available` 줄(명령마다 이름, 별칭, 플래그, 권한 단계, 오버로드 매개변수 형)을 쓴다.
- 콘솔 명령(`op`, `kick`, `scriptevent p6:<종류>`, `summon snowball|egg`)은 하네스가 BDS 콘솔에, Anchor에서는 공개 API(`Context::op`, `Player::kick`, `Context::send_form`, `Context::spawn_projectile`)로 넘긴다(`xtask/src/p6/game/console.rs`).
- 비교(`xtask/src/p6/api.rs`): 봇 단계를 순서대로 짝짓고, 단계마다 `fields.txt`에 있는 패킷의 개수와 고정 필드, `output`의 `uuid_echoed`·`state`, 폼 JSON(숫자는 수로), 확인 줄을 비교한다. 준비 단계(`setup`)는 UpdateAbilities만 비교한다. 명령 목록은 둘 다 가진 명령만 메모로 비교한다. `particles`, `script`는 사실 기록만 한다.
- `script` 시나리오: `-script 파일`의 동작(`chat`, `command`, `expect-output`, `expect-text`, `expect-title`, `expect-toast`, `expect-form`, `answer-form`, `expect-score`, `expect-bossbar`, `expect-window`, `click`, `expect-disconnect`, `wait`)을 차례로 돈다. 예제 시험용이다. `xtask/data/p6/script.txt`로 BDS와 Anchor 둘 다에서 통과를 확인했다.

## 1. 스폰 창
- SetCommandsEnabled: BDS는 `allow-cheats=false`면 Enabled false, true면 true. Anchor는 언제나 true(PMMP와 같음, 치트 개념 없음). 고정 필드에 넣지 않았다.
- AvailableCommands(구성원): BDS는 권한과 상관없이 82개 명령을 모두 보낸다(PermissionLevel 0: 4개, 1: 60개, 2: 7개, 3: 1개, 4: 10개). 클라이언트가 자기 권한 단계로 숨긴다. Anchor는 권한 있는 명령만 보낸다(PMMP). 순서(SetDifficulty 뒤 SetCommandsEnabled, CraftingData 뒤 AvailableCommands)는 BDS와 같고, 스폰 창 비교가 zlib·Snappy·NetherNet 모두 SAME이다.
- UpdateAbilities(구성원): CommandPermissions 0, PlayerPermissions 1. BDS와 Anchor가 같다(고정 필드에 넣음).
- CameraPresets(프리셋 6개: first_person, fixed_boom, follow_orbit, free, third_person, third_person_front), CameraAimAssistPresets(범주 bucket·empty_hand·default, 프리셋 하나), CameraSpline(빈 목록): BDS만 보낸다. PMMP·dragonfly는 보내지 않고 클라이언트는 없이 들어온다. → 유예표에서 `-`(PMMP has no camera API).

## 2. op
- 콘솔 `op <이름>` 뒤 BDS는 AvailableCommands를 다시 보내지 않는다(목록이 이미 전부라서). 보내는 것: Text(종류 10, rawtext `commands.op.message`), UpdateAbilities(CommandPermissions 1, PlayerPermissions 2, 층 하나 Abilities 1048575, Values 255, FlySpeed 0.05, WalkSpeed 0.1, VerticalFlySpeed 1).
- Anchor는 `Context::op` 뒤 AvailableCommands를 다시 보낸다(권한별 목록이라 필요). 비교에서 메모로만 둔다.

## 3. 명령 출력 (CommandOutput)
- 공통: 원점은 요청 그대로(Origin 0 player, 같은 UUID, RequestID "", PlayerUniqueID 0), OutputType 3(AllOutput), DataSet 없음. 성공이면 SuccessCount 1, 메시지 Success true. 실패면 0과 false. 메시지는 번역 키와 매개변수다.
- 명령마다 BDS 값(메시지 키, 매개변수):
  - 권한 없음(구성원의 `time set 500`): `commands.generic.error.permissions` `["time"]`. 목록에 있는 명령이라 "모르는 명령"이 아니다.
  - `say hi`: CommandOutput을 보내지 않는다. 모두에게 Text(종류 8 Announcement, SourceName `P6Commands`, Message `[P6Commands] hi`, 번역 없음).
  - `give @s stone 3`: `commands.give.success` `["Stone","3","P6Commands"]`. 받는 이에게 Text(종류 10, rawtext `commands.give.successRecipient` `["Stone","3"]`), InventoryTransaction(서버 → 클라이언트), InventoryContent 4창, PlayerHotBar, 소리 `pop`.
  - `tp ~ ~1 ~`: `commands.tp.success.coordinates` `["P6Commands","-3.50","-59.00","-11.50"]`(발 좌표, 소수 둘째 자리). MovePlayer(Mode 2 Teleport, TeleportCause 3, 눈 높이 +1), Text(종류 10, `commands.tp.successVictim` `["-3.50, -59.00, -11.50"]`). 떠 있는 봇에게 곧 CorrectPlayerMovePrediction을 보낸다.
  - `gamemode creative`: `commands.gamemode.success.self` `["%createWorldScreen.gameMode.creative"]`. UpdatePlayerGameType(GameType 1, PlayerUniqueID 자신, Tick 서버 틱)만 보내고 SetPlayerGameType·UpdateAbilities는 보내지 않는다. Text(종류 10, `gameMode.changed` `["%createWorldScreen.gameMode.creative"]`). PMMP는 SetPlayerGameType, UpdateAbilities, UpdateAdventureSettings를 보낸다.
  - `time set 1000`: `commands.time.set` `["1000"]`. SetTime이 아니라 SyncWorldClocks(SyncStates 하나, Time 1000, Paused false)로 알린다. BDS는 시각이 흐를 때도 몇 초마다 SyncWorldClocks를 보낸다.
  - `effect @s speed 10 1`: `commands.effect.success` `["%potion.moveSpeed","1","P6Commands","10"]`. MobEffect(Operation 1 Add, EffectType 1, Amplifier 1, Duration 200, Particles true, Ambient false), UpdateAttributes(이동 속도), SetActorData.
  - `xp 10`: `commands.xp.success` `["10","P6Commands"]`. UpdateAttributes(레벨 1, 진행 0.33).
  - `give @s no_such_item`: `commands.generic.syntax` `["/give @s ","no_such_item",""]`(앞부분, 틀린 낱말, 뒷부분).
  - `no_such_command`: `commands.generic.unknown` `["no_such_command"]`.
- BDS는 명령마다 Event(SlashCommandExecuted: 명령 이름, 성공 수, 메시지 수·키)를 보낸다(원격 측정). PMMP·dragonfly는 보내지 않는다. 비교하지 않는다.
- 같은 명령을 공식 클라이언트가 보내는 Version 값은 관찰할 수 없다. 검증 필요(M9): 공식 클라이언트.

## 4. 명령 목록 모양 (AvailableCommands, BDS)
- 매개변수 형 번호: int 1, float 3, target 8, string 56, position 65, message 68(MessageRoot: say, me, tell, kick), raw_text 70, json 74. PMMP `ARG_TYPE_MESSAGE`도 68이다. gophertunnel의 `CommandArgTypeMessage`는 67이다.
- 공통 명령의 BDS 모양(권한 단계, 플래그, 오버로드): give 1 0 `target enum:Item int? int? json?` / effect 1 0 `target enum:ClearEffects enum:Effect?`, `target enum:Effect int? int? enum:Boolean?`, `target enum:Effect enum:AddInfiniteEffect int? enum:Boolean?` / gamemode 1 0 `enum:GameMode target?`, `int target?` / time 1 0 `enum:TimeModeAdd int`, `enum:TimeModeSet int`, `enum:TimeModeSet enum:TimeSpec`, `enum:TimeModeQuery enum:TimeQuery` / xp 1 0 `int target?`, `suffix:L target?`(접미사 매개변수는 Valid 비트 없이 접미사 번호) / say 1 192 `message` / kick 1 128 `target message` / op 2 1152 `target` / particle 1 0 `string position?` / tell 0 192 별칭 `tell w msg` `target message` / tp는 `teleport`의 별칭.
- 별칭이 없는 명령의 AliasesOffset은 0xFFFFFFFF(PMMP·Anchor와 같음). 별칭 enum은 이름을 맨 앞에 넣는다(`teleport`: `["teleport","tp"]`, `allowlist`: `["allowlist","whitelist"]`). Anchor는 이름을 끝에 넣는다(PMMP), 클라이언트에 차이가 있는지는 검증 필요(M9).

## 5. 채팅
- 클라이언트 Text(Chat, SourceName 이름, 메시지)에 BDS는 모두에게 Text(종류 1 Chat, SourceName `이름§r`, Message 원문, Parameters 없음, NeedsTranslation false)를 보낸다. PMMP는 Text(종류 2 Translation, `chat.type.text`, [이름, 메시지], NeedsTranslation true)다. Anchor는 E1부터 BDS 모양이다(9절).

## 6. 입자 (10.7 K 검증 필요)
- `/particle <이름> ~ ~1 ~`: BDS는 SpawnParticleEffect(Dimension 0, EntityUniqueID -1, Position, ParticleName 그대로, MoLangVariables 없음)와 `commands.spawnParticleEmitter.success` [이름]을 보낸다. 번호를 쓰는 옛 입자(LevelEvent)는 이 명령으로 나오지 않는다.
- 눈덩이가 땅에 맞으면: LevelEvent 2009(ParticleGenericSpawn) ×6, EventData 15, 위치 = 맞은 블록 윗면. 알: LevelEvent 2009 ×6, EventData = (알 아이템 런타임 ID 393 << 16) | 14. 그 뒤 RemoveActor와 LevelSoundEvent.
- → 입자 번호 14 = 아이템 깨짐(PMMP `ITEM_BREAK`), 15 = 눈덩이(PMMP `SNOWBALL_POOF`). PMMP 번호가 맞고, gophertunnel은 여기서 하나 작다(IconCrack 13, SnowballPoof 14: 9번 CandleFlame이 목록 끝 81에 있음). 19 이상(PMMP 18 BREEZE_WIND_EXPLOSION 끼움)은 BDS가 번호 입자를 내는 경우를 찾지 못했다: 검증 필요(M9).
- BDS의 옛 입자 형식은 LevelEvent 2009 + 데이터 `id | (추가값 << 16)`이다. PMMP는 LevelEvent `0x4000 | id` + 데이터 = 추가값이다. 공식 클라이언트가 둘 다 받는지는 관찰할 수 없다: 검증 필요(M9).

## 7. kick
- 콘솔 `kick <이름> <이유>`: Disconnect(Reason 55 Kicked, HideDisconnectionScreen false, Message = 이유 그대로 `P6 kick reason`, FilteredMessage ""). Anchor `Player::kick`과 같다(A1의 "PMMP는 0: 검증 필요" 닫음).

## 8. 폼 (BDS 스크립트 팩 `@minecraft/server-ui` 2.0.0, 실험 토글 없음)
- 팩: `xtask/data/p6/forms-pack`(매니페스트 + `scripts/main.js`). `/scriptevent p6:<종류>`로 모두에게 폼을 보낸다. `development_behavior_packs/p6_forms`와 `worlds/p6/world_behavior_packs.json`으로 켠다. ActionFormData와 ModalFormData 둘 다 `header`, `label`, `divider`가 있다.
- 모달: `{"type":"modal","title","content","button1","button2"}`. 답 true/false, 닫으면 CancelReason.
- 메뉴: `{"type":"form","title","content","elements":[...]}`. 원소: 버튼 `{"type":"button","text","image":null}` 또는 `"image":{"type":"path","data":"textures/items/apple"}`, `{"type":"header","text"}`, `{"type":"label","text"}`, `{"type":"divider","text":""}`. `buttons` 키는 없다(C2의 "메뉴 `elements` 형식" 검증 필요 닫음). 답은 버튼만 센 번호.
- 사용자 정의: `{"type":"custom_form","title","icon":null,"content":[...]}`. 원소: header·label·divider는 메뉴와 같은 모양, `{"type":"input","text","placeholder","default"}`, `{"type":"toggle","text","default"}`, `{"type":"slider","text","min","max","step","default","timeout":100}`, `{"type":"dropdown","text","options","default"}`. 숫자는 `10.0`처럼 실수로 쓴다(default 정수는 `3`). 답은 원소마다 하나(header·label·divider는 null).

## 9. Anchor와 다른 점 (E1 뒤, 2026-10-04)
E1이 BDS에 맞춘 뒤 `p6 api bds`와 `p6 api anchor`를 다시 돌렸다: kick·forms·particles·script SAME, commands는 아래 "남은 차이"만 DIFFERENT다.

이제 BDS와 같은 것:
- 모르는 명령: `commands.generic.unknown` [이름]. 권한 없음: `commands.generic.error.permissions` [명령 이름](PMMP는 영어 문장). 채팅으로 친 `/` 줄도 같은 번역 키로 받는다.
- 채팅 메아리: Text(종류 1 Chat, SourceName `이름§r`, 원문, 매개변수 없음, 번역 없음). PlayerChat의 format이 기본값(`None`)일 때 이 모양이고, 리스너가 format을 주면 PMMP처럼 그 글을 Raw로 보낸다. 받는 이·말은 이벤트를 따른다. 표시 이름이 채팅 출처 상한(256바이트, `player.rs`)을 넘으면 PMMP의 `chat.type.text` 번역으로 보낸다(줄을 잃지 않게).
- message 매개변수 형 68(MessageRoot). gophertunnel의 67(`Message`)이 아니다.
- op·deop 뒤 UpdateAbilities: op면 CommandPermissions 1, PlayerPermissions 2, Values에 OperatorCommands·Teleport(생존 255), deop면 0·1·63. 같은 상태로 다시 op하면 보내지 않는다. op인 플레이어가 들어올 때 스폰 창부터 op 값을 보내려면 `spawn::join`에 한 줄이 필요하다(메인 몫, 보고 참고). 스폰 창 StartGame의 PlayerPermissions는 아직 언제나 1(구성원).
- gamemode: 바뀐 플레이어에게만 UpdatePlayerGameType(GameType, PlayerUniqueID 자신). SetPlayerGameType은 보내지 않는다(하드코어 관전 전환도 같음). gophertunnel 문서대로 남의 ID로 보내면 클라이언트가 무시하므로 다른 플레이어에게는 보내지 않는다. 게임 모드 이름은 `%createWorldScreen.gameMode.<모드>`(BDS 언어 파일에 넷 다 있음, PMMP는 `%gameMode.<모드>`).
- 명령이 손댄 플레이어에게 가는 BDS 메시지: 객체 Text(종류 10, rawtext JSON + 줄바꿈)로 gamemode `gameMode.changed`(자기 자신에게도), give `commands.give.successRecipient` [아이템, 개수], tp `commands.tp.successVictim` ["x, y, z"]. tp 좌표는 소수 둘째 자리(`-3.50`, PMMP는 `-3.5`). 플레이어에게 보내는 tp(`tp <누구> <누구에게>`)의 successVictim 매개변수(도착 플레이어 이름)는 BDS에서 보지 않았다: 검증 필요(M9).
- 폼: 그림 없는 메뉴 버튼 `"image":null`, 사용자 정의 폼 `"icon":null`, 슬라이더 `"timeout":100`. 키 순서는 serde_json이 이름순으로 쓴다(JSON 객체라 클라이언트는 순서를 보지 않는다).

남은 차이(일부러, 이유):
- `/say`: BDS는 CommandOutput 없이 모두에게 Text 8(SourceName 이름, `[이름] 말`). Anchor는 PMMP `SayCommand`처럼 `§d%chat.type.announcement` [이름, 말] 번역과 빈 CommandOutput이다(PMMP 동작으로 두기로 한 결정).
- give 성공 메시지의 아이템: BDS는 언어 파일 이름(`Stone`), Anchor는 ID(`minecraft:stone`). 아이템 표시 이름 표가 없다(PMMP는 `Item::getName()` 영어 이름). 아이템 데이터 쪽에 이름 표가 생기면 맞춘다.
- gamemode 뒤 UpdateAbilities: BDS는 보내지 않고(클라이언트가 게임 모드로 능력을 정하는 듯) Anchor는 PMMP·dragonfly처럼 보낸다. API가 바꾼 비행·충돌·비행 속도·op 값을 실어야 하고, 같은 값이라 클라이언트에 해가 없다.
- 문법 오류: BDS `commands.generic.syntax` [앞, 틀린 낱말, 뒤](`["/give @s ","no_such_item",""]`), Anchor는 PMMP `Usage: /give ...`. 파서(`parse::fit`)가 틀린 낱말 위치를 내야 하고, 인자가 모자랄 때 BDS 모양은 보지 않았다: 검증 필요(M9), 다음 작업 후보.
- UpdatePlayerGameType Tick: BDS는 서버 틱(예 333), Anchor는 0이다(비교 필드 아님). `Player::set_game_mode`에서 틱이 닿지 않는다.
- op 뒤 AvailableCommands 재전송(PMMP: 권한별 목록), 명령 목록 모양(오버로드·권한 단계·플래그)은 메모로만 비교한다.
- `/particle`: Anchor는 PMMP 모양(번호 입자, `<name> <position> <xd> <yd> <zd> [count] [data]`)이라 BDS 오버로드 `string position?`와 다르다(사실 기록 시나리오). 입자 번호는 다른 담당.
