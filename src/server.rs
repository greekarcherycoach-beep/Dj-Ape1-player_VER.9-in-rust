//! Local HTTP server (127.0.0.1 only). Serves the UI, streams audio files with Range
//! support, and exposes the JSON "RPC" used by assets/bridge.js. Every endpoint is
//! protected by a random per-run token and a Host-header check (DNS-rebinding guard).

use crate::state::{is_audio, mtime_ms, reveal_in_file_manager, AppState};
use crate::tags;
use percent_encoding::percent_decode_str;
use serde_json::{json, Value};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

const PREFERRED_PORT: u16 = 47381; // fixed so the web origin (localStorage) stays stable

pub fn start(state: Arc<AppState>, html: String) -> u16 {
    let server = Server::http(("127.0.0.1", PREFERRED_PORT))
        .or_else(|_| Server::http(("127.0.0.1", 0)))
        .expect("cannot bind local server");
    let port = server.server_addr().to_ip().map(|a| a.port()).unwrap_or(PREFERRED_PORT);
    let server = Arc::new(server);
    let html = Arc::new(html.replace(
        "\"djfile://local/\"",
        &format!("\"/f/{}/\"", state.token),
    ));
    for _ in 0..32 {
        let (server, state, html) = (server.clone(), state.clone(), html.clone());
        std::thread::spawn(move || {
            while let Ok(rq) = server.recv() {
                let (state, html) = (state.clone(), html.clone());
                // never let one bad request kill a worker
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle(&state, &html, rq)));
            }
        });
    }
    port
}

fn hdr(k: &str, v: &str) -> Header {
    Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap()
}

fn reply(rq: Request, code: u16, ctype: &str, body: Vec<u8>) {
    let r = Response::from_data(body)
        .with_status_code(StatusCode(code))
        .with_header(hdr("Content-Type", ctype))
        .with_header(hdr("Cache-Control", "no-store"));
    let _ = rq.respond(r);
}
fn text(rq: Request, code: u16, s: &str) {
    reply(rq, code, "text/plain; charset=utf-8", s.as_bytes().to_vec())
}

fn host_ok(rq: &Request) -> bool {
    rq.headers().iter().any(|h| {
        h.field.equiv("Host") && {
            let v = h.value.as_str();
            v.starts_with("127.0.0.1:") || v.starts_with("localhost:")
        }
    })
}

fn mime(p: &Path) -> &'static str {
    match p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("mp3") => "audio/mpeg",
        Some("flac") => "audio/flac",
        Some("m4a") | Some("mp4") => "audio/mp4",
        Some("aac") => "audio/aac",
        Some("ogg") | Some("oga") | Some("opus") => "audio/ogg",
        Some("wav") => "audio/wav",
        Some("wma") => "audio/x-ms-wma",
        Some("aif") | Some("aiff") => "audio/aiff",
        Some("json") => "application/json",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("png") => "image/png",
        _ => "application/octet-stream",
    }
}

fn handle(st: &AppState, html: &str, mut rq: Request) {
    if !host_ok(&rq) {
        return text(rq, 403, "forbidden");
    }
    let url = rq.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    let method = rq.method().clone();

    if method == Method::Get && (path == "/" || path == "/index.html") {
        return reply(rq, 200, "text/html; charset=utf-8", html.as_bytes().to_vec());
    }
    let tok = format!("/{}/", st.token);
    let seg: Vec<&str> = path.trim_start_matches('/').splitn(3, '/').collect();
    // seg = [kind, token, rest]
    if seg.len() < 2 || seg[1] != st.token {
        let _ = tok;
        return text(rq, 404, "not found");
    }
    let rest = seg.get(2).copied().unwrap_or("");
    match (seg[0], &method) {
        ("f", Method::Get) | ("f", Method::Head) => {
            let p = PathBuf::from(percent_decode_str(rest).decode_utf8_lossy().to_string());
            serve_file(rq, &p)
        }
        ("rpc", Method::Post) => {
            let mut body = Vec::new();
            let _ = rq.as_reader().read_to_end(&mut body);
            let arg: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            match rpc(st, rest, arg) {
                Ok(v) => reply(rq, 200, "application/json", serde_json::to_vec(&v).unwrap_or_default()),
                Err(e) => text(rq, 500, &e),
            }
        }
        ("lib", Method::Get) => match st.lib_load() {
            Some((b, from)) => {
                let r = Response::from_data(b)
                    .with_header(hdr("Content-Type", "application/json; charset=utf-8"))
                    .with_header(hdr("X-From", &from))
                    .with_header(hdr("Cache-Control", "no-store"));
                let _ = rq.respond(r);
            }
            None => {
                let _ = rq.respond(Response::empty(StatusCode(204)));
            }
        },
        ("lib", Method::Put) => {
            let mut body = Vec::new();
            if let Err(e) = rq.as_reader().read_to_end(&mut body) {
                return text(rq, 400, &e.to_string());
            }
            match st.lib_save(&body) {
                Ok(()) => text(rq, 200, "ok"),
                Err(e) => {
                    st.log(&format!("libSave failed: {e}"));
                    text(rq, 500, &e)
                }
            }
        }
        ("libbk", Method::Get) => {
            let name = percent_decode_str(rest).decode_utf8_lossy().to_string();
            match st.lib_backup_read(&name) {
                Ok(b) => reply(rq, 200, "application/json; charset=utf-8", b),
                Err(e) => text(rq, 404, &e),
            }
        }
        ("w", Method::Put) => {
            let id: u64 = rest.parse().unwrap_or(0);
            let pos = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("pos="))
                .and_then(|v| v.parse::<u64>().ok());
            let mut body = Vec::new();
            if let Err(e) = rq.as_reader().read_to_end(&mut body) {
                return text(rq, 400, &e.to_string());
            }
            match st.file_write(id, pos, &body) {
                Ok(()) => text(rq, 200, "ok"),
                Err(e) => text(rq, 500, &e),
            }
        }
        _ => text(rq, 404, "not found"),
    }
}

fn serve_file(rq: Request, path: &Path) {
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(_) => return text(rq, 404, "not found"),
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let range = rq
        .headers()
        .iter()
        .find(|h| h.field.equiv("Range"))
        .and_then(|h| h.value.as_str().strip_prefix("bytes=").map(|s| s.to_string()));
    let (mut start, mut end, mut code) = (0u64, len.saturating_sub(1), 200u16);
    if let Some(r) = range {
        let first = r.split(',').next().unwrap_or("");
        if let Some((a, b)) = first.split_once('-') {
            match (a.trim().parse::<u64>().ok(), b.trim().parse::<u64>().ok()) {
                (Some(a), Some(b)) => { start = a; end = b.min(len.saturating_sub(1)); code = 206; }
                (Some(a), None) => { start = a; code = 206; }
                (None, Some(n)) => { start = len.saturating_sub(n); code = 206; }
                _ => {}
            }
        }
        if code == 206 && (start >= len || start > end) {
            let r = Response::empty(StatusCode(416)).with_header(hdr("Content-Range", &format!("bytes */{len}")));
            let _ = rq.respond(r);
            return;
        }
    }
    let n = if len == 0 { 0 } else { end - start + 1 };
    if start > 0 && f.seek(SeekFrom::Start(start)).is_err() {
        return text(rq, 500, "seek failed");
    }
    let mut headers = vec![
        hdr("Content-Type", mime(path)),
        hdr("Accept-Ranges", "bytes"),
        hdr("Cache-Control", "no-store"),
    ];
    if code == 206 {
        headers.push(hdr("Content-Range", &format!("bytes {start}-{end}/{len}")));
    }
    let resp = Response::new(StatusCode(code), headers, f.take(n), Some(n as usize), None);
    let _ = rq.respond(resp);
}

fn entry(p: &Path, name: &str) -> Option<Value> {
    let m = fs::metadata(p).ok()?;
    Some(json!({"name": name, "dir": m.is_dir(), "size": m.len(), "mtime": mtime_ms(&m)}))
}

fn rpc(st: &AppState, method: &str, a: Value) -> Result<Value, String> {
    match method {
        "stat" => {
            let list = a.as_array().cloned().unwrap_or_default();
            let mut out = vec![];
            for p in list.iter().filter_map(Value::as_str) {
                let pb = Path::new(p);
                let name = pb.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| p.to_string());
                if let Some(mut e) = entry(pb, &name) {
                    e["path"] = json!(p);
                    out.push(e);
                }
            }
            Ok(Value::Array(out))
        }
        "readdir" => {
            let p = a.as_str().ok_or("missing path")?;
            let mut out = vec![];
            for e in fs::read_dir(p).map_err(|e| e.to_string())?.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with('.') { continue; }
                // follow symlinks (metadata), skip unreadable entries
                if let Some(v) = entry(&e.path(), &name) {
                    if v["dir"] == json!(true) || is_audio(&e.path()) { out.push(v); }
                }
            }
            Ok(Value::Array(out))
        }
        "readTags" => Ok(tags::read(a.as_str().ok_or("missing path")?)),
        "libInfo" => Ok(st.lib_info()),
        "libBackups" => Ok(st.lib_backups()),
        "libBackupNow" => st.lib_backup_now(),
        "openLibDir" => {
            let _ = open::that(&st.data_dir);
            Ok(Value::Null)
        }
        "showItem" => Ok(json!(reveal_in_file_manager(a.as_str().unwrap_or("")))),
        "watch" => {
            let paths = a.as_array().map(|v| v.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
            st.watch(paths)?;
            Ok(Value::Null)
        }
        "fileOpen" => st.file_open(a["dir"].as_str().ok_or("missing dir")?, a["name"].as_str().unwrap_or("recording")),
        "fileClose" => {
            st.file_close(a.as_u64().unwrap_or(0));
            Ok(Value::Null)
        }
        "prefsGet" => Ok(st.prefs_get()),
        "prefsSet" => st.prefs_set(&a),
        "openLog" => {
            let p = st.log_path();
            if !p.exists() { let _ = fs::write(&p, ""); }
            let _ = open::that(p);
            Ok(Value::Null)
        }
        "getCrash" => Ok(json!({"crashed": st.crashed})),
        "diag" => {
            st.log(&format!("{} {}", a["name"].as_str().unwrap_or("diag"), a["data"]));
            Ok(Value::Null)
        }
        "getOpen" => Ok(json!(st.take_open_paths())),
        _ => Err(format!("unknown method {method}")),
    }
}
