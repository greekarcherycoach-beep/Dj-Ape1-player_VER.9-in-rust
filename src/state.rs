//! Shared application state: preferences, library persistence/backups,
//! recorder file handles, folder watching and diagnostics.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::UserEvent;
use tao::event_loop::EventLoopProxy;

pub const AUDIO_EXT: &[&str] = &[
    "mp3", "flac", "m4a", "mp4", "aac", "ogg", "oga", "opus", "wav", "wma", "aif", "aiff", "ape",
];
const MAX_BACKUPS: usize = 15;

pub fn is_audio(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO_EXT.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Prefs {
    #[serde(default = "yes")]
    pub close_to_tray: bool,
    #[serde(default)]
    pub login: bool,
}
fn yes() -> bool {
    true
}
impl Default for Prefs {
    fn default() -> Self {
        Prefs { close_to_tray: true, login: false }
    }
}

#[derive(Deserialize)]
struct Check {
    tracks: Vec<serde::de::IgnoredAny>,
}

pub struct AppState {
    pub token: String,
    pub data_dir: PathBuf,
    pub portable: bool,
    pub crashed: bool,
    pub prefs: Mutex<Prefs>,
    pub open_paths: Mutex<Vec<String>>,
    writers: Mutex<HashMap<u64, File>>,
    next_writer: AtomicU64,
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
    proxy: Mutex<EventLoopProxy<UserEvent>>,
    lib_lock: Mutex<()>,
}

pub fn mtime_ms(m: &fs::Metadata) -> u64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl AppState {
    pub fn new(
        token: String,
        data_dir: PathBuf,
        portable: bool,
        crashed: bool,
        open_paths: Vec<String>,
        proxy: EventLoopProxy<UserEvent>,
    ) -> Self {
        let prefs = fs::read(data_dir.join("prefs.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Prefs>(&b).ok())
            .unwrap_or_default();
        AppState {
            token,
            data_dir,
            portable,
            crashed,
            prefs: Mutex::new(prefs),
            open_paths: Mutex::new(open_paths),
            writers: Mutex::new(HashMap::new()),
            next_writer: AtomicU64::new(1),
            watcher: Mutex::new(None),
            proxy: Mutex::new(proxy),
            lib_lock: Mutex::new(()),
        }
    }

    fn lib_path(&self) -> PathBuf {
        self.data_dir.join("library.json")
    }
    fn bk_dir(&self) -> PathBuf {
        self.data_dir.join("backups")
    }
    pub fn log_path(&self) -> PathBuf {
        self.data_dir.join("djape1.log")
    }

    // ---------- diagnostics ----------
    pub fn log(&self, line: &str) {
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(self.log_path()) {
            let _ = writeln!(f, "[{ts}] {line}");
        }
    }

    // ---------- preferences ----------
    pub fn prefs_get(&self) -> Value {
        let p = self.prefs.lock().unwrap().clone();
        json!({"closeToTray": p.close_to_tray, "login": p.login, "portable": self.portable})
    }
    pub fn prefs_set(&self, patch: &Value) -> Result<Value, String> {
        let mut p = self.prefs.lock().unwrap();
        if let Some(b) = patch.get("closeToTray").and_then(Value::as_bool) {
            p.close_to_tray = b;
        }
        if let Some(b) = patch.get("login").and_then(Value::as_bool) {
            if !self.portable {
                p.login = b;
                set_autostart(b);
            }
        }
        let s = serde_json::to_vec_pretty(&*p).map_err(|e| e.to_string())?;
        fs::write(self.data_dir.join("prefs.json"), s).map_err(|e| e.to_string())?;
        Ok(Value::Null)
    }

    // ---------- library persistence ----------
    fn count_of(bytes: &[u8]) -> Option<usize> {
        serde_json::from_slice::<Check>(bytes).ok().map(|c| c.tracks.len())
    }

    fn make_backup(&self, bytes: &[u8], count: usize) -> Result<(), String> {
        let dir = self.bk_dir();
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        fs::write(dir.join(format!("library-{ts}-{count}.json")), bytes).map_err(|e| e.to_string())?;
        let mut all = self.list_backups();
        while all.len() > MAX_BACKUPS {
            if let Some(old) = all.pop() {
                let _ = fs::remove_file(dir.join(old.0));
            }
        }
        Ok(())
    }

    /// (name, size, count) newest first
    fn list_backups(&self) -> Vec<(String, u64, Option<usize>)> {
        let mut v: Vec<(String, u64, Option<usize>, u64)> = vec![];
        if let Ok(rd) = fs::read_dir(self.bk_dir()) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if !(name.starts_with("library-") && name.ends_with(".json")) {
                    continue;
                }
                let core = &name[8..name.len() - 5];
                let mut it = core.split('-');
                let ts = it.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
                let cnt = it.next().and_then(|s| s.parse::<usize>().ok());
                let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                v.push((name, size, cnt, ts));
            }
        }
        v.sort_by(|a, b| b.3.cmp(&a.3));
        v.into_iter().map(|(n, s, c, _)| (n, s, c)).collect()
    }

    pub fn lib_save(&self, body: &[u8]) -> Result<(), String> {
        let _g = self.lib_lock.lock().unwrap();
        let new_count = Self::count_of(body).ok_or("το JSON δεν περιέχει λίστα τραγουδιών")?;
        let path = self.lib_path();
        if let Ok(old) = fs::read(&path) {
            let old_count = Self::count_of(&old).unwrap_or(0);
            let newest = self.list_backups().first().map(|b| b.0.clone());
            let age_ok = newest
                .as_deref()
                .and_then(|n| n[8..n.len() - 5].split('-').next()?.parse::<u64>().ok())
                .map(|ts| {
                    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0).saturating_sub(ts)
                        < 6 * 3600
                })
                .unwrap_or(false);
            let shrinking = old_count > 20 && (new_count as f64) < old_count as f64 * 0.9;
            if old_count > 0 && (!age_ok || shrinking) {
                let _ = self.make_backup(&old, old_count);
            }
        }
        let tmp = self.data_dir.join("library.json.tmp");
        {
            let mut f = File::create(&tmp).map_err(|e| e.to_string())?;
            f.write_all(body).map_err(|e| e.to_string())?;
            f.sync_all().map_err(|e| e.to_string())?;
        }
        fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
        let _ = fs::write(self.data_dir.join("library.count"), new_count.to_string());
        Ok(())
    }

    /// Returns (json bytes, name of backup it came from, empty if main file)
    pub fn lib_load(&self) -> Option<(Vec<u8>, String)> {
        let _g = self.lib_lock.lock().unwrap();
        if let Ok(b) = fs::read(self.lib_path()) {
            if Self::count_of(&b).is_some() {
                return Some((b, String::new()));
            }
            self.log("library.json is corrupt – trying backups");
        }
        for (name, _, _) in self.list_backups() {
            if let Ok(b) = fs::read(self.bk_dir().join(&name)) {
                if Self::count_of(&b).is_some() {
                    return Some((b, name));
                }
            }
        }
        None
    }

    pub fn lib_info(&self) -> Value {
        let p = self.lib_path();
        if !p.exists() {
            return json!({"exists": false, "count": 0});
        }
        let count = fs::read_to_string(self.data_dir.join("library.count"))
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .or_else(|| fs::read(&p).ok().and_then(|b| Self::count_of(&b)))
            .unwrap_or(0);
        json!({"exists": true, "count": count})
    }

    pub fn lib_backups(&self) -> Value {
        Value::Array(
            self.list_backups()
                .into_iter()
                .map(|(n, s, c)| json!({"name": n, "size": s, "count": c}))
                .collect(),
        )
    }

    pub fn lib_backup_read(&self, name: &str) -> Result<Vec<u8>, String> {
        if name.contains('/') || name.contains('\\') || name.contains("..") {
            return Err("invalid name".into());
        }
        fs::read(self.bk_dir().join(name)).map_err(|e| e.to_string())
    }

    pub fn lib_backup_now(&self) -> Result<Value, String> {
        let _g = self.lib_lock.lock().unwrap();
        if let Ok(b) = fs::read(self.lib_path()) {
            let c = Self::count_of(&b).unwrap_or(0);
            if c > 0 {
                self.make_backup(&b, c)?;
            }
        }
        Ok(Value::Null)
    }

    // ---------- recorder files ----------
    pub fn file_open(&self, dir: &str, name: &str) -> Result<Value, String> {
        let clean: String = name.chars().filter(|c| !matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')).collect();
        let clean = if clean.trim().is_empty() { "recording".to_string() } else { clean };
        let p = Path::new(&clean);
        let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("recording").to_string();
        let ext = p.extension().and_then(|s| s.to_str()).map(|e| format!(".{e}")).unwrap_or_default();
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        for n in 0..1000 {
            let fname = if n == 0 { format!("{stem}{ext}") } else { format!("{stem} ({}){ext}", n + 1) };
            match OpenOptions::new().write(true).create_new(true).open(Path::new(dir).join(&fname)) {
                Ok(f) => {
                    let id = self.next_writer.fetch_add(1, Ordering::SeqCst);
                    self.writers.lock().unwrap().insert(id, f);
                    return Ok(json!({"id": id, "name": fname}));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.to_string()),
            }
        }
        Err("δεν βρέθηκε ελεύθερο όνομα αρχείου".into())
    }

    pub fn file_write(&self, id: u64, pos: Option<u64>, data: &[u8]) -> Result<(), String> {
        let mut w = self.writers.lock().unwrap();
        let f = w.get_mut(&id).ok_or("άγνωστο αρχείο")?;
        match pos {
            Some(p) => {
                let end = f.stream_position().map_err(|e| e.to_string())?;
                f.seek(SeekFrom::Start(p)).map_err(|e| e.to_string())?;
                f.write_all(data).map_err(|e| e.to_string())?;
                // positional writes (e.g. FLAC header rewrite) must not move the append cursor
                f.seek(SeekFrom::Start(end.max(p + data.len() as u64))).map_err(|e| e.to_string())?;
            }
            None => {
                f.write_all(data).map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    pub fn file_close(&self, id: u64) {
        if let Some(mut f) = self.writers.lock().unwrap().remove(&id) {
            let _ = f.flush();
            let _ = f.sync_all();
        }
    }

    // ---------- folder watching ----------
    pub fn watch(&self, paths: Vec<String>) -> Result<(), String> {
        use notify::{RecursiveMode, Watcher};
        if paths.is_empty() {
            *self.watcher.lock().unwrap() = None;
            return Ok(());
        }
        let proxy = self.proxy.lock().unwrap().clone();
        let last = Arc::new(Mutex::new(std::time::Instant::now() - Duration::from_secs(60)));
        let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                if ev.paths.iter().any(|p| is_audio(p)) {
                    let mut l = last.lock().unwrap();
                    if l.elapsed() > Duration::from_secs(3) {
                        *l = std::time::Instant::now();
                        let _ = proxy.send_event(UserEvent::Changed);
                    }
                }
            }
        })
        .map_err(|e| e.to_string())?;
        for p in paths {
            let _ = w.watch(Path::new(&p), RecursiveMode::Recursive);
        }
        *self.watcher.lock().unwrap() = Some(w);
        Ok(())
    }

    pub fn take_open_paths(&self) -> Vec<String> {
        std::mem::take(&mut *self.open_paths.lock().unwrap())
    }
}

pub fn set_autostart(on: bool) {
    let Ok(exe) = std::env::current_exe() else { return };
    let Some(exe) = exe.to_str() else { return };
    if let Ok(al) = auto_launch::AutoLaunchBuilder::new().set_app_name("DjApe1").set_app_path(exe).build() {
        let _ = if on { al.enable() } else { al.disable() };
    }
}

pub fn reveal_in_file_manager(path: &str) -> bool {
    let p = Path::new(path);
    if !p.exists() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        return std::process::Command::new("explorer")
            .raw_arg(format!("/select,\"{}\"", path))
            .spawn()
            .is_ok();
    }
    #[cfg(target_os = "macos")]
    {
        return std::process::Command::new("open").arg("-R").arg(path).spawn().is_ok();
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        let dir = if p.is_dir() { p } else { p.parent().unwrap_or(p) };
        return open::that(dir).is_ok();
    }
}
