# Methods: how this tool learned what it knows about the game

This is the record of what was done to Zombie Assault: Resurgence, its files and its running process in order to
make zar-logs: what was decompiled, what was extracted, which tools were used, what was run against the live game,
and what was deliberately not done. It was compiled from the development session records by the AI that did the
work, at the developer's request. The developer's own statement on AI use and on who did what is at the top of the [README](README.md).

Nobody at Ninja Kiwi was contacted, and no permission was asked or given. Ninja Kiwi's
[Terms of Service](https://ninjakiwi.com/terms) prohibit capturing data from their games, and hacking them, without
their written consent; what is described below was done without that consent, and so is using the tool.

## 1. What the program does when you run it

**To the game's process: it reads, from outside.**

- Finds the process and its modules: `CreateToolhelp32Snapshot`, `Process32FirstW/NextW`, `Module32FirstW/NextW`,
  `QueryFullProcessImageNameW`, `GetExitCodeProcess`.
- Opens it with `OpenProcess(PROCESS_VM_READ | PROCESS_QUERY_LIMITED_INFORMATION)`. No other access right is
  requested, so the handle cannot write memory, start threads or suspend the process.
- Reads with `ReadProcessMemory`. All of this is in one file, `crates/core/src/proc.rs`.

**What it reads there.** Starting from the game's `GameAssembly.dll` in memory: the `GameRunner` class's static
`Instance`; from it the per-frame update result (hits, heals, kills, ability and reload events, in a fixed-size
event buffer), the world controller, and the mission result. Once per mission: the definitions baked into that
mission (weapons, abilities, enemies, status effects), the native "world load data" block with every enemy's
health, shield and resistance tables, and each player's loadout. On every frame: the state of entities, status
effects and ability slots.

**From the game's files (read only):** `GameAssembly.dll` and `global-metadata.dat` (type metadata and code, for
the layout of the data in memory), the Addressables `catalog.bin`, the asset bundles (definitions, English
localisation text, pictures), and Steam's `appmanifest` and `libraryfolders.vdf` (to find the game and its build
number).

**What it writes:** only its own files, in its data folder (`%LOCALAPPDATA%\com.vicplusplus.zar-logs`): logs of missions and a
cache of what it read from the game. Nothing is written in the game's or Steam's folders.

**Network:** none. A small web server runs on `127.0.0.1` so the app's own windows can show their pages. The
program makes no outgoing connection, has no telemetry and no update check.

**Not done, by the app or in its development:**

| Technique | Status |
|---|---|
| Writing to the game's memory (`WriteProcessMemory`) | never |
| DLL injection, remote threads | never |
| Function hooks, detours, patching code or data | never |
| Attaching a debugger | never |
| Memory scanning tools (Cheat Engine and the like) | never |
| Mod loaders (MelonLoader, BepInEx) | never |
| Sending keyboard or mouse input to the game | never |
| Modifying game files | never |
| Writing the save file | never |
| Reading the save file | never by the app. In development, a build-viewer test read a copy of the developer's own save with the earlier research tools |
| Intercepting or capturing network traffic | never |
| Requests to the game's or Ninja Kiwi's servers | never |
| Asking for or using account credentials | never |

On injection specifically: the developer's rule was "absolutely no modding or modifying game state or save files,
this should be read-only in reference to the game, its files, its saves, and its running process", with the
addition that "injecting a DLL that only reads is okay, as long as the game is unmodified". The outside reader was
enough, so no DLL was ever built.

## 2. How that knowledge was obtained

Almost all of it by **static analysis of the installed game's files**, with tools the AI wrote for the purpose.

**Type metadata.** The game is a Unity IL2CPP build. A hand-written parser reads `global-metadata.dat` (format
version 107) and the registration tables inside `GameAssembly.dll` (a PE file) to recover type names, field names
and offsets, enum members and method addresses. First in Python (`zar_meta.py`, `ztypes.py`, `zfields.py`), now
also in Rust in this repository (`crates/layoutgen`), which is what the app runs.

**Disassembly of the game's managed code** (`GameAssembly.dll`), with the capstone disassembler driven by the
AI's own scripts (`zdis.py`, `zraw.py`, `zxref.py`). Functions that were read include `GameRunner::Update`,
`WorldController::UpdateWorld`, `WorldController::GetGameEventExtraDataSize` (event payload sizes, from its jump
table), `GameHud::UpdateChat`, `HashedString.Get` (ids are CRC-32 of asset names),
`WorldLoadDataBuilder.WriteIdentityHashArrays`, and for the gear maths `BonusHelpers::ApplyBonusToDamageInfo`,
`ApplyBonusToWeapon`, `UpgradeBehaviour`, `BuildPlayerWeaponInternal`,
`PassiveBonusDefinition::TranslateModPointToRealBonus`, `Gear.GearFitsEnhancementRequirements`. After the first
release, for the reports that followed it: `GameRunner::GetCurrentLevelType` (how the game itself tells the hub from
a mission), `GameRunner::HandleGameEndRewards` (where the result data and the experience gained are written),
`AbilityDescriptionParameterHelper::GetDescriptionParameter`,
`GearModEnhancementDefinitionSO::GetDescriptionParameter`, `AbilityUpgradeBonusDefinition::GetBonus` and
`GetParameterBonus`, `PassiveBonusDefinition::GetParameterBonus` and `BonusEvaluationContext::ForModEnhancement`
(how the numbers in upgrade and enhancement descriptions are worked out).

**Disassembly of the game's native simulation library** (`sas-5-native.dll`, no symbols), with capstone through
`znative.py`. The library was only ever read as a file, never loaded or executed. This gave the layout of the
load-data block and the order of operations in a shot (charge, HD ammo, explosions).

**At run time the app re-does the part it needs.** It contains its own x86-64 instruction decoder
(`crates/layoutgen/src/x86.rs`) and recognises a few code shapes in the installed game to find: the address of the
`GameRunner` type slot (a `mov rax,[rip+..]; mov rax,[rax+..]` pattern), event payload sizes, and four numeric
factors of the mod maths that exist only as machine code. It stores the evidence for those next to the values.

**Asset bundles.** Definitions (ScriptableObjects), localisation text and pictures were first extracted with the
UnityPy library (`zassets.py`), and the Addressables catalog with the AI's own reader (`zcatalog.py`). This
repository now has its own readers (`crates/unity`): UnityFS with LZ4, SerializedFile type trees, the Addressables
catalog, sprites and textures (DXT1/DXT5 decoding; BC7 is not decoded).

**Live, read-only probing.** A few things were settled by reading the running game while the developer played,
with the same read-only access as above: the meaning of the mission-result fields, the layout of the resistance
tables (232 enemy definitions on that day), and checks of the Rust reader. After the first release a development
build was run next to the developer's own play, again read-only and writing to a scratch folder, and recorded a
story mission from start to result screen: that is the check that story missions are recognised and that the
experience gained is read.

**Level scenes.** Whether a level is the hub or a mission was confirmed for all thirteen level scenes by reading
the `Level` component of each scene's asset bundle with UnityPy (research tooling, not part of this repository);
the app reads the same field from the loaded level in memory.

**Things no file states**, taken as assumptions: the member order of the C++ `std::vector` in the native library
(Microsoft's ABI; only its size is confirmed by the metadata) and that the resistance tables hold 32-bit floats.

## 3. Tools

**Used.**

| Tool | For | In the shipped app? |
|---|---|---|
| Claude (`claude-fable-5-2`) in the Claude Code desktop app, with subagents | all analysis and all code | no |
| Python 3.13, `ctypes` | the first meter and all research scripts | no |
| capstone 5 (disassembler library) | reading the game's managed and native code | no |
| UnityPy 1.25 (with its texture and LZ4 dependencies) | first extraction of definitions, texts and pictures | no |
| Pillow, numpy | image handling and number crunching in the Python tools | no |
| Rust (stable), Tauri 2, tiny_http, serde, flate2, lz4_flex, png, jpeg-encoder, crc32fast, windows-sys | the app | yes |
| `xxd`, `strings` | first looks at game files | no |
| The browser pane of the Claude app | looking at the tool's own pages | no |

Two third-party projects served as a reference for behaviour that had to be matched exactly, and their licences
are reproduced in [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md): UnityPy (how a type tree is read) and Pillow
(resize, crop and polygon-fill arithmetic, so that pictures come out identical to the Python tool's).

**Not used.** On the first day the AI checked whether Il2CppDumper, Cpp2IL, Ghidra, ILSpy, dnSpy, x64dbg or radare
were installed. None was, so it wrote its own parser instead and none was ever installed or run. IDA, Cheat Engine,
Frida, MelonLoader, BepInEx, UnityExplorer, AssetRipper and AssetStudio were never used.

**From the internet.** The Python packages and Rust crates named above, from PyPI and crates.io. A Rust toolchain
update. One reading of the gbfr-logs repository and of relink.cleista.cc (section 4). Nothing else: no dumps, no
offsets, no data files from other people.

## 4. Inspiration

The idea came from [gbfr-logs](https://github.com/gbfr-logs/gbfr-logs), a damage meter for Granblue Fantasy:
Relink, and in particular from Cleista's fork of it at [relink.cleista.cc](https://relink.cleista.cc/en). The
developer's first request named both, and many design choices and quality-of-life features here were modelled on
that site. An AI subagent read that project's README and parts of its source to see how such a tool is put
together. Its approach (a DLL injected into the game that hooks functions) was not followed, and no code was taken
from it.

## 5. Where the tooling came from

zar-logs did not start from nothing. The reverse-engineering tools it was built with (the metadata parser, the
disassembly scripts, the asset extraction) and its understanding of the game's damage formulas came from earlier
private research by the same developer with the same AI, from 11 to 16 November 2026, on the game's loot
randomness and its damage formulas. That research is private, is not part of this repository and is not described
further here; this document covers zar-logs. zar-logs uses none of that project's code at run time.

The Python damage meter was written in that same private repository on 16 November, and ported to the Rust program
in this repository from 16 to 18 November.

Game data extracted during that research (definition dumps, localisation text) stays on the developer's machine.
None of it is in this repository.

## 6. What in this repository is derived from the game

No game asset is here: no picture, no text table, no data file, no code of the game. What is here, because a
program that reads the game has to name things:

- names of the game's types, fields, enum members and definition classes, and patterns of its localisation keys;
- names and addresses of a few of the game's functions, in comments and in a test script;
- asset names used as test cases (for example `HVM_MPG_MK13`, `Op_Geode_Map.unity`, `ModePods`);
- formulas read from the game's code (how a mod's points, quality and level combine);
- short x86 instruction byte sequences used as test cases for the instruction decoder.

The app icon is a neutral drawing made for this project.

## 7. Privacy of other players

In co-op the game's memory holds every party member's display name, class, loadout and account id. The meter saves
display names and loadouts in its logs, because that is what a damage meter shows. It does not save account ids.
Logs stay on your machine unless you share them.

## 8. Timeline

| When (2026) | What |
|---|---|
| 11 to 16 Nov | Earlier private research (section 5): metadata parser, disassembly scripts, asset extraction, damage formulas. |
| 16 Nov | Python damage meter: reader, event decoder, encounter model, web viewer, overlay, build viewer. First live runs by the developer. |
| 16 to 17 Nov | Rust + Tauri port, in this repository: core, app, layout generator, asset readers, names, gear maths, build viewer. |
| 17 Nov | Rarity odds spreadsheet (separate session). |
| 18 Nov | Everything derived per game version, nothing hard-coded; mod damage corrected to flat; explosion row; hub fix; preparation for publishing. |
| 18 Nov, after releases 0.1.0 and 0.1.1 | First reports from players: story missions and prologues recorded (the hub is now told apart by the level's own type); damage done by enemies no longer credited to players; damage to structures kept apart and left out by default; description numbers; experience gained; export and import of logs. |
