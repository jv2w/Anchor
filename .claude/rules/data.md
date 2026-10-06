---
paths:
  - "src/block.rs"
  - "src/block/**"
  - "src/item.rs"
  - "src/item/**"
  - "src/crafting.rs"
  - "src/crafting/**"
  - "src/effect.rs"
  - "src/effect/**"
  - "src/world/biome.rs"
  - "src/world/biome/**"
  - "src/world/chunk.rs"
  - "src/world/sub_chunk.rs"
  - "src/world/connect.rs"
  - "src/import/upgrade.rs"
  - "src/import/upgrade/**"
  - "src/tick/update.rs"
  - "src/tick/interact.rs"
  - "src/tick/interact/inert.rs"
  - "src/tick/interact/breaking.rs"
  - "src/tick/interact/place.rs"
  - "src/tick/interact/using.rs"
  - "src/tick/block_entity.rs"
  - "src/tick/block_entity/**"
  - "xtask/src/gen.rs"
  - "xtask/src/gen/**"
  - "xtask/src/dump.rs"
  - "xtask/src/dump/**"
  - "xtask/php/**"
  - "xtask/go/reference/**"
  - "xtask/data/pmmp/*.json"
  - "xtask/data/pmmp/liquid.txt"
  - "xtask/data/reference/**"
  - "xtask/data/upgrade/**"
---
# 블록·아이템 데이터 규칙

## 출처
- 블록, 아이템, 레시피, 크리에이티브 목록, 효과, 인챈트 표는 손으로 쓰지 않는다. 고정한 외부 출처를 실행하거나 변환한 결과를 `xtask/data/`에 커밋하고, `cargo xtask gen`으로 `src/` 아래 생성 표를 만든다.
- 블록·아이템의 종류, 상태, 직렬화 규칙은 PMMP에서 가져온다. PMMP에 없는 블록과 아이템은 표에 채우지 않는다.
- 버전별 번호(블록 네트워크 ID, 아이템 런타임 ID)는 프로토콜 기준 목록에서 가져온다. 기준 목록은 검증과 번호에만 쓰고, 블록을 만드는 데 쓰지 않는다.
- PMMP 소스를 파싱하지 않는다. PMMP 레지스트리를 실행해 그 결과를 쓴다.
- Mojang 프로토콜 문서는 숫자만 옮기고 스키마 파일은 복사하지 않는다.

## 덤프와 PHP 스크립트
- `dump`는 기준 목록(`xtask/go/reference`)을 PMMP 실행보다 먼저 만든다.
- PHP 스크립트(`xtask/php/`)는 일반 PHP가 아니라 pmmp/PHP-Binaries의 PM5 빌드로 돌리고, 작업 폴더는 `.cache/pmmp`로 한다.
- PHP 스크립트 출력은 정렬하고 한 줄에 한 항목으로 써서, 같은 체크아웃이 같은 바이트를 내게 한다.

## PMMP 결함 보정
- 모르는 인챈트를 버려 생기는 빈 인챈트 책은 크리에이티브 목록에서 뺀다.
- 불길한 배너는 BDS 값으로 바꾼다. 바꾸는 곳은 덤프의 모든 아이템이 거치는 한 곳(`xtask/src/gen/data.rs`)이다.
- 이름에 PMMP 블록 아이템과 바닐라 상태가 하나씩이면 블록 아이템은 바닐라 크리에이티브 상태(`item_blocks.json`)로 보낸다. 아이템이 놓는 상태는 그대로 PMMP 상태로 둔다.
- 크리에이티브 아이템 네트워크 ID는 BDS처럼 1부터 센다.

## 기준 목록
- dragonfly와 CloudburstMC의 블록 상태(이름, 속성 이름·타입·값, 버전)와 아이템(이름, 런타임 ID, 버전, 컴포넌트 여부)은 완전히 같아야 한다.
- 블록 네트워크 ID는 표의 기준 목록 값을 쓴다. 해시 함수는 커스텀 블록에 쓴다.
- 바이옴은 생성 데이터가 없는 판만 쓴다.
- 같은 Bedrock 상태로 가는 PMMP 상태의 차이(침대 색, 배너 색 등)는 블록 엔티티에 둔다. 이런 겹침은 `gen/data.rs`의 `SHARED`에 든 것만 허용한다.
- BDS light dampening은 비활성 블록에만 쓴다.
- BDS `isSolid`는 쓰지 않는다. 단단함은 충돌 상자로 정한다.
- BDS가 StartGame에 보내는 data-driven 블록 정의는 보내지 않는다.

## 생성 표
- 생성 표는 모두 `static`으로 둔다.
- 1.26.50이 더한 속성(`minecraft:connection_*`, `minecraft:corner`)은 서버에서 이웃으로 계산하고, 상태 표는 그 모든 값을 담는다.
- gen은 다음 중 하나라도 있으면 실패하게 한다.
  - 업그레이드 뒤 기준 목록에 없는 상태, 쓰이지 않는 업그레이드 항목
  - 같은 Bedrock 상태로 가는 PMMP 상태끼리 값 충돌(`SHARED` 밖), 필요 없는 `SHARED` 항목
  - 기준 목록에 없는 아이템 이름, 풀리지 않는 레시피·크리에이티브 아이템
  - PMMP가 읽는 기준 상태에 Anchor 상태가 없음
  - 블록 상태 버전이 `blocks.json`과 마지막 스키마에서 다름

## 알 수 없는 블록과 비활성 블록
- 저장된 블록 상태를 Anchor가 모르면 저장 원본(NBT)을 그대로 보존한다.
- 클라이언트가 보낸 알 수 없는 블록이나 아이템은 그 요청만 거부한다.
- 비활성 블록(`block::Inert`)은 클라이언트에 그 상태의 해시 ID, 즉 BDS가 보내는 값 그대로 보낸다. 그 밖의 모르는 상태는 `minecraft:info_update`로 보낸다.
- 비활성 판정은 청크를 읽을 때 한 번 하고 팔레트 항목 `Entry::Inert`에 담는다. 틱마다 다시 하지 않는다.
- 이웃 규칙과 액체는 비활성 블록을 기반암으로 읽는다.
- 비활성 블록의 부수기 시간은 굳기 × 1.5 ÷ 들고 있는 도구 효율이다. 들고 있는 도구를 맞는 도구로 보아 서버 허용 시간이 클라이언트보다 길지 않게 한다.
- 비활성 블록을 부수면 공기로 바꾸고 블록 엔티티를 지운다. 드롭은 없다.
- 놓기 규칙은 비활성 블록을 대역으로 읽는다. 칸 전체 충돌 상자 하나면 기반암(모든 면 완전 지지), 아니면 거미줄(지지와 충돌 없음)이고, 둘 다 대체되지 않는다.
- 해초·켈프 칸은 대체하지 않는다.
- 이벤트는 Anchor 상태만 담는다. `BlockBreak`와 오른쪽 클릭 `PlayerInteract`는 비활성 블록 대신 `block::INFO_UPDATE`를 담는다. 취소하면 블록을 남기고 원래 블록으로 다시 보낸다.
- 비활성 블록에는 왼쪽 클릭 `PlayerInteract`와 파티클 이벤트를 쏘지 않는다.
- 블록 엔티티 안의 블록과 무늬도 원본을 보존한다. 모르는 화분 식물(`PlantBlock`)은 원본 그대로 두고, 비활성 블록이면 클라이언트에 그대로 보낸다. 모르는 배너 무늬 ID도 그대로 두고 보낸다.
- 공개 API는 아는 블록과 무늬만 보인다.
- 블록 엔티티 안의 모르는 아이템은 PMMP처럼 뺀다. 뺀 것은 info 로그로 남긴다(슬롯 번호가 틀린 아이템은 warn).

## 업그레이드 스키마
- 블록 상태 버전은 생성 상수 `BLOCK_STATE_VERSION` 하나만 쓴다.
- 저장소 루트의 `0351`은 덤프가 따로 읽어 다른 블록 스키마 옆에 둔다.
- 상태 버전 묶음의 첫 스키마는 이미 적용된 것으로 보고 건너뛴다. gen은 기준 상태 22,091개가 모두 그대로 돌아오는지 확인한다.
- gen은 한 스키마에서 같은 ID가 이름 바꾸기와 평탄화에 함께 있는 것, 순서에 따라 결과가 달라지는 속성 이름 바꾸기, 127보다 큰 byte 값을 거부한다.
- 모르는 레거시 meta는 원본 `{id, val}`을 알 수 없는 블록으로 보존한다.
- 상태 버전이 가장 새 스키마보다 높으면 가장 새 버전의 상태로 보고 그 버전의 스키마를 적용한다.
- gen은 아이템 스키마의 소문자 ID, 숫자 meta, 1:1 레거시 ID를 검사한다. CRLF는 LF로 바꾼다.
- 규칙 해석기(`src/import/upgrade/apply.rs`)는 gen과 공유한다.

## `.cache/` 다시 만들기
- 빌드, 시험, `verify`는 커밋된 데이터만으로 돌게 한다(dimpl 사본 시험의 Perl만 예외).
- 저장소 루트에서 이 순서로 다시 만든다. `<원본>`, `<커밋>`, sha256은 문서의 `.cache/` 표 값이다.
  1. git 체크아웃 여섯 개: 폴더마다 `git init .cache/<폴더>`, `git -C .cache/<폴더> remote add origin <원본>`, `git -C .cache/<폴더> fetch --depth 1 origin <커밋>`, `git -C .cache/<폴더> checkout --detach FETCH_HEAD`.
  2. PM5 PHP: 릴리스 zip을 받아 sha256을 확인하고 `.cache/php-pm5/`에 푼다. `bin/php/php.exe`와 `bin/php/php.ini`가 그 자리에 있어야 한다.
  3. PMMP 의존성: `.cache/pmmp`에서 PM5 PHP로 composer를 돌려 `composer install --no-dev --ignore-platform-reqs`를 실행한다. PMMP `composer.lock` 그대로 `.cache/pmmp/vendor`에 둔다.
  4. BDS: 받은 zip을 `.cache/bds/1.26.52.3/`에 푼다.
  5. Strawberry Perl: portable zip을 받아 sha256을 확인하고 `.cache/tools/strawberry-perl-5.42.3.1/`에 푼다.
  6. str0m 캡처: `cargo xtask vendor-test str0m`으로 받는다.
  7. `cargo xtask dump --check`가 통과하는지 확인한다.

배경과 자세한 사실은 `.claude/docs/data.md`.
