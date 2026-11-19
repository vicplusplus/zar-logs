# Contributing and developer notes

How this project is developed, and what that means for contributions, is in the developer's own words at the top
of the [README](README.md): it is AI-programmed, directed and playtested by a person who does not review the Rust
by hand. Bug reports with what the game showed are the most valuable contribution. Code contributions are welcome,
but expect them to be assessed through that same loop (try it, play with it, check the numbers).

Two rules are not negotiable, for any change:

- **Read-only.** Nothing may write to the game's process, files or save, inject into it, hook it or send it input.
  The process handle is opened with `PROCESS_VM_READ | PROCESS_QUERY_LIMITED_INFORMATION` and nothing else.
- **No game assets, and nothing about the game hard-coded.** No picture, text or data file of the game in the
  repository or in a build. No offset, enum number or game value in the source: everything is read from the
  installed game and stored per game version (see "Where the game's values come from").

## Build

Rust (stable) on Windows:

```
cargo build --release -p zar-logs --features custom-protocol      # target/release/zar-logs.exe
cargo test --workspace                                            # unit tests that need no game
```

With the game installed, `ZAR_GAME_DIR=<game folder> cargo test --workspace -- --ignored` also runs the tests that
read it.

The installer is made with the Tauri command-line tool (`cargo install tauri-cli --version "^2" --locked`, once):

```
cargo tauri build          # target/release/bundle/nsis/zar-logs_<version>_x64-setup.exe, and the exe as above
```

It is Tauri's stock NSIS installer, set up in `src-tauri/tauri.conf.json`: per-user (no administrator rights),
Start Menu shortcut, optional desktop shortcut, an entry in Windows' list of apps. It installs the program to
`%LOCALAPPDATA%\zar-logs`. The app's data is kept apart from the program, in `%LOCALAPPDATA%\com.vicplusplus.zar-logs`
(`zar_logs_core::paths::data_dir`), which is the folder the uninstaller's "Delete the application data" checkbox
removes. The installer never downloads anything (`webviewInstallMode: skip`).

A release has two files: `zar-logs-setup.exe` (the installer) and `zar-logs-portable.zip` (the exe plus
`uninstall.cmd`, `uninstall.ps1`, `README.md`, `LICENSE` and `THIRD-PARTY-NOTICES.md`).
`.github/workflows/release.yml` builds both when a `v*` tag is pushed and leaves a draft release.

`--headless` runs the whole app without windows (the pages are then reachable in a browser at the address it
prints).

## What is in the repository

```
crates/core       process reader, layout, decoder, world data, names, encounter model, log writer
crates/layoutgen  memory layout from GameAssembly.dll + global-metadata.dat
crates/unity      the game's asset bundles: UnityFS, type trees, Addressables catalog, sprites -> pictures
crates/namespack  the names pack and the class skills table, from the definitions and the localisation text
crates/gearcalc   what a mod, a weapon and an armour piece are worth: the numbers on the game's own cards
crates/buildview  a captured loadout -> the build the page shows
crates/cli        diagnostics: probe the running game, show where things were found
src-tauri         the app: local HTTP server, log store, capture thread, viewer and overlay windows
web               the pages (plain JS + SVG), compiled into the app
```

## Where the game's values come from

Nothing about the game is written into this program: no offset, no enum number, no weapon, mod, armour, enemy or
skill value, no display text. All of it is read from the installed game and stored per game version:

```
<cache>/game/<version id>/manifest.json   which game version this is, and what made the files
                         /layout.json     memory layout: field offsets, struct sizes, enum values, runtime facts
                         /tables.json     definitions, localisation text, enum values, code factors, skills table
                         /names.json      the names pack
                         /icons/          pictures
<cache>/current.json                      the version used last (so logs can be viewed without the game)
```

The version id is a hash of the game's own markers: the Steam build number, the content catalog's hash, and the
size and time of `GameAssembly.dll`, `global-metadata.dat` and the catalog. When the installed game has no complete
folder (first start, or after any game update) the tables are made again from the game, about ten seconds, and the
status line says so. The folders of the last three versions are kept. `zar-logs.exe --prepare-only` does just this
step, without opening a window, and prints what it did.

What is read, and from where:

| What | Source |
|---|---|
| Field offsets, struct sizes and strides, enum members and values, string / array / list layout | IL2CPP metadata and `GameAssembly.dll` (`crates/layoutgen`) |
| Weapons, mods, enhancements, armour, abilities, skills, classes, levels, story missions | the definitions in the asset bundles (`crates/unity`) |
| Whether a level is the hub or a mission | the loaded level's own type, read from the running game (a story mission has no mode, like the hub) |
| The numbers in upgrade and enhancement descriptions | the definitions, combined the way the game's own description routines combine them |
| Enemy health, shields and resistances, difficulty multipliers | the running mission's own data, per mission |
| Every label the page shows for a game thing (quality, damage type, slot, stat, difficulty) | the game's localisation text, by enum member name |
| Four factors of the mod maths that exist only as machine code | read from the code of that game version, with the evidence stored next to the values |
| Pictures | the UI texture bundle |

The code factors are the one thing whose extraction can fail after an update, because it recognises a code shape.
Then the previous version's values are carried over, marked unverified, and the app says so in its status line
instead of silently using them.

What stays in the source, because the game's files do not state it:

- **Names, not numbers.** The program asks for things by the game's own names: struct and field names
  (`GameEntity.health`), enum member names (`EntityType.Enemy`), localisation key patterns
  (`Equipment_Quality_<member>`), definition class and field names. A rename in an update is reported (a missing
  name is an error with that name in it; `tables.json` carries a `notes` list of names that no longer exist) and
  never silently turns into a wrong number.
- **The formulas.** How mod points, quality and level combine into a card's number is this program's reading of
  the game's code; its inputs are all from the tables.
- **Two facts about native memory**: the member order of the C++ vector the simulation uses (its size is confirmed
  from the metadata, the order is the compiler's) and that the resistance tables hold 32-bit floats.
- **The meter's own choices**: read caps against garbage, how often the entity list is walked, rounding of what is
  stored, which skills count as damage-relevant in the skills table.
- **How to find the game**: the process name and the Steam app id.

All of it stays on your machine, in the cache folder. None of it is part of this repository or of a build.

## What is recorded, and how often

- **Events** (every hit, heal, kill) are read on every frame the game simulates, so damage is exact.
- **State** (health, status effects, ability slots, bosses) is also sampled on every frame. An effect's start comes
  from the game's own apply time; its end is the last frame it was seen on.
- The full walk of the entity list (finding players and bosses) runs once a second, because it is one read per
  entity. The world's tables (names, defences, loadouts) are read once per mission, on a separate thread.

## What counts as a player's damage

The game attributes every hit to a client. That attribution is how it credits kills, and it is wider than "damage
this player dealt", so the encounter model (`crates/core/src/session.rs`) narrows it:

- **Counted**: hits by a weapon or an ability (turrets and other deployables are abilities), and by things the
  player set off that are not enemies (an explosive barrel).
- **Not counted**: hits whose source definition is an enemy. A Detonator's blast among other zombies and a
  Multiplier clawing a turret both arrive credited to a player; they are counted in `stats.enemy_hits` and nowhere
  else. A source the mission's tables do not have yet is held for up to two seconds of game time while the tables
  are read again, then decided.
- **Counted apart**: hits on anything that is not an enemy (crates, pods, barrels, level geometry, a turret caught
  in the player's own blast). Every figure is kept twice, without and with these; a log stores the first in place
  and the second under `with_structures`, and `session::view` turns a log, a list row or a live payload into
  whichever the user asked for. So the switch works on missions already recorded.

The game's own damage counter (shown in the capture note under a log) counts weapon damage with structures and
nothing from abilities or deployables. It is a check on the decoder, not the figure shown.

## Logs

One file per mission in the logs folder, `enc-<id>_<map>_<mode>_<difficulty>.json.gz`: gzip-compressed JSON with
`summary` (the list row), `detail` (everything the page shows) and `hits` (one row per counted hit). The id is the
local start time; the rest of the name is for people. `index.json` is a cache of the summaries and is rebuilt from
the files whenever they disagree. `detail.format` is the log format (`session::FORMAT`); a log of an older format
is shown as recorded and is not converted.

An imported log (`store::import`) is another person's file and is handled as untrusted: it must be a whole log
with a usable id and a format this version knows, every rendered build in it is cut back to the raw loadout and
rendered again from the local game data, and the page only ever loads pictures from the app's own cache.

## Diagnostics and tests

```
cargo test --workspace                                              unit tests; no game needed
ZAR_GAME_DIR=<game folder> cargo test --workspace -- --ignored      also the tests that read the installed game
cargo run -p zar-logs-cli -- paths                                  where the game and the cache were found
cargo run -p zar-logs-cli -- probe --seconds 10                     decode the running game for 10 s, print a summary
cargo run -p zar-logs-layoutgen -- --game <game folder> --out <layout.json>    the memory layout of that install
zar-logs.exe --prepare-only                                         read the game's data into the cache, no window
```

There is no recorded-session replay and no reference implementation to compare against: a change to the decoder
or the encounter model is checked by the unit tests, by `probe` against the running game, and by playing a mission
and comparing with what the game shows. `probe` attaches read-only, exactly as the app does.

A build is rendered when the mission is recorded and saved with the log. A log that only holds the raw loadout
(recorded before the game's data had been read) is rendered when it is opened.

## Known gaps

- Windows only. Not signed, no automatic updates.
- The export and import dialogs and "Show file" are thin wrappers over Windows' own dialogs and are not covered
  by a test; what they call (`store::import`, copying a log) is.
- Who gave a status effect is read (`giver_entity_id`) but only used to rule a teammate out. An effect the game
  applies to everyone (the invulnerability at the end of a mission) is still attributed to a teammate's ability
  when a teammate has an ability that applies the same effect set.
- Descriptions: a number that is a mod-point parameter of an enhancement's bonus (Last Gasp) shows as `?`.
- BC7-compressed textures are not decoded (no picture the app uses needs it today).
- A weapon whose explosion is defined only on an entity it spawns gets no explosion row (none exists today).
- Not yet confirmed in game: the shared pistol bonus applying to a pistol's explosion; a rocket's contact hit
  using the shot's damage unchanged; the meaning of some effect-resistance values.
- Not yet exercised: a real game update; the fallback that carries code-derived factors over from the previous
  game version; the CI and release workflows.
