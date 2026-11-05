# Storage

What Anchor writes to disk and in what format, when it commits, and what holds after an interruption. Setting keys are in `config.md`; the input log and the replay's structure in `architecture.md`; the redb copy and the defects fixed in it in `dependencies.md`; the import's block upgrade schemas in `data.md`.

Rules: `.claude/rules/storage.md`.

## Data folder

The data folder is `data/` in the run folder; `ServerBuilder::data_dir` moves it.

- `players.redb` keys a player by the XUID in decimal; a player without Xbox sign-in by `offline:` and the ASCII-lowercase name.
- Operators, bans and the whitelist are data, not settings: commands change them.
- `packs/` is the base of relative `resource-packs` paths (`config.md`). `identity.key` and `auth.json` are in `network.md`; the crash input logs in `crash/` in `architecture.md`.
- Backups: `backups/main/`, `backups/players/`, and `backups/worlds/<world>/` for a world.
- A temporary world (`Kind::Temporary`) has no file and is never saved. A chunk no one holds loses its changes when it unloads and is made again by the generator.
- A world's name is its file name. `storage::is_world_file_name` accepts the names that are file names on both Windows and Linux: 1 to 250 bytes; no control character and none of `< > : " / \ | ? *`; no trailing dot or space and no leading space; not a Windows device name (`CON`, `PRN`, `AUX`, `NUL`, `CONIN$`, `CONOUT$`, `COM0` to `COM9`, `LPT0` to `LPT9`, and those with a superscript digit), in any letter case and with any extension. Names that differ only in letter case are one world, because Windows sees one file: a name resolves, ignoring case, to the file on disk (`save::world_place`), and the file and its backup folder keep that file's name. Only when there is no file is one made under the name as given. The tick's name lookups (`load_world`, `delete_world`, `is_world_generated`, and the others) ignore case too.
- The start lists `data/worlds/*.redb` in name order, so the tick can `load_world` a world no one declared. A file whose name breaks the rule, or the second of two names that differ only in case, is left out with a warning. A file added while the server runs is seen from the next start. `data/worlds/` holds at most 1,024 world files (`storage::MAX_WORLDS`). The files a start leaves out (damaged, without a world record, or named as a temporary world) count too: the tick's `create_world` fails with `Full` by the file count at start plus the persistent worlds made this run.
- `Context::delete_world` deletes the one file of an unloaded persistent world, and keeps its backup folder. The save thread forgets it at once, puts its deletion mark into the next commit's main file, and deletes the file after that commit; a start finishes a marked deletion whose file a stop left. A world made again under the name before that commit takes the file, and its first commit deletes every value of the deleted world and the deletion mark, if a commit already holds it (a file that could not be removed), so no start deletes the new world's file.
- Why one file per world: a world file is as large as that world's data, a save does not write the files that did not change, and a world is deleted or moved as one file.
- Why the players have a file of their own: data that grows with the player count stays apart from the small operational data, and deleting that one file resets the player data alone. The rest shares `main.redb` so that it commits in one transaction: fewer fsyncs per save, and fewer files that can disagree.

## File format

Every data file is a redb database. Every file has the same tables (`meta`, `worlds`, `chunks`, `players`, `access`, `user`), which the first commit of a start creates, and fills only those of its part. Keys and values are bytes.

| Table | Key | Value |
|---|---|---|
| `meta` | `meta` | The save turn the file last committed: engine tick u64, then the commit identity (a flag byte and a u64) |
| `meta` | `closed` | The normal-stop mark (empty value) |
| `meta` | `deleted:` and a world name | A deletion mark: the save the main file holds deleted that world (main file only) |
| `worlds` | `world` | The world record: number u32, world time i64, spawn i32 x, y, z |
| `chunks` | Chunk key (below) | The terrain of a sub-chunk, or a page of an extras list |
| `players` | Player key | World name (u16 length, at most 256 bytes of UTF-8), position f64 x, y, z, then `extra`: a little-endian NBT compound of the inventories and the rule modules' other state |
| `access` | Kind byte (0 operator, 1 name ban, 2 IP ban, 3 whitelist), then the ASCII-lowercase name or the IP address as text | Empty for an operator or whitelist entry; for a ban: reason, source, the engine tick it was made at, and the engine tick it ends at, if any |
| `user` | Registered name | The JSON bytes |

A chunk key is a 12-byte prefix (the world number u32, x i32 and z i32, big-endian, with the coordinates' sign bit flipped so keys sort by coordinate) and a kind byte:

- `0x20 | index` (index 0 is y -64 to -49, index 23 the top): the terrain of one sub-chunk. A sub-chunk without blocks whose biomes are all plains has no value, and reads back from none. Only a chunk whose every sub-chunk is so keeps the value of its lowest one, so that the file still holds the chunk and its generator does not refill it.
- 1 block entities, 2 entities: an extras list, cut into pages of at most 16 MiB that also read back within the decoder's budget. Page 0 has the 13-byte key; page 1 on adds the page number as a big-endian u16.
- Entries per list: entities up to the blocks of a column (98,304); block entities up to one per block, plus 1,024 waiting spills (the items of broken containers; once those are full, new stacks join the free slots of the waiting spills, and a break or an explosion that would need more room than those hold is refused before it changes anything) and 98,304 entries kept as read (no place in the chunk, or a second at one place). So a list read back can always be written again, even with a block entity at every block. A block entity list keeping more entries as read is damage. The import writes up to 98,304 entries per list.
- Any other key suffix is damage.

The world number tells the chunks apart inside the file; an imported world's is 0. Blocks are stored as disk-palette NBT, not runtime IDs, so a block Anchor does not know goes back as its NBT. A chunk generated and never changed is not saved: its generator makes it again, so changing the generator changes such chunks.

### Seal

Every value is a 12-byte header and a body, little-endian.

| Bytes | Content |
|---|---|
| 0 to 1 | Format version u16 |
| 2 | Compression: 0 stored, 2 raw DEFLATE |
| 3 | Reserved (0) |
| 4 to 7 | Record length before compression, u32 |
| 8 to 11 | CRC-32C over the key, the first 8 header bytes and the body |

- The checksum covers the key, so a value moved under another key is caught, and the version bytes, so a newer value (a matching checksum, a larger version) is told from a damaged one.
- A record holds at most 16 MiB before compression (`storage::seal::MAX_RECORD`). The largest sub-chunk value takes about 0.92 MB with 4,096 vanilla states and about 9.4 MB with 4,096 custom states of the longest NBT; a whole column would take up to 24 times that, which is why the terrain is stored by sub-chunk. A record decoder may allocate four times the record (64 MiB).
- Records under 64 bytes are stored as they are. Compression is libdeflate level 6: levels 9 and 12 are 1.2% and 4.1% smaller on a whole imported world, and both slower.
- redb's reads do not verify its page checksums, so every value carries its own.

### Format version

The current format is `storage::seal::FORMAT_VERSION`; the rules raise it by 1 for every change (`.claude/rules/storage.md`). A value of an older format is damage: its chunk is quarantined, its player refused, and a `meta` value of one refuses the start. Before release no data of an older format exists that would need moving.

The checksum is checked before the version, so a flipped version byte is damage, not a newer format. A start refuses a file whose `meta` value, with a matching checksum, is of a newer format (`NewerFormat`), so an older Anchor never misreads a newer file and overwrites it.

## Save cycle and commit

1. Snapshot: at a tick boundary, the tick takes what changed since the last snapshot (the changed sub-chunks, which it shares with the live world, the extras lists, the world records, the player data, the changed `access` and `user` entries, the meta value) and queues it for the save thread.
2. A snapshot is taken: every `save-interval-minutes`; on `Context::save` and `/save all`; on `Context::backup`, `backup` and `/check`; on an operator, ban or whitelist change; when more than 4,096 unloaded chunks or 4,096 departed players wait for a commit; when the input log asks for one (`architecture.md`); and at a clean shutdown. `/save-off` stops only the periodic one: the others bound memory or were asked for. While a snapshot is still being committed, the next waits for a later tick boundary; the final snapshot of a shutdown queues behind it.
3. Commit (`save::commit`): the world files, then the players file, then the main file. A world file or the players file with nothing changed is not written; the main file is written by every commit, as it carries the save turn. A world record is written only when its time or spawn changed. The terrain and extras of the chunks are sealed and compressed on the CPU pool.
4. The commit's answer is an input of a later tick. Until it arrives the snapshot's data stays in memory, so a chunk unloaded and loaded again comes from memory, and a read from the disk always sees the newest commit.

Leaving, chunk unloading and world unloading wait for the next snapshot so that no file holds another moment than the others.

A failed commit drops redb's database, opens it again on the same file handle, which keeps redb's lock in between, and tries again, up to 3 times (`storage::RETRIES`). If it still fails, the tick stops, every player is disconnected, the input log since the last commit is written to `data/crash/`, and `Server::run` returns `Error::Storage`. No later snapshot is committed: it would write a state that lacks the failed changes. A chunk batch whose read still fails after the same retries quarantines that batch's chunks of the file instead (an error is logged once per batch): a failed read loses no change, so the server goes on, and the next load of each chunk reads it again.

The data-file errors `Error::DataDamaged`, `Error::DataNewerFormat` and `Error::Storage` carry the file in their `path` field. The place that knows the file adds it (`storage::Error::in_file`); a failure of no single file (the listing of `data/worlds/`, the save thread stopping) carries the data folder or the world folder.

A world made or loaded while the server runs, whose file does not open, is not saved and its chunks are quarantined; the save thread tells the tick, which unloads the world, and the next `load_world` opens its file again.

Reads: the tick sends bounded requests for chunks and players. The save thread reads a chunk batch in one read transaction per world file, then decodes, generates and lights the chunks on the CPU pool, and reads their extras into the rule modules' typed form (`ChunkRules`) there too, one pool item per list that has entries. So the tick only puts what was read into its stores; a chunk still held in memory comes back in the view phase, at most `MAX_RETURNS_PER_TICK` a tick, lit and read on the CPU pool by the same functions, and chunks the tick lets go of are freed by the save thread (`Request::Discard`). The chunks go through the pool one turn (a chunk per pool thread) at a time, and each turn's answers go back in request order as soon as it ends.

`Server::delete_player_data(key)` deletes one record of `players.redb` without reading it, with the file checked as a start checks it, and refuses while a server runs; the key is the XUID in decimal or `offline:` and the name, as the join-refusal log gives it.

Players who are not online (`Context::hold_offline_player`): the load is the same player request a join sends; once it is answered, a ticket holds the record in memory to read and change. The newest data of such a player is its held record, else its data waiting for a commit since it left, else the file's. The save thread answers in request order and data waiting for a commit stays in memory until the tick learns of the commit, so a load that finds neither in memory when its answer arrives has read the newest data. A changed record goes to wait for a commit as a departed player's data does, when a snapshot is taken, its last ticket is released, or the player joins: the next snapshot commits it with the rest, and a join takes it as it takes a departed player's data, so it is neither lost nor duplicated. Only the inventories and data values that changed are written anew; the rest of the record stays as it was read. A damaged record is not held and stays as it is. No record is held for a player online or joining.

A quarantined chunk shows empty, never changes and is never saved; its stored values stay as they are.

## Clean shutdown and crash

A clean shutdown starts on Ctrl+C, the console `stop`, `ShutdownHandle`, SIGTERM and SIGHUP on Unix, and Ctrl+Break, closing the console window, and system shutdown on Windows. The signals are listened for from before the first tick.

- SIGHUP is not listened for when it is ignored as the server starts (`nohup`), so the server keeps running when the terminal closes; a handler would replace that disposition. If `/proc/self/status` cannot tell whether it is ignored, Anchor warns and listens for it, since the default disposition would end the process without the final save.
- The Windows logoff event is not listened for: only a service receives it, at any user's logoff, and without a listener the OS's default handling keeps a service running.
- After the console close and shutdown events, Windows ends the process about 5 seconds later, whether the stop has finished or not.
- A stop writes only the changed files and does not compact. It waits for a backup in progress (a start backup included), which takes seconds on a large world (the start backup row under "Start cost").
- A second stop signal ends the process at once with status 130, with the guarantees of a crash.
- `tests/shutdown.rs` sends SIGINT, SIGTERM and SIGHUP on Unix and checks the clean stop, and checks that a server started with SIGHUP ignored keeps running through a SIGHUP. A test cannot raise the Windows logoff event.

A crash (a killed process, a power cut, a panic of the tick or the save thread):

- A commit to one file is a redb transaction: it lands whole or not at all. The next open has redb recover the file.
- The changes since the last commit are lost.
- A crash between the files of one save leaves the files written before the main file at the new save. The next start, before it opens them, rolls each such file back to the savepoint its last commit kept (`storage::settle`), or removes it when that save made it, and logs it as a warning: every file then holds the main file's last save. A file that cannot be rolled back, whatever the cause (no savepoint, a savepoint that is later too, a removal or write that fails), refuses the start with its bytes as they were. Without a main file nothing is rolled back. An earlier file is normal: one that did not change, so a save did not write it.
- The data of one block or one player is in one file, so it is never two within that file.
- A new data folder (`data/`, `data/worlds/`, a backup folder, made by `fs::create_dir_all_durable`), a new data file, a world file's deletion, a backup's rename, and an atomic write (`fs::write_atomic`: the settings file, `auth.json`, `identity.key`) flush their folder afterwards (`fs::sync_parent`): on Unix, flushing a file does not make its name durable. Windows cannot flush a folder (opening one to flush it is refused), so nothing is flushed there; NTFS journals entry changes, so a crash leaves the old entry or the new one.
- An atomic write removes a `<name>.tmp` a crash left and writes a new file: the old one keeps the mode it was made with, and whoever opened it then could read the new bytes. So `identity.key` is always 0600 on Unix.
- When the tick thread panics, its request queue closes without `Finish` and the save thread commits none of the requests left. When the save thread panics, nothing more is committed. Either way the files get no normal-stop mark.

Why one savepoint per file: the pages it holds are freed when the next save deletes it, so a file grows by at most one save's changes; a normal stop deletes it.

Limits:

- What user callbacks did to outside systems (files, network) is not undone by a crash.
- redb defects not fixed upstream (`dependencies.md`): a crafted file with matching checksums can cause a large allocation, and page damage made by hardware while the server runs makes a read panic, which stops the server without saving (the next start's check refuses the file).
- With `integrity-check = "never"`, a start runs no redb integrity check; damage shows only when a value is read.
- On Windows, a file creation or rename just before a power cut can be lost.

## Start and integrity check

A start does this for each file (`Storage::open`):

1. Checks the closed file without writing to it (`storage::verify`). It opens redb on a backend (`check::Overlay`) that reads the file and keeps every write and length change in memory, runs redb's crash recovery there, redb's `check_integrity()` as `integrity-check` says, and the format check of the last commit's `meta` value, then drops it all. `check_integrity()` runs always with `always`, only when the file lacks the normal-stop mark with `after-unclean-shutdown`, and never with `never`; the format check runs at every start. A clean stop's last commit writes the mark and a start's first commit removes it. The mark is read with a read-only open that writes nothing, before redb's lock; a file that cannot be opened so (one left for redb to recover among them) counts as unmarked and is checked. The mark counts only if the file's length and first page (redb's header) are the same once the check holds the lock, so no commit or recovery came in between. A missing file has nothing to check; an empty one opens as a new database.
2. Refuses the start for a damaged file (one redb repaired, `Ok(false)`, included) or one of a newer format. The check wrote nothing, so the file keeps its bytes, and no backup is made.
3. Opens the file on the handle the check opened (`storage::Handle`); redb recovers a file a crash left here again. redb's lock, taken by the check's open, stays with that handle until the file closes: every later redb open on it (the roll back, this open, a reopen after a failed commit) takes the ranges the handle holds, and no redb close releases them. So the file opened is the file checked, and another process is refused (`InUse`) from the check on. A missing file is made here only if no other process made one since the check (`InUse` otherwise).
4. Its first commit creates the missing tables and removes the normal-stop mark.
5. With `backups = true`, for a file that existed before, begins the start backup of that first commit (see "Backups").

Between opening the main file and the others, every players and world file later than the main file is rolled back (see "Clean shutdown and crash").

Why the check must not write: redb's open writes the header, a repair commits, and the close commits the allocator state. The backend keeps those writes in memory, so the original bytes stay without a copy. The writes are small, the header and the allocator state: 24 KiB for a 96 KiB file and 52 KiB for a 368 MiB world, the same for a file a crash left. The backend keeps at most 16 MiB of writes, past which that file's start fails with an error; the allocator state grows with the file, so files hundreds of times larger than that world fit.

What keeps the check cheap (the same check over the same pages): the backend reads the file 256 KiB at a time and keeps the 16 most recent blocks (4 MiB), and redb's page cache is off. redb's check reads page by page, mostly in file order: on a 368 MiB world this made 3,241 read calls instead of 185,770 and took 350 to 481 ms instead of 631 to 878 ms. Blocks of 64 KiB to 1 MiB, or fewer blocks, read more and were slower. A 64 MiB redb page cache was slower than none (545 to 630 ms against 333 to 517 ms): its evictions cost more than it spared. So checking a file takes 4 MiB and its writes, whatever the file's size.

A server start (`Server::run`) runs step 1 for the main file, the players file and the declared persistent worlds' files together on the CPU pool (`save::check_all`), and keeps each file's handle. If any is refused, it opens no file, refuses the start, and logs an error for each refused file. It then opens the files one by one on their handles (steps 3 to 5); the roll back between them works on the same handles, and a file it or a finished deletion removes is made again by a check and open of its own. So a start takes about the check of the largest file plus the opens.

Why a start needs no compaction: later commits reuse the pages copy-on-write freed, and only data that shrinks leaves free pages at the end of the file; deleting a world deletes its file.

The files a start opens are the main file, the players file and the declared persistent worlds'. Another world file goes through the same steps, without redb's integrity check (never made while the server runs), when it is first opened. A start only reads the record of an undeclared world file; if a crash left that file for redb to repair, the start opens it through the same steps without a start backup, and it gets its backup when it is loaded. If a declared persistent world's file does not open at the start (the open writes, so it can fail after the check passed: a read-only file, a backup folder that cannot be written), the start is refused, as for the main and players files. If a world file does not open while the server runs, or a replay's world has no copy or its copy does not open, every chunk of that world is quarantined and nothing of it is saved.

### Start cost

A data folder of 381 MB (main 56 KiB, players 84 KiB, one world 368 MiB), `integrity-check = "always"`, a release build, the measurement `storage::tests::measure_data_folder_start` (its rustdoc tells how to run it). Warm means the files are in the OS cache. Cold means, on Windows, after an open with `FILE_FLAG_NO_BUFFERING` evicted them, and on WSL after `drop_caches` (not fully cold: Windows may cache the disk image). Other programs were building and writing to disk during the runs, so each value is the range of several runs.

| Stage | Windows warm | Windows cold | WSL ext4 warm | WSL ext4 cold |
|---|---|---|---|---|
| World check | 0.17 to 0.20 s | 0.36 to 0.80 s | 0.18 to 0.20 s | 0.58 to 1.95 s |
| Opening every file, `backups = true` | 127 to 146 ms | 32 to 34 ms | 35 to 38 ms | 143 to 268 ms |
| Opening every file, `backups = false` | 48 to 265 ms | 45 to 51 ms | 11 to 12 ms | 48 to 875 ms |
| Data files in all, `backups = true` | 0.30 to 0.35 s | 0.39 to 0.41 s | 0.21 to 0.22 s | 0.76 to 1.13 s |
| Data files in all, `backups = false` | 0.24 to 0.74 s | 0.65 to 0.84 s | 0.19 to 0.21 s | 0.68 to 2.75 s |
| Start backup of the world (while the server runs); its longest step | 2.2 to 10.0 s; 0.11 to 0.86 s | 2.2 to 4.5 s; 0.11 to 0.98 s | 1.4 to 1.6 s; 62 to 78 ms | 4.6 to 5.8 s; 0.53 to 0.77 s |
| Floor: reading every file in order | 56 to 64 ms | 251 to 462 ms | 60 to 68 ms | 222 to 566 ms |

- The rest of a server start (settings, threads, packs, opening the network) takes about 60 ms. A server start ("server starting" to "server started") took 0.29 s on Windows, warm, with `backups = true`.
- The time left is redb's check itself: it takes memory and computes a checksum for every page. redb's public API offers nothing cheaper, so a warm check stays at about three times the floor.
- Opening with `backups = true` takes longer for the syncs of redb's initialization of each new start backup file.
- During a start backup, a request to the save thread (a chunk read) can wait for one step (up to 8 MiB copied). With the disk busy, a step took over a second.

### Check while running

`/check` (operators, permission `anchor.command.check`, in game and on the console): once the next snapshot is committed, the save thread reads every value of that commit from one read transaction per open file, 8 MiB per step, and checks its seal (header and CRC-32C) without decompressing or decoding it. Between steps it serves the requests that wait, so the tick does not wait for it. One check runs at a time. The result (the values that passed, the damaged count, the first damaged value's file and table) goes to whoever asked, to the log and to the input log. redb's `check_integrity()` needs a closed file, which is why a running server never calls it.

## Backups

- Place and name: `data/backups/<file>/<unix seconds>[-n].redb`, a world's in `data/backups/worlds/<world>/`. A second backup in the same second takes `-n`, and after the clock went back a name keeps the newest second found, so name order is time order.
- Each file keeps its `max-backups` newest backups (start and online backups counted together), so the disk holds up to that many backup copies of each file besides the one being made. Old backups are removed only once the new one is whole.
- Every backup is written as `.tmp`, flushed and renamed, so a cut copy never counts as a backup and pushes out a good one. A start removes, after opening the file (under redb's lock), the `.tmp` files a cut run left. If it cannot, it logs an error and opens the file all the same; the next backup skips the name left.
- Start backup: with `backups = true`, a start copies the first commit of each file it opened that existed before, as an online backup does. A file refused as damaged or of a newer format is never opened, so it has no start backup. The server takes players without waiting for it. A start backup that cannot begin is logged as an error and the file opens without it. A backup asked for while a start backup runs is not made. A clean stop finishes the start backup in progress; a crash leaves only its `.tmp`, which is never taken for a backup and which the next start removes before it makes its own start backup, and the earlier backups remain. It holds the same data as the closed file (the same save turn), in bytes redb wrote anew, which differ from the original's.
- Online backup (`Context::backup`, the `backup` command for operators): once the next snapshot is committed, the save thread copies every open file from one read transaction of that commit into `.tmp`, 8 MiB per step, serving other requests between steps. The step commits are not flushed; a whole copy ends with one durable commit, a close, a flush and the rename. On failure it removes the `.tmp`, logs an error, and the server runs on. A backup asked for while one is being made is not made.
- With `backups = false` no start backup is made and `Context::backup` returns `false`.
- To restore, the operator stops the server and copies the files of one backup turn into place together.

## Replay and data files

`Server::replay(log)` runs a `data/crash/` input log again (its structure is in `architecture.md`). It starts from the oldest segment whose save the main file or one of its backups (newest first) still holds, matched by the tick and commit identity of the copy's last commit, preferring a segment after the log's last truncation. Each other file is its newest copy (the file, then its backups) whose last commit is that save or an earlier one, since a save writes only the files that changed. Chunks load from those copies and are checked against the logged checksums, which catches a copy that does not match. A data file later than the main file is refused; one start of the server rolls it back.

## World import

`Server::import_world(name, folder)` imports a Bedrock (LevelDB) world folder into a new file `data/worlds/<name>.redb`, before `run`.

- A server program run as `<program> import <world folder> [name]` does this import from `Server::run` without starting the server (the default world, `level-name`, without a name). It uses the worlds, generators and custom content the program registered. It writes nothing but the new world file, so a server may be running; that server knows the world from its next start.
- Import into a running server only under a name that server does not use: if it makes a world of the same name with `create_world` meanwhile, both use one file.
- A running server's tick does not count a file imported while it runs. So when `data/worlds/` is full at 1,024 files, a persistent world `create_world` accepted gets no file from the save thread: its chunks are quarantined and nothing of it is saved. This is a known limit.
- The LevelDB reader is Anchor's own read-only reader. The import fails if a source file changes meanwhile (the world is open in another program).
- The same reader is public as `import::Db` (`open`, `get`, `scan_prefix`), so a program can read the keys the import leaves out (players, entities, scoreboards, and the like). It reads the files as they were at open without checking for changes, and does not decode values (NBT and others). It blocks, so call it before `run` or inside `spawn_blocking`.
- Only the overworld's blocks and block entities (their whole NBT: container items, sign text, and the like) and `level.dat`'s time and spawn are imported. Old block states and items are upgraded with the upgrade schemas (`data.md`); an unknown block stays as an unknown block with its NBT. A chunk's biomes are those its world's generator gives. Biomes, entities, pending block updates, the Nether, the End, and players are not imported but counted in `import::Report`.
- No setting of `level.dat` is imported (seed, game mode, difficulty, game rules). The spawn goes into the world record and is also reported (`Report::spawn`).
- An all-air chunk the import did not write would be filled by the generator.
- A failed import's error names the source file, and the chunk and key when a chunk's value is at fault. A file of the same name (in any letter case) refuses it with `WorldExists`, changing nothing, as does a file another process opened after the import made it (a server that made a world of the name meanwhile): it is that process's and stays. A name declared temporary, in any letter case, is refused (`TemporaryWorld`). A declared world's file takes the declared name's letter case; an undeclared name takes the default world's generator, and the tick loads it with `load_world`.

## World export

`Server::export_world(name, folder)` writes a persistent world's file `data/worlds/<name>.redb` (the name in any letter case) as a new Bedrock (LevelDB) world folder: the import's reverse.

- A server program run as `<program> export <new folder> [name]` does this export from `Server::run` without starting the server (the default world, `level-name`, without a name).
- The world file is opened read-only and read in one read transaction (`storage::Snapshot`), so the folder holds the last save, and nothing is written in the data folder. The main file is opened read-only too, to refuse a world file later than it; one server start rolls such a file back. So a running server, which holds the main file, refuses every export by redb's lock (`Error::Storage`). A file a crash left needs redb's repair, so it does not open read-only; one server start repairs it.
- The folder must be new: an existing folder or `<folder>.tmp` refuses the export with `FolderExists`, changing nothing. Everything goes into `<folder>.tmp`, each file flushed, then the rename. A failure removes the temporary folder, and a crash leaves only `<folder>.tmp`, so a half-written world never appears under the name.
- Every chunk the file holds is written: blocks (the stored palette NBT as it is, unknown blocks as they came), biomes, and block entities (stored in Bedrock form). A chunk with saved extras but no saved terrain gets its world generator's terrain, as a load does: a declared world's generator and seed, else the default world's generator and the seed of the name. Entities and spills (the items of broken containers waiting to drop) are stored in Anchor's own form, so they are not written but counted in `export::Report`. A failed export removes its own folder; a parent folder it made stays. Players are not in the world file.
- Chunk values take the layout the official server writes: chunk version 42, sub-chunk version 9 (only for sub-chunks with blocks), `Data3D` (the height map left at zero, then each sub-chunk's biomes, 0xff for a storage the same as the one below), the block entities, and finalization state 2.
- The LevelDB is written in one compacted form: sequence number 0 for every key (so any value the game writes later is newer), 128 KiB data blocks (raw DEFLATE, id 4, as the official server writes them), 2 MiB tables, and every table in the last level, so opening it compacts nothing. Blocks are made and compressed on the CPU pool.
- `level.dat` holds the name, spawn, time, storage and network versions, and a flat generator. Chunks the file does not hold (generated and never changed) are not written; that flat generator makes them: for a `Flat` world the official server's default flat layers (the same terrain), for any other generator one layer of air (nothing). Game mode, difficulty, game rules and seed are not written: a world runs with the settings of the server that opens it.
- The same world file gives the same files whatever the thread count, and a folder exported and imported again gives the same world file (`tests/export.rs`).
- `cargo xtask p6 import` compares the export with the official server (`testing.md`).

## What is not saved

Every start's settings come only from `anchor.toml` and the code (`ServerBuilder`, and `world::Options` for a world made while running). So a data file and the settings never hold different values with one winning, and a changed setting takes effect at the next start.

- A setting changed through the API while running (server or world difficulty, game rules, `motd`, `max-players`, the whitelist switch, and the like) is back to the settings' value after a restart. The per-world time stop (`Context::stop_time`), which has no setting key, starts off.
- With `level-seed` empty, a world's seed is the first 8 bytes of the SHA-256 of its name: the same at every start and different between worlds.
- A world file keeps only state: the number, time, spawn and chunks. The time goes on, and the day cycle continues from it.
- The world spawn is state, not a setting: a spawn moved with `World::set_spawn` stays after a restart. The settings' `spawn` and `world::Options::with_spawn` place only a new world's first spawn, one without a record.
- With `save-player-data = false`, no player data is saved.
