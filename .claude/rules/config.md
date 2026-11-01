---
paths:
  - "src/config.rs"
  - "src/config/**"
  - "tests/config.rs"
  - ".claude/docs/config.md"
---
# Configuration rules

## The file
- Keep one configuration file, `anchor.toml`, with the tables `[general]` (settings for anyone running a server), `[developer]` (performance and protocol tuning), and per-world `[worlds.<name>]`.
- Add only the keys that are needed. Name a key after the existing server setting it matches, flattening that setting's nested path into one key name.
- The canonical sources are the code that writes the default file (`DEFAULT_FILE` in `src/config.rs`) and `.claude/docs/config.md`. Change both in the same change: keep the default file as the doc's first `toml` block, byte-identical to `DEFAULT_FILE` (`tests/config.rs` compares them), and update the doc's key tables with it.
- Put every key in `DEFAULT_FILE`, with its comment lines directly above its line: `Config::update_file` copies exactly those lines into older files.

## New keys and behavior without a key
- Give a new key the default that matches Anchor's behavior without it, so an old file that lacks the key gives the same server.
- Give behavior that has no key the value that is fastest, safest, and right for every server.

## Validation
- Never fix or ignore a wrong value silently. Reject the start with a `config::Error` that names the key, and let the user fix the value.
- Never change an existing settings file at start. Only `Config::update_file` (which `<program> update-config` runs) changes it, and only by adding missing keys and removing unknown keys; it changes no value.

## Feature switches
- Make a feature that can be turned off run no code at all when off: zero tick work, events, packets, and memory. Never make the off state by only hiding the feature.
- Test the off state: the feature creates no state, output, or events, and its tables stay empty.

Background and detailed facts are in `.claude/docs/config.md`.
