//! zar-logs: damage meter and build viewer for Zombie Assault: Resurgence. Unofficial; reads the game, never writes.
//!
//! Starting the app starts everything: the capture thread that reads the running game (zar-logs-core) and records
//! each mission, the viewer window, and the in-game overlay. A local HTTP server carries the pages and their data
//! (server.rs).
//!
//! Settings live in `config.json` in the data folder (`%LOCALAPPDATA%\com.vicplusplus.zar-logs`); environment
//! variables override it:
//!   ZARLOGS_DATA    data folder
//!   ZARLOGS_LOGS    recorded encounters              config "logs"    default <data>\logs
//!   ZARLOGS_CACHE   what is read from the game       config "cache"   default <data>\cache
//!                   (one folder per game version: layout, tables, names, pictures; see gamedata.rs)
//!   ZARLOGS_PORT    port of the local server         config "port"    default 5175 (any free port when taken)
//!   ZAR_GAME_DIR    the game's folder                default: found (running game, Steam libraries)
//! config.json also keeps what the pages set: "overlay" (shown, position), and "structures" and "blasts" (count
//! damage to structures, and the blasts of enemies that explode, in the figures shown; both off by default).
//! `--headless` runs without windows; `--prepare-only` only reads the game's files into the cache and exits.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod dialogs;
mod gamedata;
mod logs;
mod server;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};
use tauri::Manager;
use zar_logs_core::capture::Decoder;
use zar_logs_core::mem::Live;
use zar_logs_core::names::Resolver;
use zar_logs_core::session::{self, Tracker};
use zar_logs_core::store;

pub struct App {
    pub store: logs::Store,
    data: PathBuf,
    config: Mutex<Value>,
    status: Mutex<Value>,
    /// what is being prepared from the game's files right now ("" = nothing): shown in the viewer's status line
    prep: Mutex<String>,
    capture: Mutex<Value>,
    tracker: Mutex<Tracker>,
    /// live payload of the most recent finished encounter (the overlay keeps showing it)
    last_closed: Mutex<Option<Value>>,
    handle: OnceLock<tauri::AppHandle>,
    /// what the build viewer needs from the game's data; empty until it has been read
    builds: Arc<std::sync::RwLock<Option<Arc<zar_logs_buildview::BuildData>>>>,
    /// names and pictures are being read from the game's files right now
    preparing: std::sync::atomic::AtomicBool,
}

impl App {
    /// What the pages poll: capture status, a number that changes with the list of logs, decoder counters, the
    /// mission in progress (or the one that just ended), and whether the overlay is shown.
    pub fn live(&self) -> Value {
        let t = self.tracker.lock().unwrap();
        let last = self.last_closed.lock().unwrap();
        // the hub is a loaded world but not a mission: it is named as the place the player is in, nothing more
        let place = t.cur.as_ref().and_then(|c| c.hub_name()).map(str::to_string);
        let mission = t.cur.as_ref().filter(|c| c.is_mission());
        let mut enc = match mission {
            Some(c) if c.server_hits > 0 || last.is_none() => Some(c.live()),
            cur => last.clone().or_else(|| cur.map(|c| c.live())),
        };
        let counted = self.counted();
        if let Some(e) = &mut enc {
            session::view(e, counted);
        }
        json!({ "status": *self.status.lock().unwrap(), "version": self.store.version(),
                "capture": *self.capture.lock().unwrap(), "encounter": enc, "place": place, "overlay": self.overlay_on(),
                "structures": counted.structures, "blasts": counted.blasts,
                "prep": *self.prep.lock().unwrap(),
                // for the About page
                "app": { "version": env!("CARGO_PKG_VERSION"), "repository": env!("CARGO_PKG_REPOSITORY"),
                         "data": self.data.display().to_string(),
                         "links": server::LINKS.iter().map(|(name, url)| (name.to_string(), json!(url))).collect::<serde_json::Map<_, _>>() } })
    }

    /// Logs, newest first; the mission in progress leads once it has damage.
    pub fn encounters(&self) -> Value {
        let mut rows = match self.store.list() { Value::Array(r) => r, _ => Vec::new() };
        let t = self.tracker.lock().unwrap();
        if let Some(c) = t.cur.as_ref().filter(|c| c.server_hits > 0 && c.is_mission()) {
            let mut row = c.summary();
            self.store.decorate(&mut row);
            rows.insert(0, row);
        }
        let counted = self.counted();
        rows.iter_mut().for_each(|row| session::view(row, counted));
        Value::Array(rows)
    }

    pub fn detail(&self, id: &str) -> Option<Value> {
        let live = {
            let t = self.tracker.lock().unwrap();
            t.cur.as_ref().filter(|c| c.id == id).map(|c| {
                let mut d = c.detail(&t.resolver);
                self.store.decorate(&mut d);
                d
            })
        };
        let mut d = live.or_else(|| self.store.detail(id))?;
        self.render_builds(&mut d);
        session::view(&mut d, self.counted());
        Some(d)
    }

    // ---- sharing logs ----
    /// The viewer's window, for a dialog to belong to (0 when there is none).
    fn owner(&self) -> isize {
        self.handle.get().and_then(|h| h.get_webview_window("main")).and_then(|w| w.hwnd().ok()).map_or(0, |h| h.0 as isize)
    }

    /// Asks which log files to add, and adds them.
    pub fn import(&self) -> Value {
        let files = dialogs::open_many(self.owner());
        if files.is_empty() {
            return json!({ "cancelled": true });
        }
        self.store.import(&files)
    }

    /// Shows a log's file in Explorer, selected: that is how a log is shared (dragged from there).
    pub fn reveal(&self, id: &str) -> Value {
        json!({ "ok": self.store.path_of(id).is_some_and(|path| dialogs::reveal(&path)) })
    }

    /// A log whose builds are still raw loadouts (recorded before the game's data was read, or by a version of
    /// the app without the build viewer) gets them rendered when it is opened.
    fn render_builds(&self, detail: &mut Value) {
        let Some(data) = self.builds.read().unwrap().clone() else { return };
        for p in detail.get_mut("players").and_then(Value::as_array_mut).into_iter().flatten() {
            let Some(b) = p.get("build").filter(|b| b["version"].as_u64() != Some(zar_logs_buildview::VERSION) && b["raw"].is_object()) else { continue };
            let rendered = data.render(&b["raw"]);
            p["build"] = rendered;
        }
    }

    /// A name resolver with the build renderer attached.
    fn resolver(&self) -> Resolver {
        let mut r = Resolver::load(&self.store.game_dir().join("names.json"));
        let builds = self.builds.clone();
        r.render_build = Some(Arc::new(move |raw: &Value| match builds.read().unwrap().as_ref() {
            Some(data) => data.render(raw),
            None => json!({ "version": 0, "raw": raw, "note": "This build has not been rendered yet." }),
        }));
        r
    }

    fn set_status(&self, v: Value) { *self.status.lock().unwrap() = v; }
    fn set_prep(&self, what: &str) { *self.prep.lock().unwrap() = what.to_string(); }

    // ---- settings ----
    fn save_config(&self) {
        let text = serde_json::to_string_pretty(&*self.config.lock().unwrap()).unwrap_or_default();
        let _ = std::fs::write(self.data.join("config.json"), text);
    }
    pub fn overlay_on(&self) -> bool { self.config.lock().unwrap()["overlay"]["enabled"].as_bool().unwrap_or(true) }

    /// What the figures shown count besides the players' own damage to enemies: damage to structures (crates,
    /// pods, barrels, level geometry) and the blasts of enemies that explode. Both off unless the user turns them
    /// on; every log has the figures for each choice, so this only changes the view.
    pub fn counted(&self) -> session::Counted {
        let c = self.config.lock().unwrap();
        session::Counted { structures: c["structures"].as_bool().unwrap_or(false), blasts: c["blasts"].as_bool().unwrap_or(false) }
    }
    /// Sets one of the two (`what` is "structures" or "blasts") and remembers it.
    pub fn set_counted(&self, what: &str, on: bool) -> bool {
        self.config.lock().unwrap()[what] = json!(on);
        self.save_config();
        on
    }

    /// Show or hide the overlay window and remember the choice.
    pub fn set_overlay(&self, on: bool) -> bool {
        self.config.lock().unwrap()["overlay"]["enabled"] = json!(on);
        self.save_config();
        if let Some(w) = self.handle.get().and_then(|h| h.get_webview_window("overlay")) {
            let _ = if on { w.show() } else { w.hide() };
        }
        on
    }
}

fn dir(var: &str, default: impl FnOnce() -> PathBuf) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(default)
}

/// Make sure the tables of values, the names and the pictures are those of the installed game version, on a thread
/// of its own (it must not hold up the capture). Called at start-up and each time the game is attached to, so a
/// game update is picked up without restarting the app.
fn prepare(app: Arc<App>, game: PathBuf) {
    use std::sync::atomic::Ordering;
    if app.preparing.swap(true, Ordering::SeqCst) {
        return; // already running
    }
    std::thread::Builder::new().name("zar-logs-gamedata".into()).spawn(move || {
        let ready = gamedata::ensure_tables(&app.store.cache, &game, &|what| app.set_prep(what));
        app.set_prep("");
        match ready {
            Ok(ready) => {
                let changed = ready.made || app.store.game_dir() != ready.dir;
                app.store.set_game_dir(ready.dir);
                *app.builds.write().unwrap() = Some(Arc::new(ready.builds));
                if let Some(w) = &ready.warning {
                    app.set_prep(w); // stays in the status line: the user should know
                }
                while changed {
                    // other names: swap them in between missions, so the one being recorded keeps its tables
                    {
                        let mut t = app.tracker.lock().unwrap();
                        if t.cur.is_none() {
                            t.resolver = app.resolver();
                            break;
                        }
                    }
                    std::thread::sleep(Duration::from_secs(5));
                }
            }
            Err(e) => app.set_prep(&format!("the game's values could not be read: {e}")),
        }
        app.preparing.store(false, Ordering::SeqCst);
    }).ok();
}

/// Attach to the game whenever it runs, decode it and record every mission. Read-only towards the game.
fn capture(app: Arc<App>) {
    loop {
        app.set_status(json!({ "state": "waiting" }));
        let Some((proc, base)) = zar_logs_core::attach() else {
            std::thread::sleep(Duration::from_secs(2));
            continue;
        };
        // the layout belongs to the build that is running: derived from its files when the cache has none
        let game = proc.image_path().and_then(|p| p.parent().map(PathBuf::from));
        let lay = game.clone().ok_or_else(|| "the game's folder is unknown".to_string())
            .and_then(|game| gamedata::ensure_layout(&app.store.cache, &game, &|what| app.set_prep(what)));
        if !app.preparing.load(std::sync::atomic::Ordering::SeqCst) {
            app.set_prep("");
        }
        if let Some(game) = game {
            prepare(app.clone(), game);
        }
        let lay = match lay {
            Ok(lay) => lay,
            Err(e) => {
                app.set_status(json!({ "state": "error", "error": e }));
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
        };
        {
            // the names pack may have appeared since the app started
            let mut t = app.tracker.lock().unwrap();
            if !t.resolver.pack_loaded && app.store.game_dir().join("names.json").is_file() {
                t.resolver = app.resolver();
            }
        }
        app.set_status(json!({ "state": "attached", "pid": proc.pid }));
        let mut mem = Live::new(proc.clone());
        let mut dec = Decoder::new(&lay, base);
        let (mut recs, mut polls) = (Vec::new(), 0u32);
        loop {
            recs.clear();
            let fresh = dec.poll(&mut mem, &mut recs);
            if !recs.is_empty() {
                let mut t = app.tracker.lock().unwrap();
                t.feed(&recs);
                save_closed(&app, &mut t);
            }
            if !fresh {
                std::thread::sleep(Duration::from_micros(500));
            }
            polls += 1;
            if polls % 512 == 0 {
                *app.capture.lock().unwrap() = json!({ "frames": dec.stats.frames, "events": dec.stats.events,
                    "hits": dec.stats.hits, "late": dec.stats.late, "torn": dec.stats.torn });
                if !proc.alive() {
                    break;
                }
            }
        }
        // the game closed: whatever was being recorded ends here
        let mut t = app.tracker.lock().unwrap();
        t.close();
        save_closed(&app, &mut t);
    }
}

fn save_closed(app: &App, t: &mut Tracker) {
    let closed: Vec<_> = t.closed.drain(..).collect();
    for enc in closed {
        match store::save(&app.store.logs, &enc, &t.resolver) {
            Ok(Some(_)) => *app.last_closed.lock().unwrap() = Some(enc.live()),
            Ok(None) => {}
            Err(e) => {
                eprintln!("could not save encounter {}: {e}", enc.id);
                app.set_prep(&format!("could not save the last mission: {e}"));
            }
        }
    }
}

/// Before the app had an installer its data was in `%LOCALAPPDATA%\zar-logs`, which is now the program's own
/// folder. What is still there is moved to the data folder once; nothing is overwritten.
fn move_old_data(data: &std::path::Path) {
    let Some(old) = zar_logs_core::paths::old_data_dir().filter(|old| old != data) else { return };
    for name in ["config.json", "logs", "cache"] {
        let (from, to) = (old.join(name), data.join(name));
        if from.exists() && !to.exists() {
            let _ = std::fs::rename(&from, &to);
        }
    }
}

fn main() {
    let data = zar_logs_core::paths::data_dir();
    let _ = std::fs::create_dir_all(&data);
    move_old_data(&data);
    let config: Value = std::fs::read_to_string(data.join("config.json")).ok().and_then(|t| serde_json::from_str(&t).ok())
        .filter(Value::is_object).unwrap_or_else(|| json!({}));
    let from_config = |key: &str, default: PathBuf| config[key].as_str().filter(|s| !s.is_empty()).map(PathBuf::from).unwrap_or(default);
    let logs = dir("ZARLOGS_LOGS", || from_config("logs", data.join("logs")));
    let cache = dir("ZARLOGS_CACHE", || from_config("cache", data.join("cache")));
    let _ = std::fs::create_dir_all(&logs);
    let _ = std::fs::create_dir_all(&cache);
    if std::env::args().any(|a| a == "--prepare-only") {
        // no windows, no capture: make the layout, names and pictures for the installed game and say how it went
        let Some((game, how)) = zar_logs_core::paths::find_game() else {
            eprintln!("the game was not found (set ZAR_GAME_DIR)");
            std::process::exit(1);
        };
        println!("game: {} ({how})
cache: {}", game.display(), cache.display());
        let started = std::time::Instant::now();
        let note = |what: &str| println!("[{:6.1}s] {what}", started.elapsed().as_secs_f64());
        let result = gamedata::ensure_layout(&cache, &game, &note).and_then(|_| gamedata::ensure_tables(&cache, &game, &note));
        match result {
            Ok(ready) => {
                println!("[{:6.1}s] {}: {}", started.elapsed().as_secs_f64(), if ready.made { "made" } else { "already current" }, ready.dir.display());
                if let Some(w) = ready.warning {
                    println!("warning: {w}");
                }
            }
            Err(e) => {
                eprintln!("failed: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    let want = std::env::var("ZARLOGS_PORT").ok().and_then(|p| p.parse().ok())
        .or_else(|| config["port"].as_u64().map(|p| p as u16)).unwrap_or(5175);
    let overlay_at = (config["overlay"]["x"].as_f64(), config["overlay"]["y"].as_f64());
    let app = Arc::new(App {
        store: logs::Store::new(logs, cache), data, config: Mutex::new(config), status: Mutex::new(json!({ "state": "starting" })),
        prep: Mutex::new(String::new()), capture: Mutex::new(json!({})), tracker: Mutex::new(Tracker::new(Resolver::empty())), last_closed: Mutex::new(None),
        handle: OnceLock::new(), builds: Arc::new(std::sync::RwLock::new(None)), preparing: std::sync::atomic::AtomicBool::new(false),
    });
    // what was read from the game last time, so logs can be viewed at once and without the game
    if let Some(dir) = gamedata::last_dir(&app.store.cache) {
        if let Some(builds) = gamedata::stored_builds(&dir) {
            *app.builds.write().unwrap() = Some(Arc::new(builds));
        }
        app.store.set_game_dir(dir);
    }
    app.tracker.lock().unwrap().resolver = app.resolver();
    let port = server::start(app.clone(), want).expect("could not start the local server");
    println!("zar-logs (read-only): http://127.0.0.1:{port}/");
    {
        let app = app.clone();
        // names and pictures for the installed game, without waiting for it to be started
        if let Some((game, _)) = zar_logs_core::paths::find_game() {
            prepare(app.clone(), game);
        }
        std::thread::Builder::new().name("zar-logs-capture".into()).spawn(move || {
            loop {
                // a fault while decoding must not end the capture for good: say so in the viewer and start over
                let run = app.clone();
                if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || capture(run))) {
                    let msg = e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
                    app.set_status(json!({ "state": "error", "error": format!("capture stopped and is restarting: {msg}") }));
                    app.tracker.clear_poison();
                    std::thread::sleep(Duration::from_secs(3));
                }
            }
        }).expect("capture thread");
    }
    if std::env::args().any(|a| a == "--headless") {
        // development: the server and the capture without any window (pages are reachable in a browser)
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }
    let state = app.clone();
    tauri::Builder::default()
        .setup(move |tauri_app| {
            let _ = state.handle.set(tauri_app.handle().clone());
            let url = |path: &str| tauri::WebviewUrl::External(format!("http://127.0.0.1:{port}/{path}").parse().expect("local url"));
            // The windows only ever show the app's own pages. A link to somewhere else is opened in the user's
            // browser (server.rs `LINKS`), and the window itself refuses to go anywhere but its local server.
            let local_only = |url: &tauri::Url| !matches!(url.scheme(), "http" | "https") || url.host_str() == Some("127.0.0.1");
            let main = tauri::WebviewWindowBuilder::new(tauri_app, "main", url(""))
                .title("zar-logs")
                .inner_size(1360.0, 900.0)
                .min_inner_size(720.0, 480.0)
                .on_navigation(local_only)
                .build()?;
            // the in-game overlay: a small always-on-top panel that never takes focus from the game
            let mut ov = tauri::WebviewWindowBuilder::new(tauri_app, "overlay", url("overlay"))
                .title("zar-logs overlay")
                .on_navigation(local_only)
                .inner_size(330.0, 176.0)
                .decorations(false)
                .transparent(true)
                .shadow(false)
                .always_on_top(true)
                .skip_taskbar(true)
                .resizable(false)
                .focused(false)
                .visible(state.overlay_on());
            if let (Some(x), Some(y)) = overlay_at {
                ov = ov.position(x, y);
            }
            let ov = ov.build()?;
            no_activate(&ov);
            let app = state.clone();
            ov.on_window_event(move |ev| {
                if let tauri::WindowEvent::Moved(pos) = ev {
                    // remember where the overlay was put (logical pixels are close enough at one scale factor)
                    let mut c = app.config.lock().unwrap();
                    c["overlay"]["x"] = json!(pos.x);
                    c["overlay"]["y"] = json!(pos.y);
                    drop(c);
                    app.save_config();
                }
            });
            let handle = tauri_app.handle().clone();
            let closing = state.clone();
            main.on_window_event(move |ev| {
                if let tauri::WindowEvent::CloseRequested { .. } = ev {
                    // Closing the viewer closes the overlay and stops the capture. A mission in progress ends
                    // here and is saved first; the lock is held until the process is gone, so the capture
                    // thread cannot be cut off in the middle of a save of its own.
                    let mut t = closing.tracker.lock().unwrap_or_else(|e| e.into_inner());
                    t.close();
                    save_closed(&closing, &mut t);
                    handle.exit(0);
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running zar-logs");
}

/// Clicking the overlay must not take keyboard focus away from the game: WS_EX_NOACTIVATE on its window.
#[cfg(windows)]
fn no_activate(w: &tauri::WebviewWindow) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW};
    if let Ok(hwnd) = w.hwnd() {
        // SAFETY: a style change on our own window.
        unsafe {
            let h = hwnd.0 as _;
            let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
            SetWindowLongPtrW(h, GWL_EXSTYLE, ex | (WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW) as isize);
        }
    }
}
#[cfg(not(windows))]
fn no_activate(_w: &tauri::WebviewWindow) {}

#[cfg(test)]
mod tests {
    #[test]
    fn the_identifier_is_the_one_the_data_folder_is_named_after() {
        let conf: serde_json::Value = serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert_eq!(conf["identifier"], zar_logs_core::paths::APP_IDENTIFIER);
    }
}
