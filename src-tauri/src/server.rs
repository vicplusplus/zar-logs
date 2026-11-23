//! The page's backend: a local HTTP server on 127.0.0.1. The window loads the page from it, and a browser can too.
//!
//!   /                       the page          /static/*      its scripts and stylesheet (compiled in)
//!   /icons/*                pictures extracted from the local install (the game version's folder)
//!   /api/encounters         list of logs      /api/encounter/<id>   one log (GET, DELETE)
//!   /api/records[?version=<id>|all]   the best missions of this machine's own characters, per game version
//!   /api/live, /api/stream  capture status and the mission in progress (once, or as server-sent events)
//!   /overlay                the in-game overlay's page        /api/overlay/toggle   show / hide it (POST)
//!   /api/open/<name>        open one of the links of `LINKS` in the default browser (POST)
//!   /api/encounter/<id>/reveal    show the log's file in Explorer, selected (POST)
//!   /api/import             add logs from files the user picks (POST)
//!   /api/structures/on, /off    whether damage to structures is counted in what the pages show (POST)
//!   /api/blasts/on, /off        the same for the blasts of enemies that explode (POST)
//!
//! Everything that changes or does something is a POST or DELETE and must carry the header `X-Zar-Logs`: a page
//! of some other site open in a browser cannot send that to this server, so only the app's own pages can ask.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

use crate::App;

const INDEX: &str = include_str!("../../web/index.html");
const APP_JS: &str = include_str!("../../web/app.js");
const CHARTS_JS: &str = include_str!("../../web/charts.js");
const STYLE: &str = include_str!("../../web/style.css");
const OVERLAY: &str = include_str!("../../web/overlay.html");

/// The outside links the pages show, by name. The page can ask for one of these to be opened in the default
/// browser, and for nothing else: the request carries a name from this list, never an address.
pub const LINKS: &[(&str, &str)] = &[
    ("repo", env!("CARGO_PKG_REPOSITORY")),
    ("readme", concat!(env!("CARGO_PKG_REPOSITORY"), "/blob/main/README.md#ai-disclosure")),
    ("methods", concat!(env!("CARGO_PKG_REPOSITORY"), "/blob/main/METHODS.md")),
    ("issues", concat!(env!("CARGO_PKG_REPOSITORY"), "/issues")),
    ("releases", concat!(env!("CARGO_PKG_REPOSITORY"), "/releases")),
    ("terms", "https://ninjakiwi.com/terms"),
    ("gbfr-logs", "https://github.com/gbfr-logs/gbfr-logs"),
    ("relink", "https://relink.cleista.cc/en"),
];

/// Hands an address to Windows to open with the default browser. Only ever called with an entry of `LINKS`.
#[cfg(windows)]
fn open_in_browser(url: &str) -> bool {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (verb, file) = (wide("open"), wide(url));
    // SAFETY: both strings are NUL-terminated and outlive the call; the other arguments are null.
    let result = unsafe { ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL) };
    result as usize > 32 // ShellExecute's own convention: values up to 32 are errors
}
/// The same with the desktop's own opener (`xdg-open` on Linux, `open` on macOS).
#[cfg(not(windows))]
fn open_in_browser(url: &str) -> bool { crate::dialogs::run_detached(if cfg!(target_os = "macos") { "open" } else { "xdg-open" }, &[url]) }

/// The header the app's own pages send with every request that changes or does something.
const OWN_PAGE: &str = "X-Zar-Logs";

fn header(k: &str, v: &str) -> Header { Header::from_bytes(k.as_bytes(), v.as_bytes()).expect("static header") }

fn text(status: u16, ctype: &str, body: impl Into<Vec<u8>>) -> Response<std::io::Cursor<Vec<u8>>> {
    // the page's scripts change with every update of the app: never let the webview keep an old copy
    Response::from_data(body).with_status_code(StatusCode(status)).with_header(header("Content-Type", ctype))
        .with_header(header("Cache-Control", "no-store"))
}

fn json(v: &serde_json::Value) -> Response<std::io::Cursor<Vec<u8>>> { text(200, "application/json", v.to_string()) }

/// Binds 127.0.0.1 on the wanted port (or any free one when it is taken) and serves from a background thread.
pub fn start(app: Arc<App>, port: u16) -> Result<u16, String> {
    let server = Server::http(("127.0.0.1", port)).or_else(|_| Server::http(("127.0.0.1", 0))).map_err(|e| e.to_string())?;
    let port = server.server_addr().to_ip().map(|a| a.port()).ok_or("no local address")?;
    std::thread::Builder::new().name("zar-logs-http".into()).spawn(move || {
        for req in server.incoming_requests() {
            let app = app.clone();
            // one thread per request: the event stream stays open for as long as the page does
            std::thread::spawn(move || handle(&app, req));
        }
    }).map_err(|e| e.to_string())?;
    Ok(port)
}

fn handle(app: &App, req: Request) {
    // the UI is for this machine only: reject requests made through another host name (DNS rebinding)
    let host = req.headers().iter().find(|h| h.field.equiv("Host")).map(|h| h.value.as_str().to_string()).unwrap_or_default();
    if !matches!(host.split(':').next().unwrap_or(""), "127.0.0.1" | "localhost") {
        let _ = req.respond(text(403, "text/plain", "local only"));
        return;
    }
    let path = req.url().split('?').next().unwrap_or("").to_string();
    let method = req.method().clone();
    if method != Method::Get && !req.headers().iter().any(|h| h.field.equiv(OWN_PAGE)) {
        let _ = req.respond(text(403, "text/plain", "not from the app's own page"));
        return;
    }
    // "/api/encounter/<id>/<action>"
    let action = |name: &str| path.strip_prefix("/api/encounter/").and_then(|rest| rest.strip_suffix(name)).and_then(|id| id.strip_suffix('/'));
    let resp = match (&method, path.as_str()) {
        (Method::Get, "/") => text(200, "text/html; charset=utf-8", INDEX),
        (Method::Get, "/static/app.js") => text(200, "text/javascript; charset=utf-8", APP_JS),
        (Method::Get, "/static/charts.js") => text(200, "text/javascript; charset=utf-8", CHARTS_JS),
        (Method::Get, "/static/style.css") => text(200, "text/css; charset=utf-8", STYLE),
        (Method::Get, "/overlay") => text(200, "text/html; charset=utf-8", OVERLAY),
        (Method::Post, p) if p.starts_with("/api/open/") => {
            match LINKS.iter().find(|(name, _)| *name == &p["/api/open/".len()..]) {
                Some((_, url)) => json(&serde_json::json!({ "ok": open_in_browser(url) })),
                None => text(404, "text/plain", "no such link"),
            }
        }
        (Method::Post, "/api/overlay/toggle") => json(&serde_json::json!({ "overlay": app.set_overlay(!app.overlay_on()) })),
        (Method::Post, "/api/structures/on" | "/api/structures/off" | "/api/blasts/on" | "/api/blasts/off") => {
            let mut parts = path["/api/".len()..].split('/');
            let (what, on) = (parts.next().unwrap_or(""), parts.next() == Some("on"));
            json(&serde_json::json!({ what: app.set_counted(what, on) }))
        }
        (Method::Post, "/api/import") => json(&app.import()),
        (Method::Post, _) if action("reveal").is_some() => json(&app.reveal(action("reveal").unwrap_or(""))),
        (Method::Get, "/api/encounters") => json(&app.encounters()),
        (Method::Get, "/api/records") => {
            // ?version=<game version id> or "all"
            let query = req.url().split_once('?').map(|q| q.1).unwrap_or("");
            let version = query.split('&').find_map(|kv| kv.strip_prefix("version=")).filter(|v| v.chars().all(|c| c.is_ascii_alphanumeric()));
            json(&app.records(version))
        }
        (Method::Get, "/api/live") => json(&app.live()),
        (Method::Get, "/api/stream") => return stream(app, req),
        (Method::Get, p) if p.starts_with("/api/encounter/") => match app.detail(&p["/api/encounter/".len()..]) {
            Some(d) => json(&d),
            None => text(404, "text/plain", "unknown encounter"),
        },
        (Method::Delete, p) if p.starts_with("/api/encounter/") => {
            json(&serde_json::json!({ "ok": app.store.delete(&p["/api/encounter/".len()..]) }))
        }
        (Method::Get, p) if p.starts_with("/icons/") => {
            let name = &p["/icons/".len()..];
            let plain = !name.is_empty() && !name.contains(['/', '\\']) && !name.contains("..");
            match plain.then(|| std::fs::read(app.store.game_dir().join("icons").join(name)).ok()).flatten() {
                Some(bytes) => {
                    let ctype = if name.ends_with(".jpg") { "image/jpeg" } else { "image/png" };
                    Response::from_data(bytes).with_header(header("Content-Type", ctype))
                        .with_header(header("Cache-Control", "max-age=3600"))
                }
                None => text(404, "text/plain", "no such icon"),
            }
        }
        _ => text(404, "text/plain", "not found"),
    };
    let _ = req.respond(resp);
}

/// Server-sent events: the capture status twice a second, until the page goes away.
fn stream(app: &App, req: Request) {
    let mut w = req.into_writer();
    if w.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n").is_err() {
        return;
    }
    loop {
        let msg = format!("data: {}\n\n", app.live());
        if w.write_all(msg.as_bytes()).and_then(|_| w.flush()).is_err() {
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}
