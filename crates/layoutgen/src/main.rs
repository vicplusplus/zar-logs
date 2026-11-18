//! Command line front end of the layout generator:
//!
//! ```text
//! zar-logs-layoutgen --game <game folder> --out <file.json>
//! ```
//!
//! Exists so the layout can be produced and inspected without the app (after a game update, say). It only reads
//! the game folder; as a guard against a mistyped `--out` it refuses
//! to write inside the game folder or the Steam installation the game is in.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

const USAGE: &str = "usage: zar-logs-layoutgen --game <game folder> --out <file.json>";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("zar-logs-layoutgen: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let (mut game, mut out) = (None, None);
    let mut args = std::env::args_os().skip(1);
    while let Some(a) = args.next() {
        let slot = match a.to_str() {
            Some("--game") => &mut game,
            Some("--out") => &mut out,
            _ => return Err(USAGE.into()),
        };
        *slot = Some(PathBuf::from(args.next().ok_or(USAGE)?));
    }
    let (Some(game), Some(out)) = (game, out) else { return Err(USAGE.into()) };
    refuse_game_folders(&game, &out)?;

    let started = Instant::now();
    let text = zar_logs_layoutgen::generate_text(&game)?;
    let took = started.elapsed();
    std::fs::write(&out, &text).map_err(|e| format!("{}: {e}", out.display()))?;
    eprintln!(
        "{} written in {:.2} s (cache name for this build: {})",
        out.display(),
        took.as_secs_f64(),
        zar_logs_layoutgen::cache_file_name(&game)?
    );
    Ok(())
}

/// Error when `out` lies in the game folder or anywhere in the Steam installation that contains it.
fn refuse_game_folders(game: &Path, out: &Path) -> Result<(), String> {
    let absolute = |p: &Path| std::path::absolute(p).map_err(|e| format!("{}: {e}", p.display()));
    // the output file usually does not exist yet, so resolve its folder and compare from there
    let resolve = |p: &Path| p.canonicalize().or_else(|_| absolute(p));
    let game = resolve(game)?;
    let out_abs = absolute(out)?;
    let out_dir = resolve(out_abs.parent().unwrap_or(&out_abs))?;
    // <steam>/steamapps/common/<game>: protect everything from <steam> down
    let steam = game.ancestors().find(|a| a.file_name().is_some_and(|n| n.eq_ignore_ascii_case("steamapps"))).and_then(Path::parent);
    let protected = steam.unwrap_or(&game);
    if out_dir.starts_with(protected) {
        // canonical Windows paths carry a `\\?\` prefix that only clutters the message
        let shown = protected.display().to_string();
        let shown = shown.strip_prefix(r"\\?\").unwrap_or(&shown);
        return Err(format!("refusing to write into {shown} (the game's files are read-only for this tool)"));
    }
    Ok(())
}
