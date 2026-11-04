# Anchor

Anchor is a Rust library for building Minecraft: Bedrock Edition servers, intended for minigame, PvP, lobby and custom-content servers. A server is an ordinary Rust binary that depends on Anchor; there is no plugin folder.

Anchor leaves out what vanilla does on its own: blocks act like stone apart from placing, breaking and a few kept clicks (chests, doors, signs), and there is no crafting, redstone, mob AI or terrain generation. Players keep every survival feature. The full list of what is kept and removed is in [.claude/docs/architecture.md](.claude/docs/architecture.md).

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
- Windows and Linux (x86_64, aarch64) are supported. Open port 19132 (`server-port`) for both UDP and TCP; it is the only port the server uses; on hosts without IPv6, set `enable-ipv6 = false` under `[general]`. Behind a router, forwarding that port is enough; `public-ip` is not needed.
- If a player on the same network as a Windows host times out joining by the host's LAN address (`InitialConnection-149`), they join through the public address or domain instead (the router must loop it back, as most do): the client may also try TCP 443 on the typed address, and Windows Firewall leaves that try unanswered (`.claude/docs/network.md`).

## Documentation

API reference: `cargo doc --open -p anchor`. Design notes: [.claude/docs/](.claude/docs/). Examples: [examples/](examples/).

## License

Licensed under either [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option. Third-party code and data are listed in [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).

NOT AN OFFICIAL MINECRAFT PRODUCT. NOT APPROVED BY OR ASSOCIATED WITH MOJANG OR MICROSOFT.
