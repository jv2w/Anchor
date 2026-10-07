# Anchor

Rust로 만든 Minecraft: Bedrock Edition 서버 라이브러리. 미니게임, PvP, 로비, 커스텀 콘텐츠 서버를 자기 바이너리로 만든다. 플러그인 폴더는 없고, Anchor를 의존성으로 두고 코드로 서버를 짠다.

## 시작하기

```toml
# Cargo.toml
[dependencies]
anchor = { git = "https://github.com/jv2w/Anchor", rev = "<commit>" }
```

`rev`로 커밋을 고정한다. 고친 의존성 사본(`vendor/`)은 경로 의존성이라 함께 따라온다.

```rust
fn main() -> anchor::Result<()> {
    anchor::Server::builder()
        .config(anchor::Config::default().load("anchor.toml")?)
        .build()?
        .run()
}
```

- 바이너리 프로필은 `panic = "unwind"`(기본값)를 유지해야 한다.
- 실행 폴더에는 `anchor.toml`과 `data/`만 생긴다. `anchor.toml`이 없으면 기본 파일을 만든다. 키는 [.claude/docs/config.md](.claude/docs/config.md), 데이터 폴더는 [.claude/docs/storage.md](.claude/docs/storage.md).

## 실행 인자 명령

Anchor로 만든 서버 프로그램은 코드 없이 두 명령을 받는다. 서버를 켜지 않고 일만 하고 끝나므로, 서버가 켜져 있어도 서버 폴더에서 그대로 돌린다.

```
<서버>.exe update-config                        # anchor.toml에 새 키를 더하고 모르는 키를 지움
<서버>.exe import <Bedrock 월드 폴더> [이름]     # LevelDB 월드 → data/world/<이름>.redb (이름 없으면 level-name)
```

그 밖의 인자는 프로그램의 것이고 서버가 평소대로 켜진다.

## 문서

- API: `cargo doc --open -p anchor`. `Player::getName` 같은 다른 서버의 API 이름으로 검색해도 찾는다.
- 설계: [.claude/docs/](.claude/docs/) — architecture, network, protocol, storage, data, dependencies, testing, config. 같은 주제의 작업 규칙은 [.claude/rules/](.claude/rules/)에 있다.
- 예제: [examples/](examples/) (lobby, pvp, minigame, custom).

## AI 도구로 개발할 때

서버 프로젝트에서 `cargo vendor`를 돌려 Anchor와 의존성 소스를 프로젝트 안에 두면, AI 도구가 Anchor의 코드와 rustdoc을 바로 읽을 수 있다.
