//! Diagnostics for the core: probe the running game, show where the game was found.
//!
//!   zar-logs-cli probe [--seconds N] [--layout <layout.json>] [--dump <out.jsonl>]
//!   zar-logs-cli paths
//!
//! `probe` attaches to the running game the way the app does (read-only), decodes for a few seconds and prints
//! what it saw; `--dump` also writes every decoded record as one JSON line. Without `--layout` it uses the layout
//! the app cached for the installed game version.

use std::collections::BTreeMap;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use zar_logs_core::layout::Layout;
use zar_logs_core::records::Rec;

struct Args {
    cmd: String,
    layout: Option<PathBuf>,
    dump: Option<PathBuf>,
    seconds: f64,
}

fn parse() -> Args {
    let mut a = Args { cmd: String::new(), layout: None, dump: None, seconds: 10.0 };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        match x.as_str() {
            "--layout" => a.layout = it.next().map(PathBuf::from),
            "--dump" => a.dump = it.next().map(PathBuf::from),
            "--seconds" => a.seconds = it.next().and_then(|s| s.parse().ok()).unwrap_or(10.0),
            _ if a.cmd.is_empty() => a.cmd = x,
            _ => fail(&format!("unexpected argument {x:?}")),
        }
    }
    a
}

/// The app's cache folder: ZARLOGS_CACHE, else the `cache` folder of the app's data folder.
fn cache_dir() -> PathBuf {
    if let Some(c) = std::env::var_os("ZARLOGS_CACHE") {
        return PathBuf::from(c);
    }
    zar_logs_core::paths::data_dir().join("cache")
}

/// The layout to use: `--layout`, else the one the app cached for the installed game version.
fn layout(arg: &Option<PathBuf>) -> Result<Layout, String> {
    if let Some(p) = arg {
        return Layout::load(p);
    }
    let game = zar_logs_core::paths::find_game().map(|g| g.0).ok_or("the game was not found (set ZAR_GAME_DIR)")?;
    let version = zar_logs_core::paths::game_version(&game)?;
    let path = cache_dir().join("game").join(&version.id).join("layout.json");
    eprintln!("layout {}", path.display());
    Layout::load(&path).map_err(|e| format!("{e} (start zar-logs once so it reads the game, or pass --layout)"))
}

fn fail(msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(1)
}

#[cfg(any(windows, target_os = "linux"))]
fn probe(a: &Args) {
    use zar_logs_core::capture::Decoder;
    use zar_logs_core::mem::Live;
    let lay = layout(&a.layout).unwrap_or_else(|e| fail(&e));
    let (proc, base) = match zar_logs_core::attach() {
        Ok(Some(found)) => found,
        Ok(None) => fail("the game is not running"),
        Err(e) => fail(&e),
    };
    println!("attached read-only to pid {}, GameAssembly.dll at {base:#x}", proc.pid);
    let mut mem = Live::new(proc.clone());
    let mut dec = Decoder::new(&lay, base);
    let mut out = a.dump.as_ref().map(|p| BufWriter::new(std::fs::File::create(p).unwrap_or_else(|e| fail(&format!("{}: {e}", p.display())))));
    let (mut kinds, mut recs): (BTreeMap<&'static str, u64>, Vec<Rec>) = (BTreeMap::new(), Vec::new());
    let start = std::time::Instant::now();
    while start.elapsed().as_secs_f64() < a.seconds && proc.alive() {
        recs.clear();
        if !dec.poll(&mut mem, &mut recs) {
            std::thread::sleep(std::time::Duration::from_micros(500));
        }
        for r in &recs {
            *kinds.entry(r.kind()).or_default() += 1;
            match r {
                Rec::Defs(d) => println!("world: level {:?} mode {:?} difficulty {} modifier {}  {} weapons, {} abilities, {} entities, {} effect sets, defences for {}",
                    d.level, d.mode, d.difficulty, d.modifier, d.weapons.len(), d.abilities.len(), d.entities.len(), d.sets.len(),
                    d.defense.as_ref().map(|x| x.ents.len()).unwrap_or(0)),
                Rec::Builds { players } => println!("loadouts: {} players", players.len()),
                _ => {}
            }
            if let Some(w) = &mut out {
                serde_json::to_writer(&mut *w, r).expect("serialise record");
                w.write_all(b"\n").expect("write record");
            }
        }
    }
    println!("records {kinds:?}");
    println!("decoder {:?}", dec.stats);
}

#[cfg(not(any(windows, target_os = "linux")))]
fn probe(_a: &Args) {
    fail("probe reads the running game, on Windows or Linux only")
}

fn main() {
    let a = parse();
    match a.cmd.as_str() {
        "probe" => probe(&a),
        "paths" => {
            match zar_logs_core::paths::find_game() {
                Some((p, how)) => println!("game:  {}  ({how})", p.display()),
                None => println!("game:  not found (set ZAR_GAME_DIR)"),
            }
            println!("steam: {}", zar_logs_core::paths::steam_roots().iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "));
            println!("cache: {}", cache_dir().display());
        }
        _ => {
            eprintln!("usage: zar-logs-cli probe [--seconds N] [--layout F] [--dump F] | paths");
            std::process::exit(2);
        }
    }
}
