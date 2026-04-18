#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

#[cfg(target_os = "windows")]
use std::process::Command;
#[cfg(target_os = "windows")]
use std::process::Stdio;
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, TrayIconBuilder, TrayIconEvent},
    Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AppConfig {
    hotkey: String,
    blur_radius: u32,
    spotlight_radius: u32,
    mode: String,
    auto_start: bool,
    overlay_opacity: f64,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            hotkey: "ctrl+alt+p".to_string(),
            blur_radius: 25,
            spotlight_radius: 150,
            mode: "spotlight".to_string(),
            auto_start: false,
            overlay_opacity: 0.88,
        }
    }
}

struct AppState {
    config: Mutex<AppConfig>,
    overlay_visible: Mutex<bool>,
    config_path: Mutex<PathBuf>,
    shortcut_pressed: Mutex<bool>,
}

static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

fn init_log_path() -> PathBuf {
    LOG_PATH
        .get_or_init(|| {
            let log_dir = dirs::config_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("own-view");
            fs::create_dir_all(&log_dir).ok();
            log_dir.join("own-view.log")
        })
        .clone()
}

fn log_line(message: &str) {
    eprintln!("{}", message);
    let path = init_log_path();
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{}", message);
    }
}

fn emit_overlay_visibility(app: &tauri::AppHandle, visible: bool) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.emit("overlay-visibility-changed", visible);
    }
}

fn ensure_main_window(app: &tauri::AppHandle) {
    if app.get_webview_window("main").is_some() {
        return;
    }

    if let Ok(window) = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
        .title("Own View - Settings")
        .inner_size(500.0, 600.0)
        .resizable(false)
        .center()
        .build()
    {
        let app_handle = app.clone();
        window.on_window_event(move |event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                if let Some(main) = app_handle.get_webview_window("main") {
                    let _ = main.hide();
                }
            }
        });
    }
}

fn focus_main_window(app: &tauri::AppHandle) {
    ensure_main_window(app);
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn attach_main_window_behavior(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let app_handle = app.clone();
        window.on_window_event(move |event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                if let Some(main) = app_handle.get_webview_window("main") {
                    let _ = main.hide();
                }
            }
        });
    }
}

fn js_string(input: &str) -> String {
    serde_json::to_string(input).unwrap_or_else(|_| "\"\"".to_string())
}

fn push_overlay_config(app: &tauri::AppHandle, config: &AppConfig) {
    if let Some(window) = app.get_webview_window("overlay") {
        let script = format!(
            "window.__OWN_VIEW_setConfig({mode}, {radius}, {opacity}, {hotkey});",
            mode = js_string(&config.mode),
            radius = config.spotlight_radius,
            opacity = config.overlay_opacity,
            hotkey = js_string(&config.hotkey),
        );
        let _ = window.eval(&script);
    }
}

fn push_overlay_cursor(app: &tauri::AppHandle, x: i32, y: i32) {
    if let Some(window) = app.get_webview_window("overlay") {
        let script = format!("window.__OWN_VIEW_setCursor({x}, {y});");
        let _ = window.eval(&script);
    }
}

fn sync_auto_start(config: &AppConfig) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        fn run_reg(args: &[&str]) -> Result<std::process::ExitStatus, String> {
            Command::new("reg.exe")
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map_err(|e| format!("reg.exe failed: {e}"))
        }

        let exe_path = std::env::current_exe().map_err(|e| e.to_string())?;
        let exe = exe_path.to_string_lossy().replace('\'', "''");
        if config.auto_start {
            let status = run_reg(&[
                    "add",
                    r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                    "/v",
                    "OwnView",
                    "/t",
                    "REG_SZ",
                    "/d",
                    &exe,
                    "/f",
                ])?;

            if status.success() {
                return Ok(());
            }

            return Err(format!("reg.exe exited with status {status}"));
        }

        let query_status = run_reg(&[
                "query",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                "/v",
                "OwnView",
            ])?;

        if !query_status.success() {
            return Ok(());
        }

        let status = run_reg(&[
                "delete",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                "/v",
                "OwnView",
                "/f",
            ])?;

        if status.success() {
            return Ok(());
        }

        return Err(format!("reg.exe exited with status {status}"));
    }

    #[cfg(target_os = "linux")]
    {
        let config_dir = dirs::config_dir()
            .ok_or_else(|| "config directory unavailable".to_string())?;
        let autostart_dir = config_dir.join("autostart");
        fs::create_dir_all(&autostart_dir).map_err(|e| e.to_string())?;
        let desktop_file = autostart_dir.join("own-view.desktop");

        if config.auto_start {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let entry = format!(
                "[Desktop Entry]\nType=Application\nVersion=1.0\nName=Own View\nExec={}\nX-GNOME-Autostart-enabled=true\n",
                exe.to_string_lossy()
            );
            fs::write(&desktop_file, entry).map_err(|e| e.to_string())?;
        } else if desktop_file.exists() {
            fs::remove_file(&desktop_file).map_err(|e| e.to_string())?;
        }

        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        return Ok(());
    }

    #[allow(unreachable_code)]
    Ok(())
}

#[tauri::command]
fn get_config(state: State<AppState>) -> AppConfig {
    state.config.lock().unwrap().clone()
}

#[tauri::command]
fn update_config(app: tauri::AppHandle, state: State<AppState>, config: AppConfig) {
    let mut cfg = state.config.lock().unwrap();
    *cfg = config.clone();
    let path = state.config_path.lock().unwrap().clone();
    if let Ok(json) = serde_json::to_string_pretty(&config) {
        let _ = fs::write(&path, json);
    }
    if let Err(err) = sync_auto_start(&config) {
        log_line(&format!("[WARN] auto start sync failed: {err}"));
    }
    push_overlay_config(&app, &config);
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.emit("config-updated", config);
    }
}

#[tauri::command]
fn toggle_overlay(app: tauri::AppHandle, state: State<AppState>) {
    let mut visible = state.overlay_visible.lock().unwrap();
    *visible = !*visible;
    let vis = *visible;
    drop(visible);
    log_line(&format!("[DEBUG] toggle_overlay called, visible={}", vis));
    if let Some(window) = app.get_webview_window("overlay") {
        if vis {
            let _ = window.show();
            let _ = window.set_always_on_top(true);
        } else {
            let _ = window.hide();
        }
    }
    emit_overlay_visibility(&app, vis);
}

#[tauri::command]
fn is_overlay_visible(state: State<AppState>) -> bool {
    *state.overlay_visible.lock().unwrap()
}

#[tauri::command]
fn set_mode(app: tauri::AppHandle, state: State<AppState>, mode: String) {
    let next_config = {
        let mut cfg = state.config.lock().unwrap();
        cfg.mode = mode.clone();
        let next = cfg.clone();
        let path = state.config_path.lock().unwrap().clone();
        if let Ok(json) = serde_json::to_string_pretty(&*cfg) {
            let _ = fs::write(&path, json);
        }
        next
    };
    push_overlay_config(&app, &next_config);
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.emit("config-updated", next_config);
    }
}

#[tauri::command]
fn get_cursor_position() -> Result<(i32, i32), String> {
    #[cfg(target_os = "linux")]
    {
        use std::process::Command;
        let output = Command::new("xdotool")
            .args(["getmouselocation", "--shell"])
            .output()
            .map_err(|e| format!("xdotool error: {}", e))?;
        let stdout = String::from_utf8(output.stdout).map_err(|e| e.to_string())?;
        let mut x: i32 = 0;
        let mut y: i32 = 0;
        for line in stdout.lines() {
            if let Some(val) = line.strip_prefix("X=") {
                x = val.parse::<i32>().map_err(|e| e.to_string())?;
            }
            if let Some(val) = line.strip_prefix("Y=") {
                y = val.parse::<i32>().map_err(|e| e.to_string())?;
            }
        }
        return Ok((x, y));
    }
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::Foundation::POINT;
        use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
        unsafe {
            let mut point: POINT = std::mem::zeroed();
            if GetCursorPos(&mut point) != 0 {
                return Ok((point.x, point.y));
            }
        }
        return Ok((0, 0));
    }
    #[cfg(target_os = "macos")]
    {
        use cocoa::appkit::NSEvent;
        use cocoa::base::{id, nil};
        use cocoa::foundation::NSPoint;
        use objc::msg_send;
        unsafe {
            let event: id = NSEvent::mouseLocation(nil);
            let location: NSPoint = msg_send![event, location];
            return Ok((location.x as i32, location.y as i32));
        }
    }
    #[allow(unreachable_code)]
    Ok((0, 0))
}

#[tauri::command]
fn get_screen_size(app: tauri::AppHandle) -> (u32, u32) {
    if let Some(monitor) = app.primary_monitor().ok().flatten() {
        (monitor.size().width, monitor.size().height)
    } else {
        (1920, 1080)
    }
}

fn load_config() -> (AppConfig, PathBuf) {
    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("own-view");
    fs::create_dir_all(&config_dir).ok();
    let config_path = config_dir.join("config.json");
    let config = if config_path.exists() {
        if let Ok(content) = fs::read_to_string(&config_path) {
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            AppConfig::default()
        }
    } else {
        let default = AppConfig::default();
        if let Ok(json) = serde_json::to_string_pretty(&default) {
            let _ = fs::write(&config_path, json);
        }
        default
    };
    (config, config_path)
}

fn generate_overlay_html(config: &AppConfig) -> String {
    let opacity = config.overlay_opacity;
    let spotlight = config.spotlight_radius;
    let hotkey = &config.hotkey;
    let mode = &config.mode;
    format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<style>
* {{ margin: 0; padding: 0; box-sizing: border-box; }}
html, body {{ width: 100vw; height: 100vh; overflow: hidden; background: transparent; cursor: none; }}
#veil {{
  position: fixed;
  inset: 0;
  background: rgba(0, 0, 0, {opacity});
  pointer-events: none;
}}
#cutout {{
  position: fixed;
  left: 50%;
  top: 50%;
  width: {spotlight}px;
  height: {spotlight}px;
  transform: translate(-50%, -50%);
  background: transparent;
  border-radius: 9999px;
  box-shadow: 0 0 0 99999px rgba(0, 0, 0, {opacity});
  outline: 2px solid rgba(255,255,255,0.18);
  pointer-events: none;
}}
.hint {{ position: fixed; bottom: 24px; left: 50%; transform: translateX(-50%);
  color: rgba(255,255,255,0.56); font-family: sans-serif; font-size: 13px; pointer-events: none; user-select: none;
  text-shadow: 0 1px 10px rgba(0,0,0,0.85); }}
</style>
</head>
<body>
<div id="veil"></div>
<div id="cutout"></div>
<div class="hint" id="hint">Press {hotkey} to dismiss</div>
<script>
const hint = document.getElementById('hint');
const veil = document.getElementById('veil');
const cutout = document.getElementById('cutout');
const scale = window.devicePixelRatio || 1;
let mouseX = window.innerWidth / 2;
let mouseY = window.innerHeight / 2;
let mode = '{mode}';
let spotlight = {spotlight};
let opacity = {opacity};
function resize() {{ draw(); }}
function draw() {{
  if (mode === 'full') {{
    veil.style.display = 'block';
    const fullOpacity = opacity >= 0.99 ? 1 : opacity;
    veil.style.background = 'rgba(0,0,0,' + fullOpacity + ')';
    cutout.style.display = 'none';
    return;
  }}
  veil.style.display = 'none';
  cutout.style.display = 'block';
  cutout.style.boxShadow = '0 0 0 99999px rgba(0,0,0,' + opacity + ')';
  if (mode === 'window') {{
    const width = Math.max(180, spotlight * 2.4);
    const height = Math.max(120, spotlight * 1.5);
    cutout.style.width = width + 'px';
    cutout.style.height = height + 'px';
    cutout.style.left = (mouseX / scale) + 'px';
    cutout.style.top = (mouseY / scale) + 'px';
    cutout.style.transform = 'translate(-50%, -50%)';
    cutout.style.borderRadius = '24px';
    return;
  }}
  cutout.style.width = (spotlight * 2) + 'px';
  cutout.style.height = (spotlight * 2) + 'px';
  cutout.style.left = (mouseX / scale) + 'px';
  cutout.style.top = (mouseY / scale) + 'px';
  cutout.style.transform = 'translate(-50%, -50%)';
  cutout.style.borderRadius = '9999px';
}}
window.__OWN_VIEW_setCursor = (x, y) => {{
  mouseX = x;
  mouseY = y;
  if (mode !== 'full') draw();
}};
window.__OWN_VIEW_setConfig = (nextMode, nextSpotlight, nextOpacity, nextHotkey) => {{
  mode = nextMode;
  spotlight = nextSpotlight;
  opacity = nextOpacity;
  hint.textContent = `Press ${{nextHotkey}} to dismiss`;
  draw();
}};
window.addEventListener('resize', resize);
resize();
draw();
</script>
</body>
</html>"#
    )
}

fn base64_encode(input: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity((input.len() + 2) / 3 * 4);
    let mut chunks = input.chunks(3);
    for chunk in chunks.by_ref() {
        match chunk.len() {
            3 => {
                let b0 = chunk[0] as u32;
                let b1 = chunk[1] as u32;
                let b2 = chunk[2] as u32;
                let triple = (b0 << 16) | (b1 << 8) | b2;
                output.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
                output.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
                output.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
                output.push(CHARS[(triple & 0x3F) as usize] as char);
            }
            2 => {
                let b0 = chunk[0] as u32;
                let b1 = chunk[1] as u32;
                let triple = (b0 << 16) | (b1 << 8);
                output.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
                output.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
                output.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
                output.push('=');
            }
            1 => {
                let b0 = chunk[0] as u32;
                let triple = b0 << 16;
                output.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
                output.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
                output.push_str("==");
            }
            _ => {}
        }
    }
    output
}

fn make_data_url(html: &str) -> WebviewUrl {
    let encoded = base64_encode(html.as_bytes());
    let data_url = format!("data:text/html;base64,{}", encoded);
    WebviewUrl::External(data_url.parse().unwrap())
}

fn main() {
    std::panic::set_hook(Box::new(|info| {
        let msg = format!("PANIC: {}", info);
        log_line(&msg);
    }));

    let log_path = init_log_path();
    let _ = fs::write(&log_path, "");
    log_line("[1] Starting");
    let (config, config_path) = load_config();
    log_line(&format!("[2] Config loaded: mode={}", config.mode));

    let overlay_html = generate_overlay_html(&config);
    let overlay_url = make_data_url(&overlay_html);

    log_line("[3] HTML generated");

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            focus_main_window(app);
        }))
        .plugin(tauri_plugin_shell::init())
        .manage(AppState {
            config: Mutex::new(config.clone()),
            overlay_visible: Mutex::new(false),
            config_path: Mutex::new(config_path.clone()),
            shortcut_pressed: Mutex::new(false),
        })
        .setup(move |app| {
            log_line("[4] Setup starting");

            let app_handle = app.handle().clone();
            attach_main_window_behavior(&app_handle);

            // Create overlay window first
            log_line("[5] Creating overlay window...");
            let overlay = WebviewWindowBuilder::new(app, "overlay", overlay_url)
                .title("Own View")
                .fullscreen(true)
                .always_on_top(true)
                .transparent(true)
                .decorations(false)
                .resizable(false)
                .visible(false)
                .skip_taskbar(true)
                .focusable(false)
                .visible_on_all_workspaces(true)
                .build()?;
            let _ = overlay.set_ignore_cursor_events(true);
            let _ = overlay.set_shadow(false);
            let _ = overlay.set_always_on_top(true);
            let _ = overlay.set_visible_on_all_workspaces(true);
            push_overlay_config(&app.handle().clone(), &config);
            log_line("[6] Overlay window done");

            // System tray
            log_line("[7] Creating tray...");
            let open_i =
                MenuItem::with_id(app, "open", "Open Settings", true, None::<&str>)?;
            let toggle_i =
                MenuItem::with_id(app, "toggle", "Toggle (Ctrl+Alt+P)", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open_i, &toggle_i, &quit_i])?;
            let _tray = TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("Own View")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_tray_icon_event(|tray, event| match event {
                    TrayIconEvent::Click { button: MouseButton::Left, .. }
                    | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. } => {
                        focus_main_window(&tray.app_handle());
                    }
                    _ => {}
                })
                .on_menu_event(|app, event| match event.id().0.as_str() {
                    "open" => focus_main_window(&app),
                    "toggle" => {
                        let state = app.state::<AppState>();
                        toggle_overlay(app.clone(), state);
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;
            log_line("[8] Tray done");

            // Global shortcut
            let hotkey_str = app
                .state::<AppState>()
                .config
                .lock()
                .unwrap()
                .hotkey
                .clone();
            log_line(&format!("[9] Registering shortcut: {}", hotkey_str));
            if let Ok(shortcut) = hotkey_str.parse::<tauri_plugin_global_shortcut::Shortcut>() {
                use tauri_plugin_global_shortcut::{
                    GlobalShortcutExt, ShortcutEvent, ShortcutState,
                };
                let _ = app.handle().plugin(
                    tauri_plugin_global_shortcut::Builder::new()
                        .with_handler(
                            move |app: &tauri::AppHandle, _s: &_, event: ShortcutEvent| {
                                let state = app.state::<AppState>();
                                if event.state == ShortcutState::Pressed {
                                    let mut pressed = state.shortcut_pressed.lock().unwrap();
                                    if *pressed {
                                        return;
                                    }
                                    *pressed = true;
                                    drop(pressed);
                                    toggle_overlay(app.clone(), state);
                                } else if event.state == ShortcutState::Released {
                                    let mut pressed = state.shortcut_pressed.lock().unwrap();
                                    *pressed = false;
                                }
                            },
                        )
                        .build(),
                );
                match app.global_shortcut().register(shortcut.clone()) {
                    Ok(_) => log_line("[10] Shortcut registered"),
                    Err(e) => log_line(&format!("[10] Shortcut error: {}", e)),
                }
            }

            // Cursor tracking
            let handle = app_handle.clone();
            std::thread::spawn(move || {
                use std::time::Duration;
                loop {
                    if let Ok((x, y)) = get_cursor_position() {
                        push_overlay_cursor(&handle, x, y);
                    }
                    std::thread::sleep(Duration::from_millis(16));
                }
            });
            log_line("[11] Cursor thread started");
            log_line("[12] Setup complete, entering main loop");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_config,
            update_config,
            toggle_overlay,
            is_overlay_visible,
            set_mode,
            get_cursor_position,
            get_screen_size,
        ])
        .run(tauri::generate_context!())
        .expect("error running tauri");
}
