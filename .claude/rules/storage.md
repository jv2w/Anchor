---
paths:
  - "src/save.rs"
  - "src/storage.rs"
  - "src/storage/**"
  - "src/tick/save.rs"
  - "src/import.rs"
  - "src/import/**"
  - "src/replay.rs"
  - "src/export.rs"
  - "src/export/**"
  - "src/tick/tests/crash.rs"
  - "src/tick/tests/replay.rs"
  - "tests/crash.rs"
  - "tests/shutdown.rs"
  - "tests/import.rs"
  - "tests/export.rs"
  - "tests/replay.rs"
  - ".claude/docs/storage.md"
---
# Storage rules

## Data folder
- Store with redb. Keep `data/` to these entries: `worlds/<world>.redb` (one per persistent world), `players.redb`, `main.redb` (save turn, operators, bans, whitelist, user server data, deletion marks of deleted worlds), `packs/`, `identity.key`, `auth.json`, `backups/`, `crash/`.
- Never store a setting in a data file: settings always follow `anchor.toml` and the code. Never store the weather: the server starts clear, and a cycle or persistence is built with the API.
- Keep the data of one block or one player in exactly one file.
- Check every world name with `storage::is_world_file_name`: `build()`, `create_world`, the import, the export, and the start's listing of `data/worlds/`.

## Records
- Store blocks only as Bedrock disk-palette NBT `{name, states, version}`, never as runtime IDs.
- Check a value in this order: checksum, format version, length, decompression. The checksum covers the version bytes, so a damaged version is never reported as a newer format.
- Make every decoder check that each palette index is inside its palette and that the value ends exactly where its record ends.

## Changing the format
- Raise `storage::seal::FORMAT_VERSION` by exactly 1 for every change of the storage format, and describe the new layout in that constant's rustdoc. The input log's format number follows the same rule. Both started at 1.
- Keep the number for a change under which every file of the format reads exactly as before: widening a read limit only (such as the entries a list may hold), or reading an optional key no file of the format holds (one Anchor never wrote in that format and the import never carries).
- Before release, read only the current format. The record decoders (`storage::codec`) know the current layout alone, and a value of any other format is corrupt. Never add reading or migration of old formats. The input log (`tick::log`) has its own format number, raised for every change of the log's layout or of a checkpoint codec's layout (`fields!`, `variants!`, a hand-written `Codec`), and also reads only its current format.
- Test every format change: a round trip of the new format, a value of another format refused as damage with its file unchanged, and no panic on broken values.

## Saving and committing
- While the server runs, let only the save thread (`anchor-save`) touch the data files.
- Commit each save as one snapshot taken at one tick boundary, to every file it changes in turn: the world files, then `players`, then `main` last (`save::commit`), one write transaction per file. Never add a path that saves part of the state on its own for an event: a player leaving, a chunk unloading, and a world unloading stay in memory until the next snapshot writes them.
- Write the snapshot's meta value into every file a commit writes.
- Enable redb quick repair on every write transaction of a data file.
- Keep a snapshot's chunks and player data in the tick's memory until the commit's answer arrives.
- Commit no snapshot after a failed commit.
- Make the main file's commit the one point a save is made at. In every players and world file a save writes, keep one redb persistent savepoint in the same write transaction as its data and meta, made before any data table opens, and delete the one before (`Storage::commit_with_savepoint`); the main file keeps none.
- Do what cannot be undone (removing a world file, renaming or pruning a backup) only after the main file's commit of the save that holds it.
- Put the deletion of a world into the main file's commit (a deletion mark, `Key::Deleted`), remove its file only after that commit, and make the next start finish a marked deletion before anything else (`save::finish_deletions`).
- When a world is made again under the name of a world whose deletion no commit holds yet, delete every value of the old world in the new world's first commit of that file (`Batch::clear`).
- Make a clean shutdown (Ctrl+C, `stop`, and the like) commit the last tick boundary's snapshot to every file, finish any backup in progress, then delete each open file's savepoint and commit the normal-stop mark to it: zero duplication, zero loss.
- Make the next start roll every file later than the main file back to the main file's last save (`storage::settle`): remove a file whose savepoint holds no save and no world record, and refuse the start, keeping the file as it is, when the file has no savepoint or its savepoint is later too. Keep the roll back idempotent.

## Damage
- Quarantine a chunk when any of its values fails its checksum or its decode, or its read fails after its retries. Never stop the server for a failed read.
- Refuse the join of a player whose data is damaged, and keep the data as it is.
- Treat a file redb repaired in place (`check_integrity()` returning `Ok(false)`) as damaged.
- Never call redb `check_integrity()` while the server runs.
- Never restore anything automatically, from a backup or otherwise.
- Delete one damaged player record only through `Server::delete_player_data`, which never reads its content.

## Start and stop
- Never write a file in the start check: keep redb's writes on the bounded in-memory backend.
- Never compact at start or at a clean shutdown.

## Backups
- Never overwrite a backup.
- Write each backup as `<name>.tmp`, flush it, then rename it.
- Remove old backups only after the new one is whole.
- Make no start backup of a file refused as damaged or as a newer format.

## Replay
- Open the data files read-only. Commit nothing and write no file.
- Refuse a data folder whose players or world file is later than its main file (`storage::check_settled`).

## Import
- Write no file but the new world file.
- Only read the source: never write its `LOCK`, never replay its logs into tables, never compact it.
- Convert and seal chunks as the save thread does, so the bytes equal those Anchor writes itself. Write all-air chunks too.
- Make the same source give the same bytes whatever the thread count.
- Write all imported data in one write transaction. If any chunk fails, leave no file.
- Write into `<name>.redb.tmp` (which the listing of `data/worlds/` skips), flush it, then rename it to its name. On failure, remove the temporary file.

## Export
- Open the world file read-only and read it in one read transaction. Write nothing in the data folder.
- Write only a new folder. Never touch an existing one: write everything into `<folder>.tmp`, flush it, then rename it. On failure, remove the temporary folder.
- Write block palettes and block entities as their stored NBT. Never upgrade or normalize them.
- Leave out stored values that are not in Bedrock form (entities, spills) and count them in the report.
- Refuse a data folder whose players or world file is later than its main file (`storage::check_settled`).
- Make the same world file give the same bytes whatever the thread count.

Background and details: `.claude/docs/storage.md`.
