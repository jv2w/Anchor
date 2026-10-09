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
<서버> update-config                        # anchor.toml에 새 키를 더하고 모르는 키를 지움
<서버> import <Bedrock 월드 폴더> [이름]     # LevelDB 월드 → data/worlds/<이름>.redb (이름 없으면 level-name)
```

그 밖의 인자는 프로그램의 것이고 서버가 평소대로 켜진다.

## Linux에서 돌리기

Windows와 Linux(x86_64, aarch64)를 지원한다. 실행 폴더와 설정은 같다.

- 실행: 서버 폴더에서 `./<서버>`. systemd 서비스나 표준 입력이 없는 곳(`/dev/null`, 파이프)에서도 돈다. 그때는 콘솔 명령만 받지 않는다.
- 종료: SIGINT(Ctrl+C), SIGTERM(`systemctl stop`, `docker stop`), SIGHUP은 정상 종료다. 마지막 틱을 모든 파일에 저장하고 끝낸다. 두 번째 신호는 저장 없이 끝낸다(종료 코드 130). SIGKILL은 크래시와 같아서 마지막 저장 뒤의 변경을 잃고, 다음 시작이 파일 사이의 어긋남을 알린다.
- 권한: 실행 사용자가 서버 폴더에 쓸 수 있어야 한다. `data/identity.key`는 소유자만 읽는 0600으로 만든다.
- 방화벽: UDP `server-port`(기본 19132)와 `nethernet-port`(기본 19133), TCP `server-port`를 연다. 예: `ufw allow 19132/udp`, `ufw allow 19133/udp`, `ufw allow 19132/tcp`.
- 커널에서 IPv6를 끈 호스트(`ipv6.disable=1`)는 `[general]`에 `enable-ipv6 = false`를 둔다. 아니면 IPv6 소켓을 열지 못해 시작이 `could not open the network ports`로 실패한다.

## 문서

- API: `cargo doc --open -p anchor`. `Player::getName` 같은 다른 서버의 API 이름으로 검색해도 찾는다.
- 설계: [.claude/docs/](.claude/docs/) — architecture, network, protocol, storage, data, dependencies, testing, config. 같은 주제의 작업 규칙은 [.claude/rules/](.claude/rules/)에 있다.
- 예제: [examples/](examples/) (lobby, pvp, minigame, custom).

## AI 도구로 개발할 때

서버 프로젝트에서 `cargo vendor`를 돌려 Anchor와 의존성 소스를 프로젝트 안에 두면, AI 도구가 Anchor의 코드와 rustdoc을 바로 읽을 수 있다.
