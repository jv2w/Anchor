# Anchor

Anchor is a Rust library for building Minecraft: Bedrock Edition servers, intended for minigame, PvP, lobby and custom-content servers. A server is an ordinary Rust binary that depends on Anchor; there is no plugin folder.

## Usage

```toml
[dependencies]
anchor = { git = "https://github.com/jv2w/Anchor", rev = "<commit>" }
```

```rust
fn main() -> anchor::Result<()> {
    anchor::Server::builder()
        .config(anchor::Config::default().load("anchor.toml")?)
        .build()?
        .run()
}
```

- The binary must keep `panic = "unwind"`, the default.
- The server creates only `anchor.toml` and `data/` in its working directory.
- The binary also accepts `update-config`, `import <world folder> [name]` and `export <new folder> [name]`, which run without starting the server.
- Windows and Linux (x86_64, aarch64) are supported. Open port 19132 (`server-port`) for both UDP and TCP; it is the only port the server uses; on hosts without IPv6, set `enable-ipv6 = false` under `[general]`.

## Documentation

API reference: `cargo doc --open -p anchor`. Design notes: [.claude/docs/](.claude/docs/). Examples: [examples/](examples/).

## License

Licensed under either [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option. Third-party code and data are listed in [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).

NOT AN OFFICIAL MINECRAFT PRODUCT. NOT APPROVED BY OR ASSOCIATED WITH MOJANG OR MICROSOFT.
