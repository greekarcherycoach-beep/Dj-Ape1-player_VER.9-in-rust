#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

mod server;
mod state;
mod tags;

use rand::distr::{Alphanumeric, SampleString};
use serde_json::{json, Value};
use state::{is_audio, mtime_ms, AppState};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tao::dpi::LogicalSize;
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::window::{Fullscreen, Icon, WindowBuilder};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder, TrayIconEvent};
use wry::{DragDropEvent, WebContext, WebViewBuilder};

#[derive(Debug)]
pub enum UserEvent {
    Ipc(String),
    Eval(String),
    Drop { paths: Vec<String>, x: f64, y: f64 },
    Menu(String),
    TrayShow,
    Changed,
    Exit,
}

const ICON_RGBA: &[u8] = include_bytes!("../assets/icon64.rgba");
const HTML: &str = include_str!("../assets/index.html");
const BRIDGE: &str = include_str!("../assets/bridge.js");

fn data_dir() -> (PathBuf, bool) {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if dir.join("portable").exists() || dir.join("portable.txt").exists() {
                return (dir.join("data"), true);
            }
        }
    }
    let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    (base.join("DjApe1"), false)
}

fn walk_audio(root: &Path, rel: &str, out: &mut Vec<Value>) {
    let Ok(rd) = std::fs::read_dir(root) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let Ok(m) = std::fs::metadata(&p) else { continue };
        let r = format!("{rel}/{name}");
        if m.is_dir() {
            walk_audio(&p, &r, out);
        } else if is_audio(&p) {
            out.push(json!({"path": p.to_string_lossy(), "name": name, "size": m.len(), "mtime": mtime_ms(&m), "rel": r}));
        }
    }
}

fn resolve(proxy: &EventLoopProxy<UserEvent>, id: u64, ok: bool, v: Value) {
    if id != 0 {
        let _ = proxy.send_event(UserEvent::Eval(format!("window.__dj.res({id},{ok},{v})")));
    }
}

fn pick_input(window: &tao::window::Window, a: &Value, id: u64, proxy: EventLoopProxy<UserEvent>) {
    let dlg = rfd::FileDialog::new().set_parent(window);
    if a["dir"].as_bool() == Some(true) {
        match dlg.pick_folder() {
            None => resolve(&proxy, id, true, json!([])),
            Some(root) => {
                std::thread::spawn(move || {
                    let name = root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    let mut out = vec![];
                    walk_audio(&root, &name, &mut out);
                    resolve(&proxy, id, true, Value::Array(out));
                });
            }
        }
        return;
    }
    let dlg = if a["json"].as_bool() == Some(true) {
        dlg.add_filter("JSON", &["json"])
    } else {
        dlg.add_filter("Audio", state::AUDIO_EXT)
    };
    let picked: Vec<PathBuf> = if a["multi"].as_bool() == Some(true) {
        dlg.pick_files().unwrap_or_default()
    } else {
        dlg.pick_file().into_iter().collect()
    };
    let out: Vec<Value> = picked
        .iter()
        .filter_map(|p| {
            let m = std::fs::metadata(p).ok()?;
            Some(json!({"path": p.to_string_lossy(), "name": p.file_name()?.to_string_lossy(), "size": m.len(), "mtime": mtime_ms(&m)}))
        })
        .collect();
    resolve(&proxy, id, true, Value::Array(out));
}

fn make_tray(proxy: &EventLoopProxy<UserEvent>) -> Option<(TrayIcon, MenuItem)> {
    let menu = Menu::new();
    let show = MenuItem::with_id("show", "Εμφάνιση", true, None);
    let toggle = MenuItem::with_id("toggle", "Αναπαραγωγή / Παύση", true, None);
    let next = MenuItem::with_id("next", "Επόμενο", true, None);
    let prev = MenuItem::with_id("prev", "Προηγούμενο", true, None);
    let stop = MenuItem::with_id("stop", "Στοπ", true, None);
    let mini = MenuItem::with_id("mini", "Μίνι player", true, None);
    let quit = MenuItem::with_id("quit", "Έξοδος", true, None);
    menu.append_items(&[
        &show, &PredefinedMenuItem::separator(), &toggle, &next, &prev, &stop, &mini,
        &PredefinedMenuItem::separator(), &quit,
    ])
    .ok()?;
    let p1 = proxy.clone();
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        let _ = p1.send_event(UserEvent::Menu(e.id.0.clone()));
    }));
    let p2 = proxy.clone();
    TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
        if let TrayIconEvent::DoubleClick { .. } = e {
            let _ = p2.send_event(UserEvent::TrayShow);
        }
    }));
    let icon = tray_icon::Icon::from_rgba(ICON_RGBA.to_vec(), 64, 64).ok()?;
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Dj Ape1")
        .with_icon(icon)
        .build()
        .ok()?;
    Some((tray, toggle))
}

fn init_script(token: &str) -> String {
    #[allow(unused_mut)]
    let mut js = BRIDGE.replace("__TOKEN__", token);
    #[cfg(debug_assertions)]
    if let Ok(p) = std::env::var("DJAPE_INIT_JS") {
        js.push_str(&std::fs::read_to_string(p).unwrap_or_default()); // debug-only self-test hook
    }
    js
}

fn main() {
    let (dir, portable) = data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let lock = dir.join("running.lock");
    let crashed = lock.exists();
    let _ = std::fs::write(&lock, b"1");

    let open_paths: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| Path::new(a).exists())
        .collect();

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let mut token = Alphanumeric.sample_string(&mut rand::rng(), 32);
    #[cfg(debug_assertions)]
    if let Ok(t) = std::env::var("DJAPE_TOKEN") {
        token = t; // fixed token for local testing (debug builds only)
    }
    let st = Arc::new(AppState::new(token, dir.clone(), portable, crashed, open_paths, proxy.clone()));
    if crashed {
        st.log("previous run did not exit cleanly");
    }
    let port = server::start(st.clone(), HTML.to_string());

    let icon = Icon::from_rgba(ICON_RGBA.to_vec(), 64, 64).ok();
    let window = WindowBuilder::new()
        .with_title("Dj Ape1")
        .with_inner_size(LogicalSize::new(1280.0, 820.0))
        .with_min_inner_size(LogicalSize::new(640.0, 480.0))
        .with_window_icon(icon)
        .build(&event_loop)
        .expect("cannot create window");

    let mut ctx = WebContext::new(Some(dir.join("webview")));
    let (px_ipc, px_drop) = (proxy.clone(), proxy.clone());
    let scale = window.scale_factor();
    #[allow(unused_mut)]
    let mut builder = WebViewBuilder::new_with_web_context(&mut ctx)
        .with_url(format!("http://127.0.0.1:{port}/"))
        .with_initialization_script(init_script(&st.token))
        .with_ipc_handler(move |req| {
            let _ = px_ipc.send_event(UserEvent::Ipc(req.body().clone()));
        })
        .with_drag_drop_handler(move |ev| {
            if let DragDropEvent::Drop { paths, position } = ev {
                let (mut x, mut y) = (position.0 as f64, position.1 as f64);
                if cfg!(windows) {
                    x /= scale;
                    y /= scale;
                }
                let paths = paths.iter().map(|p| p.to_string_lossy().to_string()).collect();
                let _ = px_drop.send_event(UserEvent::Drop { paths, x, y });
            }
            true // we dispatch drops ourselves (real paths), block the webview default
        })
        .with_devtools(cfg!(debug_assertions));

    #[cfg(windows)]
    {
        use wry::WebViewBuilderExtWindows;
        builder = builder.with_additional_browser_args(
            "--autoplay-policy=no-user-gesture-required --disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection",
        );
    }

    #[cfg(target_os = "linux")]
    let webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        let vbox = window.default_vbox().expect("no gtk vbox");
        builder.build_gtk(vbox).expect("cannot create webview")
    };
    #[cfg(not(target_os = "linux"))]
    let webview = builder.build(&window).expect("cannot create webview");

    let mut tray: Option<(TrayIcon, MenuItem)> = None;
    let mut closing = false;

    event_loop.run(move |event, _, flow| {
        *flow = ControlFlow::Wait;
        let eval = |js: &str| {
            let _ = webview.evaluate_script(js);
        };
        let begin_shutdown = |closing: &mut bool| {
            if *closing {
                return;
            }
            *closing = true;
            eval("window.__dj.emit('flush')");
            let p = proxy.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(4));
                let _ = p.send_event(UserEvent::Exit);
            });
        };

        match event {
            Event::NewEvents(StartCause::Init) => {
                tray = make_tray(&proxy);
            }
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                let to_tray = st.prefs.lock().unwrap().close_to_tray && tray.is_some();
                if to_tray {
                    window.set_visible(false);
                } else {
                    begin_shutdown(&mut closing);
                }
            }
            Event::UserEvent(ev) => match ev {
                UserEvent::Eval(js) => eval(&js),
                UserEvent::Changed => eval("window.__dj.emit('changed')"),
                UserEvent::Drop { paths, x, y } => {
                    window.set_visible(true);
                    let p = serde_json::to_string(&paths).unwrap_or_else(|_| "[]".into());
                    eval(&format!("window.__dj.drop({p},{x},{y})"));
                }
                UserEvent::TrayShow => {
                    window.set_visible(true);
                    window.set_focus();
                }
                UserEvent::Menu(id) => match id.as_str() {
                    "show" => {
                        window.set_visible(true);
                        window.set_focus();
                    }
                    "quit" => begin_shutdown(&mut closing),
                    cmd => {
                        if cmd == "mini" {
                            window.set_visible(true);
                        }
                        eval(&format!("window.__dj.emit('cmd',{})", json!(cmd)));
                    }
                },
                UserEvent::Exit => {
                    let _ = std::fs::remove_file(dir.join("running.lock"));
                    tray = None;
                    *flow = ControlFlow::Exit;
                }
                UserEvent::Ipc(msg) => {
                    let v: Value = serde_json::from_str(&msg).unwrap_or(Value::Null);
                    let id = v["id"].as_u64().unwrap_or(0);
                    match v["m"].as_str().unwrap_or("") {
                        "pickFolder" => {
                            let r = rfd::FileDialog::new().set_parent(&window).pick_folder();
                            resolve(&proxy, id, true, json!(r.map(|p| p.to_string_lossy().to_string())));
                        }
                        "pickInput" => pick_input(&window, &v["a"], id, proxy.clone()),
                        "toggleFullscreen" => {
                            let on = window.fullscreen().is_none();
                            window.set_fullscreen(if on { Some(Fullscreen::Borderless(None)) } else { None });
                            eval(&format!("window.__dj.emit('fullscreen',{on})"));
                        }
                        "flushDone" => {
                            if closing {
                                let _ = proxy.send_event(UserEvent::Exit);
                            }
                        }
                        "state" => {
                            if let Some((t, toggle)) = &tray {
                                let a = &v["a"];
                                let playing = a["playing"].as_bool().unwrap_or(false);
                                let title = a["title"].as_str().unwrap_or("");
                                let artist = a["artist"].as_str().unwrap_or("");
                                let tip = if title.is_empty() {
                                    "Dj Ape1".to_string()
                                } else if artist.is_empty() {
                                    format!("{} {title}", if playing { "▶" } else { "⏸" })
                                } else {
                                    format!("{} {artist} – {title}", if playing { "▶" } else { "⏸" })
                                };
                                let _ = t.set_tooltip(Some(tip));
                                toggle.set_text(if playing { "Παύση" } else { "Αναπαραγωγή" });
                            }
                        }
                        _ => {}
                    }
                }
            },
            _ => {}
        }
    });
}
