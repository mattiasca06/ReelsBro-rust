#![windows_subsystem = "windows"]

#[cfg(feature = "debug-hooks")]
mod hooks;

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::net::TcpListener;
use std::process::Command;
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tao::{
    dpi::{LogicalPosition, LogicalSize},
    event::{Event, StartCause, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy},
    window::{Theme, Window, WindowBuilder},
};
use tray_icon::{
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu},
    Icon, TrayIconBuilder,
};
use wry::WebViewBuilder;

#[derive(Deserialize, Debug, Clone)]
struct ToastPayload {
    author: String,
    content: String,
    avatar: String,
    sound_path: Option<String>,
    accent_color: Option<String>,
    #[allow(dead_code)]
    position: Option<String>,
    /// Set by the plugin's test buttons so reel extraction failures are surfaced as a toast.
    #[serde(default)]
    test: bool,
}

#[derive(Debug, Clone)]
struct ReelItem {
    author: String,
    avatar: String,
    original_url: String,
    stream_url: String,
}

#[derive(Debug, Clone)]
enum CustomEvent {
    ShowToast(ToastPayload),
    HideToast(usize),
    ReelReady(ReelItem),
    ReelClose,
    ReelNext,
    SkipReel,
    ClearReelQueue,
    SetToastScale(f64),
    SetReelScale(f64),
    SetPreview(bool),
    SetTheme(String),
    SaveSettings,
    Menu(tray_icon::menu::MenuId),
}

fn create_purple_icon() -> Icon {
    let mut rgba = Vec::with_capacity(32 * 32 * 4);
    for y in 0..32 {
        for x in 0..32 {
            let dx = (x as f32) - 15.5;
            let dy = (y as f32) - 15.5;
            if dx * dx + dy * dy <= 14.0 * 14.0 {
                rgba.extend_from_slice(&[203, 166, 247, 255]); // #cba6f7 Mauve
            } else {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
    Icon::from_rgba(rgba, 32, 32).unwrap()
}

// Base (100%) sizes in logical px. Window size and webview zoom are both derived from these,
// so window and contents always scale together.
const TOAST_BASE: (f64, f64) = (980.0, 300.0);
const REEL_BASE: (f64, f64) = (380.0, 670.0);
const EDGE_MARGIN: f64 = 24.0;
const TOP_MARGIN: f64 = 35.0;

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Settings {
    toast_scale: f64, // percent
    reel_scale: f64,  // percent
    #[serde(default = "default_theme")]
    theme: String, // "system" | "light" | "dark"
}

fn default_theme() -> String {
    "system".to_string()
}

fn theme_from_str(t: &str) -> Option<Theme> {
    match t {
        "light" => Some(Theme::Light),
        "dark" => Some(Theme::Dark),
        _ => None,
    }
}

impl Default for Settings {
    fn default() -> Self {
        Settings { toast_scale: 100.0, reel_scale: 150.0, theme: default_theme() }
    }
}

fn settings_path() -> std::path::PathBuf {
    let dir = std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("rice-dunst-reels");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("settings.json")
}

fn load_settings() -> Settings {
    std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_settings(s: &Settings) {
    if let Ok(t) = serde_json::to_string_pretty(s) {
        let _ = std::fs::write(settings_path(), t);
    }
}

#[derive(Clone, Copy)]
enum Anchor {
    /// Centered on the monitor; if `Some(reel_left)` (physical px), pushed left to stay clear of the reel player.
    TopCenter(Option<i32>),
    TopRight,
}

/// Re-places the toast, pushing it left if the reel player is visible and would overlap it.
fn place_toast(toast_window: &Window, toast_webview: &wry::WebView, reel_window: &Window, percent: f64) {
    let reel_left = if reel_window.is_visible() {
        reel_window.outer_position().ok().map(|p| p.x)
    } else {
        None
    };
    apply_layout(toast_window, toast_webview, TOAST_BASE, percent, Anchor::TopCenter(reel_left));
}

/// Sizes and positions `window` on its monitor in physical pixels (DPI-aware), capping the scale so it
/// always fits on screen, then zooms the webview to match. Returns the effective scale factor applied.
fn apply_layout(window: &Window, webview: &wry::WebView, base: (f64, f64), percent: f64, anchor: Anchor) -> f64 {
    let (mx, my, mw, mh, dpi) = match window.primary_monitor().or_else(|| window.current_monitor()) {
        Some(m) => (
            m.position().x as f64,
            m.position().y as f64,
            m.size().width as f64,
            m.size().height as f64,
            m.scale_factor(),
        ),
        None => (0.0, 0.0, 1920.0, 1080.0, 1.0),
    };
    let margin = EDGE_MARGIN * dpi;
    let top = TOP_MARGIN * dpi;
    let max_by_w = (mw - 2.0 * margin) / (base.0 * dpi);
    let max_by_h = (mh - top - margin) / (base.1 * dpi);
    let gap = 12.0 * dpi;
    // If the reel is in the way, the toast may use only the space to its left (shrinks only when it can't fit at all).
    let max_by_w = match anchor {
        Anchor::TopCenter(Some(reel_left)) => {
            max_by_w.min((reel_left as f64 - gap - mx - margin) / (base.0 * dpi))
        }
        _ => max_by_w,
    };
    let scale = (percent / 100.0).min(max_by_w).min(max_by_h).max(0.1);

    let w = (base.0 * scale * dpi).round();
    let h = (base.1 * scale * dpi).round();
    let x = match anchor {
        Anchor::TopCenter(None) => mx + (mw - w) / 2.0,
        Anchor::TopCenter(Some(reel_left)) => {
            let centered = mx + (mw - w) / 2.0;
            centered.min(reel_left as f64 - gap - w).max(mx + margin)
        }
        Anchor::TopRight => mx + mw - w - margin,
    };
    let (px, py) = (x.round() as i32, (my + top).round() as i32);
    // tao's set_inner_size is silently ignored for these undecorated windows, so go straight to Win32.
    #[cfg(target_os = "windows")]
    unsafe {
        use tao::platform::windows::WindowExtWindows;
        use windows::Win32::Foundation::{HWND, RECT};
        use windows::Win32::UI::WindowsAndMessaging::{
            GetWindowRect, IsWindowVisible, SetWindowPos, ShowWindow, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOZORDER,
        };
        let hwnd = HWND(window.hwnd() as *mut _);
        let mut old = RECT::default();
        let _ = GetWindowRect(hwnd, &mut old);
        let resized = (old.right - old.left, old.bottom - old.top) != (w as i32, h as i32);

        let _ = SetWindowPos(hwnd, HWND::default(), px, py, w as i32, h as i32, SWP_NOZORDER | SWP_NOACTIVATE);

        // DWM keeps compositing the window's old-size surface, leaving a ghost rectangle (different
        // opacity) over the old area. Hiding and re-showing without activating drops that surface.
        if resized && IsWindowVisible(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_HIDE);
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        use tao::dpi::{PhysicalPosition, PhysicalSize};
        window.set_inner_size(PhysicalSize::new(w as u32, h as u32));
        window.set_outer_position(PhysicalPosition::new(px, py));
    }
    let _ = webview.zoom(scale);
    scale
}

fn get_mp4_with_ytdlp(url: &str) -> Result<String, String> {
    let mut cmd = Command::new("yt-dlp");
    cmd.args([
        "-f", "best[ext=mp4]/best", // Ensure video + audio muxed
        "-g",                        // Direct stream URL only
        "--no-warnings",
        "--no-playlist",
        url,
    ]);

    #[cfg(target_os = "windows")]
    cmd.creation_flags(0x08000000); // Hide CMD window

    let output = cmd.output().map_err(|e| {
        format!("Failed to execute yt-dlp: {}", e)
    })?;

    if output.status.success() {
        let stream_url = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let first_url = stream_url.lines().next().unwrap_or("").to_string();
        if first_url.starts_with("http") {
            return Ok(first_url);
        }
    }

    let err_msg = String::from_utf8_lossy(&output.stderr);
    Err(format!("yt-dlp failed: {}", err_msg))
}

fn hidden_command(program: &str) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(target_os = "windows")]
    cmd.creation_flags(0x08000000); // Hide CMD window
    cmd
}

/// Latest yt-dlp release tag (e.g. "2025.09.26") via GitHub, using the curl.exe bundled with Windows.
fn fetch_latest_ytdlp_version() -> Result<String, String> {
    let output = hidden_command("curl")
        .args([
            "-s", "-m", "8", "-H", "User-Agent: rice-dunst-reels",
            "https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest",
        ])
        .output()
        .map_err(|e| format!("curl failed: {}", e))?;
    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("bad GitHub response: {}", e))?;
    json["tag_name"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "GitHub response had no tag_name".to_string())
}

fn version_parts(v: &str) -> Vec<u64> {
    v.split('.').map_while(|p| p.parse::<u64>().ok()).collect()
}

/// Checks that yt-dlp is installed and at least the latest release. `up_to_date` is null if the latest version couldn't be fetched.
fn check_ytdlp() -> serde_json::Value {
    let version = match hidden_command("yt-dlp").arg("--version").output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Ok(o) => {
            return serde_json::json!({
                "installed": false,
                "error": format!("yt-dlp --version failed: {}", String::from_utf8_lossy(&o.stderr).trim()),
            })
        }
        Err(e) => {
            return serde_json::json!({
                "installed": false,
                "error": format!("yt-dlp not found on PATH: {}", e),
            })
        }
    };

    match fetch_latest_ytdlp_version() {
        Ok(latest) => serde_json::json!({
            "installed": true,
            "version": version,
            "latest": latest,
            "up_to_date": version_parts(&version) >= version_parts(&latest),
        }),
        Err(e) => serde_json::json!({
            "installed": true,
            "version": version,
            "latest": null,
            "up_to_date": null,
            "error": e,
        }),
    }
}

fn extract_reel_urls(text: &str) -> Vec<String> {
    let re = Regex::new(r"https?://(?:www\.)?(?:instagram\.com/(?:reel|reels|p|share)/[^\s]+|tiktok\.com/[^\s]+|youtube\.com/shorts/[^\s]+)").unwrap();
    re.find_iter(text)
        .map(|m| m.as_str().to_string())
        .collect()
}

fn main() {
    let event_loop = EventLoopBuilder::<CustomEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // 1. Build System Tray Menu
    let tray_menu = Menu::new();

    let item_dnd = CheckMenuItem::new("Do Not Disturb (DND)", true, false, None);
    let item_mute = CheckMenuItem::new("Mute Sound", true, false, None);
    let item_open_links = CheckMenuItem::new("Open Links in Browser", true, true, None);

    let item_settings = MenuItem::new("Size Settings…", true, None);

    let reel_menu = Submenu::new("Reel Controls", true);
    let item_hide_toast_if_reel = CheckMenuItem::new("Hide Toast if Message is Only Reel (Jumpscare Mode)", true, false, None);
    let item_skip_reel = MenuItem::new("Skip Current Reel", true, None);
    let item_clear_queue = MenuItem::new("Clear Reel Queue", true, None);
    let _ = reel_menu.append(&item_hide_toast_if_reel);
    let _ = reel_menu.append(&PredefinedMenuItem::separator());
    let _ = reel_menu.append(&item_skip_reel);
    let _ = reel_menu.append(&item_clear_queue);

    let item_quit = MenuItem::new("Quit Dunst & Reels", true, None);

    let _ = tray_menu.append(&item_dnd);
    let _ = tray_menu.append(&item_mute);
    let _ = tray_menu.append(&item_open_links);
    let _ = tray_menu.append(&PredefinedMenuItem::separator());
    let _ = tray_menu.append(&item_settings);
    let _ = tray_menu.append(&reel_menu);
    let _ = tray_menu.append(&PredefinedMenuItem::separator());
    let _ = tray_menu.append(&item_quit);

    let _tray_icon = TrayIconBuilder::new()
        .with_menu(Box::new(tray_menu))
        .with_tooltip("Rice Dunst & Reel Player")
        .with_icon(create_purple_icon())
        .build()
        .unwrap();

    let proxy_menu = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let _ = proxy_menu.send_event(CustomEvent::Menu(event.id));
    }));

    let hide_toast_if_reel = Arc::new(AtomicBool::new(false));

    // Start HTTP server on 127.0.0.1:8999
    start_server(proxy.clone(), hide_toast_if_reel.clone());

    let mut settings = load_settings();
    let mut preview_active = false;

    // Toast Notification Window (sized/positioned by apply_layout once its webview exists)
    let toast_window = WindowBuilder::new()
        .with_title("DunstToast")
        .with_inner_size(LogicalSize::new(TOAST_BASE.0, TOAST_BASE.1))
        .with_decorations(false)
        .with_transparent(true)
        .with_always_on_top(true)
        .with_resizable(false)
        .with_visible(false)
        .build(&event_loop)
        .unwrap();

    let open_links_enabled = Arc::new(AtomicBool::new(true));

    let toast_html = r#"
    <!DOCTYPE html>
    <html>
    <head>
        <style>
            * { margin: 0; padding: 0; box-sizing: border-box; }
            html, body {
                background: transparent !important;
                width: 100vw;
                height: 100vh;
                overflow: hidden;
                font-family: 'JetBrains Mono', 'Segoe UI', sans-serif;
                user-select: none;
            }
            #toast {
                display: flex;
                align-items: stretch;
                width: 100%;
                height: 100%;
                background: rgba(17, 17, 27, 0.96);
                border: 3px solid #cba6f7;
                border-radius: 18px;
                box-shadow: 0 16px 48px rgba(0, 0, 0, 0.8);
            }
            .avatar {
                border-radius: 50%;
                border: 2.5px solid #cba6f7;
                object-fit: cover;
                flex-shrink: 0;
                align-self: flex-start;
            }
            .info {
                display: flex;
                flex-direction: column;
                width: 100%;
                height: 100%;
                overflow: hidden;
            }
            .author {
                color: #cba6f7;
                font-weight: 800;
                letter-spacing: 0.4px;
                flex-shrink: 0;
            }
            .msg-list {
                display: flex;
                flex-direction: column;
                width: 100%;
                flex: 1;
                overflow-y: auto;
            }
            .msg-list::-webkit-scrollbar { width: 4px; }
            .msg-list::-webkit-scrollbar-thumb { background: rgba(203, 166, 247, 0.4); border-radius: 4px; }

            .msg-bubble {
                flex: 1;
                display: flex;
                align-items: center;
                color: #cdd6f4;
                line-height: 1.4;
                word-break: break-word;
                background: rgba(255, 255, 255, 0.05);
                border-left: 3.5px solid #cba6f7;
                border-radius: 6px 12px 12px 6px;
                animation: popIn 0.2s cubic-bezier(0.16, 1, 0.3, 1);
            }

            .toast-link {
                color: #89b4fa;
                text-decoration: underline;
                cursor: pointer;
                word-break: break-all;
                user-select: text;
            }
            .toast-link:hover {
                color: #b4befe;
            }

            /* Layout is designed at 100% size; the webview zoom handles scaling. */
            body.full #toast { padding: 20px 26px; gap: 22px; border-width: 3px; }
            body.full .avatar { width: 72px; height: 72px; margin-top: 2px; }
            body.full .author { font-size: 22px; margin-bottom: 10px; }
            body.full .msg-list { gap: 10px; padding-right: 4px; }
            body.full .msg-bubble { font-size: 17px; min-height: 48px; padding: 10px 16px; }

            @keyframes popIn {
                from { opacity: 0; transform: translateY(4px); }
                to { opacity: 1; transform: translateY(0); }
            }
        </style>
    </head>
    <body class="full">
        <div id="toast">
            <img id="avatar" class="avatar" src="" />
            <div class="info">
                <div id="author" class="author"></div>
                <div id="msg-list" class="msg-list"></div>
            </div>
        </div>
        <script>
            function escapeHtml(text) {
                const div = document.createElement('div');
                div.textContent = text;
                return div.innerHTML;
            }

            function parseLinks(text) {
                const urlRegex = /(https?:\/\/[^\s]+)/g;
                const escaped = escapeHtml(text);
                return escaped.replace(urlRegex, function(url) {
                    return `<a class="toast-link" onclick="window.ipc.postMessage('open_url:' + '${url}'); event.stopPropagation();">${url}</a>`;
                });
            }

            function initThread(author, avatar, accent) {
                document.getElementById('author').innerText = '@' + author;
                document.getElementById('author').style.color = accent;
                document.getElementById('avatar').src = avatar;
                document.getElementById('avatar').style.borderColor = accent;
                document.getElementById('toast').style.borderColor = accent;
                document.getElementById('msg-list').innerHTML = '';
            }

            function appendBubble(content, accent) {
                const list = document.getElementById('msg-list');
                const bubble = document.createElement('div');
                bubble.className = 'msg-bubble';
                bubble.style.borderLeftColor = accent;
                bubble.innerHTML = parseLinks(content);
                list.appendChild(bubble);
                list.scrollTop = list.scrollHeight;
            }
        </script>
    </body>
    </html>
    "#;

    let open_links_flag = open_links_enabled.clone();
    let toast_webview = WebViewBuilder::new()
        .with_transparent(true)
        .with_background_color((0, 0, 0, 0))
        .with_ipc_handler(move |req| {
            let body = req.body();
            if body.starts_with("open_url:") {
                let url = &body["open_url:".len()..];
                if open_links_flag.load(Ordering::Relaxed) {
                    let _ = open::that(url);
                }
            }
        })
        .with_html(toast_html)
        .build(&toast_window)
        .unwrap();

    apply_layout(&toast_window, &toast_webview, TOAST_BASE, settings.toast_scale, Anchor::TopCenter(None));

    // Reel Player Window (top-right; sized/positioned by apply_layout)
    let reel_window = WindowBuilder::new()
        .with_title("ReelPlayer")
        .with_inner_size(LogicalSize::new(REEL_BASE.0, REEL_BASE.1))
        .with_decorations(false)
        .with_transparent(true)
        .with_always_on_top(true)
        .with_resizable(false)
        .with_visible(false)
        .build(&event_loop)
        .unwrap();

    let reel_html = r#"
    <!DOCTYPE html>
    <html>
    <head>
        <style>
            * { margin: 0; padding: 0; box-sizing: border-box; }
            html, body {
                background: transparent !important;
                width: 100vw;
                height: 100vh;
                overflow: hidden;
                font-family: 'JetBrains Mono', 'Segoe UI', sans-serif;
                user-select: none;
            }
            #container {
                position: relative;
                width: 100%;
                height: 100%;
                background: #11111b;
                border: 3px solid #cba6f7;
                border-radius: 20px;
                overflow: hidden;
                box-shadow: 0 20px 60px rgba(0, 0, 0, 0.95);
                display: flex;
                flex-direction: column;
            }
            .top-bar {
                position: absolute;
                top: 0;
                left: 0;
                right: 0;
                height: 58px;
                padding: 12px 16px;
                display: flex;
                align-items: center;
                justify-content: space-between;
                background: linear-gradient(180deg, rgba(17, 17, 27, 0.95) 0%, rgba(17, 17, 27, 0) 100%);
                z-index: 10;
            }
            .author-badge {
                display: flex;
                align-items: center;
                gap: 10px;
                background: rgba(17, 17, 27, 0.85);
                backdrop-filter: blur(8px);
                border: 1.5px solid rgba(203, 166, 247, 0.4);
                padding: 6px 14px;
                border-radius: 24px;
            }
            .author-avatar {
                width: 28px;
                height: 28px;
                border-radius: 50%;
                border: 2px solid #cba6f7;
                object-fit: cover;
            }
            .author-name {
                color: #cba6f7;
                font-size: 14px;
                font-weight: 700;
                max-width: 180px;
                white-space: nowrap;
                overflow: hidden;
                text-overflow: ellipsis;
            }
            .queue-badge {
                background: #f38ba8;
                color: #11111b;
                font-size: 12px;
                font-weight: 800;
                padding: 3px 9px;
                border-radius: 12px;
                margin-left: 4px;
                display: none;
            }
            .top-actions {
                display: flex;
                align-items: center;
                gap: 8px;
            }
            .icon-btn {
                background: rgba(17, 17, 27, 0.85);
                backdrop-filter: blur(8px);
                border: 1.5px solid rgba(203, 166, 247, 0.4);
                color: #cdd6f4;
                width: 36px;
                height: 36px;
                border-radius: 50%;
                display: flex;
                align-items: center;
                justify-content: center;
                cursor: pointer;
                font-size: 15px;
                transition: all 0.15s ease;
            }
            .icon-btn:hover {
                background: #cba6f7;
                color: #11111b;
                border-color: #cba6f7;
                transform: scale(1.08);
            }
            .close-btn:hover {
                background: #f38ba8 !important;
                border-color: #f38ba8 !important;
                color: #11111b !important;
            }
            .video-wrapper {
                position: relative;
                flex: 1;
                width: 100%;
                height: 100%;
                background: #000;
                cursor: pointer;
                display: flex;
                align-items: center;
                justify-content: center;
            }
            video {
                width: 100%;
                height: 100%;
                object-fit: cover;
                display: block;
            }
            .play-pause-overlay {
                position: absolute;
                width: 70px;
                height: 70px;
                background: rgba(17, 17, 27, 0.75);
                backdrop-filter: blur(8px);
                border: 2px solid #cba6f7;
                border-radius: 50%;
                display: flex;
                align-items: center;
                justify-content: center;
                color: #cba6f7;
                font-size: 28px;
                pointer-events: none;
                opacity: 0;
                transform: scale(0.7);
                transition: opacity 0.2s cubic-bezier(0.16, 1, 0.3, 1), transform 0.2s cubic-bezier(0.16, 1, 0.3, 1);
            }
            .play-pause-overlay.show {
                opacity: 1;
                transform: scale(1);
            }
            .bottom-bar {
                position: absolute;
                bottom: 0;
                left: 0;
                right: 0;
                padding: 12px 14px;
                display: flex;
                flex-direction: column;
                gap: 8px;
                background: linear-gradient(0deg, rgba(17, 17, 27, 0.95) 0%, rgba(17, 17, 27, 0) 100%);
                z-index: 10;
            }
            .progress-container {
                width: 100%;
                height: 4px;
                background: rgba(255, 255, 255, 0.2);
                border-radius: 2px;
                cursor: pointer;
                position: relative;
            }
            .progress-bar {
                height: 100%;
                background: #cba6f7;
                border-radius: 2px;
                width: 0%;
                transition: width 0.1s linear;
            }
            .controls-row {
                display: flex;
                align-items: center;
                justify-content: space-between;
            }
            .pill-btn {
                display: flex;
                align-items: center;
                gap: 8px;
                background: rgba(17, 17, 27, 0.85);
                backdrop-filter: blur(8px);
                border: 1.5px solid rgba(203, 166, 247, 0.4);
                color: #cdd6f4;
                padding: 8px 16px;
                border-radius: 20px;
                font-size: 13px;
                font-weight: 600;
                cursor: pointer;
                transition: all 0.15s ease;
            }
            .pill-btn:hover {
                background: #cba6f7;
                color: #11111b;
                border-color: #cba6f7;
                transform: translateY(-2px);
            }
            .pill-btn.primary {
                background: rgba(203, 166, 247, 0.2);
                border-color: #cba6f7;
                color: #cba6f7;
            }
            .pill-btn.primary:hover {
                background: #cba6f7;
                color: #11111b;
            }
            .next-btn {
                display: none;
                background: rgba(166, 227, 161, 0.2);
                border-color: #a6e3a1;
                color: #a6e3a1;
            }
            .next-btn:hover {
                background: #a6e3a1;
                color: #11111b;
            }
            .volume-container {
                display: flex;
                align-items: center;
                gap: 8px;
                background: rgba(17, 17, 27, 0.85);
                backdrop-filter: blur(8px);
                border: 1.5px solid rgba(203, 166, 247, 0.4);
                padding: 6px 12px;
                border-radius: 20px;
                transition: all 0.2s ease;
            }
            .volume-container:hover {
                border-color: #cba6f7;
            }
            .volume-btn {
                background: transparent;
                border: none;
                color: #cdd6f4;
                font-size: 14px;
                cursor: pointer;
                display: flex;
                align-items: center;
                justify-content: center;
                padding: 0;
                line-height: 1;
                transition: transform 0.15s ease;
            }
            .volume-btn:hover {
                transform: scale(1.15);
                color: #cba6f7;
            }
            .volume-slider-wrapper {
                width: 75px;
                height: 16px;
                display: flex;
                align-items: center;
                cursor: pointer;
                position: relative;
            }
            .volume-slider-track {
                width: 100%;
                height: 4px;
                background: rgba(255, 255, 255, 0.2);
                border-radius: 2px;
                position: relative;
                transition: height 0.15s ease;
            }
            .volume-slider-wrapper:hover .volume-slider-track {
                height: 6px;
            }
            .volume-slider-fill {
                height: 100%;
                background: #cba6f7;
                border-radius: 2px;
                width: 80%;
                transition: background 0.15s ease;
            }
            .volume-slider-wrapper:hover .volume-slider-fill {
                background: #a6e3a1;
            }
            .volume-slider-handle {
                position: absolute;
                top: 50%;
                left: 80%;
                transform: translate(-50%, -50%);
                width: 10px;
                height: 10px;
                background: #ffffff;
                border-radius: 50%;
                opacity: 0;
                box-shadow: 0 0 4px rgba(0, 0, 0, 0.6);
                transition: opacity 0.15s ease, transform 0.1s ease;
                pointer-events: none;
            }
            .volume-slider-wrapper:hover .volume-slider-handle,
            .volume-slider-wrapper.dragging .volume-slider-handle {
                opacity: 1;
                transform: translate(-50%, -50%) scale(1.2);
            }
        </style>
    </head>
    <body>
        <div id="container">
            <div class="top-bar">
                <div class="author-badge">
                    <img id="reel-avatar" class="author-avatar" src="" />
                    <span id="reel-author" class="author-name">@Reel</span>
                    <span id="queue-badge" class="queue-badge">+0 in queue</span>
                </div>
                <div class="top-actions">
                    <button class="icon-btn" id="open-link-btn" title="Open Original Link">🔗</button>
                    <button class="icon-btn close-btn" id="close-btn" title="Close Reel">✕</button>
                </div>
            </div>

            <div class="video-wrapper" id="video-wrapper">
                <video id="reel-player" playsinline></video>
                <div class="play-pause-overlay" id="play-pause-overlay">▶</div>
            </div>

            <div class="bottom-bar">
                <div class="progress-container" id="progress-container">
                    <div class="progress-bar" id="progress-bar"></div>
                </div>
                <div class="controls-row">
                    <button class="pill-btn primary" id="play-again-btn">
                        <span>🔄</span>
                        <span>Play Again</span>
                    </button>
                    <button class="pill-btn next-btn" id="next-btn">
                        <span>⏭</span>
                        <span>Next</span>
                    </button>
                    <div class="volume-container">
                        <button class="volume-btn" id="volume-btn" title="Mute/Unmute">🔊</button>
                        <div class="volume-slider-wrapper" id="volume-slider-wrapper">
                            <div class="volume-slider-track" id="volume-slider-track">
                                <div class="volume-slider-fill" id="volume-slider-fill"></div>
                                <div class="volume-slider-handle" id="volume-slider-handle"></div>
                            </div>
                        </div>
                    </div>
                </div>
            </div>
        </div>

        <script>
            let currentOriginalUrl = "";
            let autoCloseTimeout = null;
            let currentVolume = 0.8;
            let isMuted = false;

            try {
                const savedVol = localStorage.getItem('reel_volume');
                if (savedVol !== null && !isNaN(parseFloat(savedVol))) {
                    currentVolume = parseFloat(savedVol);
                }
                const savedMuted = localStorage.getItem('reel_muted');
                if (savedMuted !== null) {
                    isMuted = (savedMuted === 'true');
                }
            } catch (e) {
                // localStorage not accessible in embedded HTML
            }

            const video = document.getElementById('reel-player');
            const wrapper = document.getElementById('video-wrapper');
            const overlay = document.getElementById('play-pause-overlay');
            const progressBar = document.getElementById('progress-bar');
            const progressContainer = document.getElementById('progress-container');
            const volumeBtn = document.getElementById('volume-btn');
            const volumeSliderWrapper = document.getElementById('volume-slider-wrapper');
            const volumeSliderTrack = document.getElementById('volume-slider-track');
            const volumeSliderFill = document.getElementById('volume-slider-fill');
            const volumeSliderHandle = document.getElementById('volume-slider-handle');
            const playAgainBtn = document.getElementById('play-again-btn');
            const nextBtn = document.getElementById('next-btn');
            const closeBtn = document.getElementById('close-btn');
            const openLinkBtn = document.getElementById('open-link-btn');
            const queueBadge = document.getElementById('queue-badge');

            function updateVolumeUI() {
                if (isNaN(currentVolume)) currentVolume = 0.8;
                currentVolume = Math.max(0, Math.min(1, currentVolume));
                const effectiveVol = isMuted ? 0 : currentVolume;
                const pct = Math.round(effectiveVol * 100);
                if (volumeSliderFill) volumeSliderFill.style.width = pct + '%';
                if (volumeSliderHandle) volumeSliderHandle.style.left = pct + '%';

                if (volumeBtn) {
                    if (isMuted || effectiveVol === 0) {
                        volumeBtn.innerText = '🔇';
                    } else if (effectiveVol < 0.35) {
                        volumeBtn.innerText = '🔈';
                    } else if (effectiveVol < 0.7) {
                        volumeBtn.innerText = '🔉';
                    } else {
                        volumeBtn.innerText = '🔊';
                    }
                }

                try {
                    video.volume = currentVolume;
                    video.muted = isMuted;
                } catch (e) {}
            }

            function setVolumeFromEvent(e) {
                if (!volumeSliderTrack) return;
                const rect = volumeSliderTrack.getBoundingClientRect();
                let x = e.clientX - rect.left;
                let ratio = Math.max(0, Math.min(1, x / rect.width));
                currentVolume = ratio;
                isMuted = (ratio === 0);
                try {
                    localStorage.setItem('reel_volume', currentVolume.toString());
                    localStorage.setItem('reel_muted', isMuted.toString());
                } catch (err) {}
                updateVolumeUI();
            }

            let isDraggingVolume = false;

            if (volumeSliderWrapper) {
                volumeSliderWrapper.addEventListener('mousedown', (e) => {
                    isDraggingVolume = true;
                    volumeSliderWrapper.classList.add('dragging');
                    setVolumeFromEvent(e);
                });
            }

            window.addEventListener('mousemove', (e) => {
                if (isDraggingVolume) {
                    setVolumeFromEvent(e);
                }
            });

            window.addEventListener('mouseup', () => {
                if (isDraggingVolume) {
                    isDraggingVolume = false;
                    if (volumeSliderWrapper) volumeSliderWrapper.classList.remove('dragging');
                }
            });

            if (volumeBtn) {
                volumeBtn.addEventListener('click', () => {
                    if (isMuted) {
                        isMuted = false;
                        if (currentVolume === 0) currentVolume = 0.8;
                    } else {
                        isMuted = true;
                    }
                    try {
                        localStorage.setItem('reel_muted', isMuted.toString());
                    } catch (err) {}
                    updateVolumeUI();
                });
            }

            function clearAutoClose() {
                if (autoCloseTimeout) {
                    clearTimeout(autoCloseTimeout);
                    autoCloseTimeout = null;
                }
            }

            function loadReel(streamUrl, author, avatar, originalUrl, queueCount) {
                clearAutoClose();
                currentOriginalUrl = originalUrl;
                const authorEl = document.getElementById('reel-author');
                if (authorEl) authorEl.innerText = '@' + author;
                const avatarEl = document.getElementById('reel-avatar');
                if (avatarEl) avatarEl.src = avatar || "https://cdn.discordapp.com/embed/avatars/0.png";
                updateQueueCount(queueCount);

                updateVolumeUI();
                video.src = streamUrl;
                video.load();

                const playPromise = video.play();
                if (playPromise !== undefined) {
                    playPromise.catch((err) => {
                        console.warn("Unmuted playback restricted, trying muted:", err);
                        isMuted = true;
                        updateVolumeUI();
                        video.play().catch(e => console.error("Play failed:", e));
                    });
                }
            }

            function previewReel() {
                clearAutoClose();
                document.getElementById('reel-author').innerText = '@Preview';
                document.getElementById('reel-avatar').src = 'https://cdn.discordapp.com/embed/avatars/0.png';
                updateQueueCount(2);
                updateVolumeUI();
            }

            function updateQueueCount(count) {
                if (count > 0) {
                    queueBadge.style.display = "inline-block";
                    queueBadge.innerText = "+" + count + " queued";
                    nextBtn.style.display = "flex";
                } else {
                    queueBadge.style.display = "none";
                    nextBtn.style.display = "none";
                }
            }

            function stopReel() {
                clearAutoClose();
                video.pause();
                video.src = "";
            }

            function togglePlayPause() {
                clearAutoClose();
                if (video.paused) {
                    video.play();
                    overlay.innerText = "▶";
                    overlay.classList.add('show');
                    setTimeout(() => overlay.classList.remove('show'), 400);
                } else {
                    video.pause();
                    overlay.innerText = "⏸";
                    overlay.classList.add('show');
                }
            }

            wrapper.addEventListener('click', (e) => {
                if (e.target === wrapper || e.target === video) {
                    togglePlayPause();
                }
            });

            playAgainBtn.addEventListener('click', () => {
                clearAutoClose();
                video.currentTime = 0;
                video.play();
                overlay.innerText = "🔄";
                overlay.classList.add('show');
                setTimeout(() => overlay.classList.remove('show'), 400);
            });

            closeBtn.addEventListener('click', () => {
                clearAutoClose();
                window.ipc.postMessage('reel_close');
            });

            nextBtn.addEventListener('click', () => {
                clearAutoClose();
                window.ipc.postMessage('reel_next');
            });

            openLinkBtn.addEventListener('click', () => {
                if (currentOriginalUrl) {
                    window.ipc.postMessage('open_url:' + currentOriginalUrl);
                }
            });

            video.addEventListener('timeupdate', () => {
                if (video.duration) {
                    const pct = (video.currentTime / video.duration) * 100;
                    progressBar.style.width = pct + "%";
                }
            });

            video.addEventListener('ended', () => {
                clearAutoClose();
                // When video finishes, wait 1.5s then auto-advance/close unless replayed
                autoCloseTimeout = setTimeout(() => {
                    window.ipc.postMessage('reel_next');
                }, 1500);
            });

            progressContainer.addEventListener('click', (e) => {
                clearAutoClose();
                const rect = progressContainer.getBoundingClientRect();
                const clickX = e.clientX - rect.left;
                const width = rect.width;
                if (video.duration && width > 0) {
                    video.currentTime = (clickX / width) * video.duration;
                }
            });
        </script>
    </body>
    </html>
    "#;

    let proxy_reel = proxy.clone();
    let reel_webview = WebViewBuilder::new()
        .with_transparent(true)
        .with_background_color((0, 0, 0, 0))
        .with_ipc_handler(move |req| {
            let body = req.body();
            if body == "reel_close" {
                let _ = proxy_reel.send_event(CustomEvent::ReelClose);
            } else if body == "reel_next" {
                let _ = proxy_reel.send_event(CustomEvent::ReelNext);
            } else if body.starts_with("open_url:") {
                let url = &body["open_url:".len()..];
                let _ = open::that(url);
            }
        })
        .with_html(reel_html)
        .build(&reel_window)
        .unwrap();

    apply_layout(&reel_window, &reel_webview, REEL_BASE, settings.reel_scale, Anchor::TopRight);

    // Size Settings window
    let settings_window = WindowBuilder::new()
        .with_title("Dunst Size Settings")
        .with_inner_size(LogicalSize::new(400.0, 440.0))
        .with_position(LogicalPosition::new(60.0, 380.0))
        .with_theme(theme_from_str(&settings.theme))
        .with_always_on_top(true)
        .with_resizable(false)
        .with_visible(false)
        .build(&event_loop)
        .unwrap();
    let settings_id = settings_window.id();

    let settings_html = r#"
    <!DOCTYPE html>
    <html>
    <head>
        <meta charset="utf-8">
        <style>
            :root {
                --bg: #eff1f5; --card: #ffffff; --border: #ccd0da; --text: #4c4f69; --muted: #7c7f93;
                --accent: #8839ef; --accent-soft: rgba(136,57,239,0.12); --track: #ccd0da; --on-accent: #ffffff;
            }
            @media (prefers-color-scheme: dark) {
                :root:not([data-theme="light"]) {
                    --bg: #181825; --card: #1e1e2e; --border: #313244; --text: #cdd6f4; --muted: #9399b2;
                    --accent: #cba6f7; --accent-soft: rgba(203,166,247,0.14); --track: #45475a; --on-accent: #11111b;
                }
            }
            :root[data-theme="dark"] {
                --bg: #181825; --card: #1e1e2e; --border: #313244; --text: #cdd6f4; --muted: #9399b2;
                --accent: #cba6f7; --accent-soft: rgba(203,166,247,0.14); --track: #45475a; --on-accent: #11111b;
            }
            * { box-sizing: border-box; margin: 0; padding: 0; }
            html, body { height: 100%; }
            body {
                background: var(--bg); color: var(--text);
                font-family: 'Segoe UI Variable Text', 'Segoe UI', system-ui, sans-serif;
                font-size: 13px; padding: 18px; user-select: none; overflow: hidden;
                transition: background 0.2s, color 0.2s;
            }
            header { display: flex; align-items: center; justify-content: space-between; margin-bottom: 14px; }
            h1 { font-size: 16px; font-weight: 700; letter-spacing: 0.2px; }
            h1 small { display: block; font-size: 11px; font-weight: 500; color: var(--muted); margin-top: 2px; }
            .seg { display: flex; background: var(--card); border: 1px solid var(--border); border-radius: 10px; padding: 2px; }
            .seg button {
                background: none; border: none; color: var(--muted); padding: 4px 9px; border-radius: 8px;
                font: inherit; font-size: 12px; cursor: pointer; transition: all 0.15s;
            }
            .seg button.active { background: var(--accent); color: var(--on-accent); font-weight: 600; }
            .card {
                background: var(--card); border: 1px solid var(--border); border-radius: 14px;
                padding: 14px 16px; margin-bottom: 10px; transition: background 0.2s, border-color 0.2s;
            }
            .label { display: flex; justify-content: space-between; align-items: center; margin-bottom: 12px; font-weight: 600; }
            .val { background: var(--accent-soft); color: var(--accent); padding: 2px 10px; border-radius: 999px; font-size: 12px; font-weight: 700; min-width: 54px; text-align: center; }
            input[type=range] {
                -webkit-appearance: none; appearance: none; width: 100%; height: 6px; border-radius: 999px; outline: none; cursor: pointer;
                background: linear-gradient(to right, var(--accent) var(--p, 50%), var(--track) var(--p, 50%));
            }
            input[type=range]::-webkit-slider-thumb {
                -webkit-appearance: none; width: 18px; height: 18px; border-radius: 50%; background: #fff;
                border: 3px solid var(--accent); box-shadow: 0 2px 6px rgba(0,0,0,0.3); transition: transform 0.12s;
            }
            input[type=range]:hover::-webkit-slider-thumb { transform: scale(1.15); }
            .toggle-row { display: flex; align-items: center; justify-content: space-between; cursor: pointer; font-weight: 600; }
            .toggle-row small { display: block; font-weight: 400; color: var(--muted); font-size: 11px; margin-top: 2px; }
            .switch { position: relative; width: 40px; height: 22px; background: var(--track); border-radius: 999px; transition: background 0.2s; flex-shrink: 0; }
            .switch::after { content: ''; position: absolute; top: 3px; left: 3px; width: 16px; height: 16px; border-radius: 50%; background: #fff; transition: transform 0.2s; }
            .switch.on { background: var(--accent); }
            .switch.on::after { transform: translateX(18px); }
            .reset {
                width: 100%; background: none; border: 1px solid var(--border); color: var(--muted); padding: 8px; border-radius: 10px;
                font: inherit; cursor: pointer; transition: all 0.15s;
            }
            .reset:hover { border-color: var(--accent); color: var(--accent); }
        </style>
    </head>
    <body>
        <header>
            <h1>Size Settings<small>Toast &amp; reel player scaling</small></h1>
            <div class="seg" id="theme-seg">
                <button data-theme="system">Auto</button>
                <button data-theme="light">Light</button>
                <button data-theme="dark">Dark</button>
            </div>
        </header>
        <div class="card">
            <div class="label"><span>Toast</span><span class="val" id="toast-val"></span></div>
            <input type="range" id="toast" min="40" max="150" step="5">
        </div>
        <div class="card">
            <div class="label"><span>Reel player</span><span class="val" id="reel-val"></span></div>
            <input type="range" id="reel" min="50" max="250" step="5">
        </div>
        <div class="card">
            <div class="toggle-row" id="preview-row">
                <span>Live preview<small>Show sample toast &amp; reel window while editing</small></span>
                <div class="switch" id="preview-switch"></div>
            </div>
        </div>
        <button class="reset" id="reset">Reset to defaults</button>
        <script>
            const init = __INIT__;
            const toast = document.getElementById('toast');
            const reel = document.getElementById('reel');
            const sw = document.getElementById('preview-switch');
            let previewOn = false;

            function paint(el) { el.style.setProperty('--p', ((el.value - el.min) / (el.max - el.min) * 100) + '%'); }
            function show() {
                document.getElementById('toast-val').innerText = toast.value + '%';
                document.getElementById('reel-val').innerText = reel.value + '%';
                paint(toast); paint(reel);
            }
            function setPreview(on) { previewOn = on; sw.classList.toggle('on', on); }
            function applyTheme(t) {
                if (t === 'system') document.documentElement.removeAttribute('data-theme');
                else document.documentElement.setAttribute('data-theme', t);
                document.querySelectorAll('#theme-seg button').forEach(b => b.classList.toggle('active', b.dataset.theme === t));
            }

            toast.value = init.toast; reel.value = init.reel; show(); applyTheme(init.theme);
            toast.addEventListener('input', () => { show(); window.ipc.postMessage('toast:' + toast.value); });
            reel.addEventListener('input', () => { show(); window.ipc.postMessage('reel:' + reel.value); });
            toast.addEventListener('change', () => window.ipc.postMessage('save'));
            reel.addEventListener('change', () => window.ipc.postMessage('save'));
            document.getElementById('preview-row').addEventListener('click', () => {
                setPreview(!previewOn);
                window.ipc.postMessage('preview:' + (previewOn ? '1' : '0'));
            });
            document.querySelectorAll('#theme-seg button').forEach(b => b.addEventListener('click', () => {
                applyTheme(b.dataset.theme);
                window.ipc.postMessage('theme:' + b.dataset.theme);
                window.ipc.postMessage('save');
            }));
            document.getElementById('reset').addEventListener('click', () => {
                toast.value = 100; reel.value = 150; show();
                window.ipc.postMessage('toast:100');
                window.ipc.postMessage('reel:150');
                window.ipc.postMessage('save');
            });
        </script>
    </body>
    </html>
    "#
    .replace(
        "__INIT__",
        &format!(
            "{{toast: {}, reel: {}, theme: {:?}}}",
            settings.toast_scale, settings.reel_scale, settings.theme
        ),
    );

    let proxy_settings = proxy.clone();
    let settings_webview = WebViewBuilder::new()
        .with_ipc_handler(move |req| {
            let body = req.body();
            let event = if let Some(v) = body.strip_prefix("toast:") {
                v.parse().ok().map(CustomEvent::SetToastScale)
            } else if let Some(v) = body.strip_prefix("reel:") {
                v.parse().ok().map(CustomEvent::SetReelScale)
            } else if let Some(v) = body.strip_prefix("preview:") {
                Some(CustomEvent::SetPreview(v == "1"))
            } else if let Some(v) = body.strip_prefix("theme:") {
                Some(CustomEvent::SetTheme(v.to_string()))
            } else if body == "save" {
                Some(CustomEvent::SaveSettings)
            } else {
                None
            };
            if let Some(e) = event {
                let _ = proxy_settings.send_event(e);
            }
        })
        .with_html(settings_html)
        .build(&settings_window)
        .unwrap();

    #[cfg(feature = "debug-hooks")]
    hooks::install(proxy.clone());

    let notification_counter = Arc::new(AtomicUsize::new(0));
    let mut last_author: Option<String> = None;
    let mut last_time: Option<Instant> = None;
    let mut history: Vec<String> = Vec::new();

    // Reel Queue state
    let mut active_reel: Option<ReelItem> = None;
    let mut reel_queue: VecDeque<ReelItem> = VecDeque::new();

    let _id_dnd = item_dnd.id().clone();
    let _id_mute = item_mute.id().clone();
    let id_open_links = item_open_links.id().clone();
    let id_settings = item_settings.id().clone();
    let id_hide_toast = item_hide_toast_if_reel.id().clone();
    let id_skip = item_skip_reel.id().clone();
    let id_clear = item_clear_queue.id().clone();
    let id_quit = item_quit.id().clone();

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::NewEvents(StartCause::Init) => {},
            Event::UserEvent(CustomEvent::Menu(id)) => {
                if id == id_quit {
                    *control_flow = ControlFlow::Exit;
                } else if id == id_open_links {
                    open_links_enabled.store(item_open_links.is_checked(), Ordering::Relaxed);
                } else if id == id_hide_toast {
                    hide_toast_if_reel.store(item_hide_toast_if_reel.is_checked(), Ordering::Relaxed);
                } else if id == id_settings {
                    settings_window.set_visible(true);
                    settings_window.set_focus();
                } else if id == id_skip {
                    let _ = proxy.send_event(CustomEvent::SkipReel);
                } else if id == id_clear {
                    let _ = proxy.send_event(CustomEvent::ClearReelQueue);
                }
            },
            Event::UserEvent(CustomEvent::SetToastScale(p)) => {
                settings.toast_scale = p;
                place_toast(&toast_window, &toast_webview, &reel_window, p);
            },
            Event::UserEvent(CustomEvent::SetReelScale(p)) => {
                settings.reel_scale = p;
                apply_layout(&reel_window, &reel_webview, REEL_BASE, p, Anchor::TopRight);
                place_toast(&toast_window, &toast_webview, &reel_window, settings.toast_scale);
            },
            Event::UserEvent(CustomEvent::SetTheme(t)) => {
                settings_window.set_theme(theme_from_str(&t));
                settings.theme = t;
            },
            Event::UserEvent(CustomEvent::SaveSettings) => save_settings(&settings),
            Event::UserEvent(CustomEvent::SetPreview(on)) => {
                preview_active = on;
                last_author = None;
                history.clear();
                if on {
                    let js = "initThread('Preview', 'https://cdn.discordapp.com/embed/avatars/0.png', '#cba6f7'); \
                        appendBubble('This is how big your toasts are.', '#cba6f7'); \
                        appendBubble('A longer message wraps onto a second line so you can judge readability at this size, with room for a few more words.', '#cba6f7');";
                    let _ = toast_webview.evaluate_script(js);
                    toast_window.set_visible(true);
                    toast_window.set_always_on_top(true);
                    if active_reel.is_none() {
                        let _ = reel_webview.evaluate_script("previewReel();");
                        reel_window.set_visible(true);
                        reel_window.set_always_on_top(true);
                    }
                    place_toast(&toast_window, &toast_webview, &reel_window, settings.toast_scale);
                } else {
                    toast_window.set_visible(false);
                    if active_reel.is_none() {
                        reel_window.set_visible(false);
                        let _ = reel_webview.evaluate_script("stopReel();");
                    }
                    place_toast(&toast_window, &toast_webview, &reel_window, settings.toast_scale);
                }
            },
            Event::UserEvent(CustomEvent::ShowToast(payload)) => {
                if item_dnd.is_checked() {
                    return;
                }

                let now = Instant::now();
                let accent = payload.accent_color.clone().unwrap_or_else(|| "#cba6f7".to_string());

                let is_same_thread = match (&last_author, &last_time) {
                    (Some(auth), Some(time)) => {
                        auth == &payload.author && now.duration_since(*time) < Duration::from_secs(14)
                    }
                    _ => false,
                };

                if is_same_thread {
                    history.push(payload.content.clone());
                    let js = format!("appendBubble({:?}, {:?});", payload.content, accent);
                    let _ = toast_webview.evaluate_script(&js);
                } else {
                    history = vec![payload.content.clone()];
                    last_author = Some(payload.author.clone());
                    let js = format!(
                        "initThread({:?}, {:?}, {:?}); appendBubble({:?}, {:?});",
                        payload.author, payload.avatar, accent, payload.content, accent
                    );
                    let _ = toast_webview.evaluate_script(&js);
                }
                last_time = Some(now);

                let total_chars: usize = history.iter().map(|m| m.len()).sum();
                let reading_time_ms = (4500 + (total_chars * 45)).clamp(4500, 20000) as u64;

                if !item_mute.is_checked() {
                    if let Some(ref sound) = payload.sound_path {
                        let sound_clone = sound.clone();
                        thread::spawn(move || {
                            if let Ok(file) = File::open(&sound_clone) {
                                if let Ok((_stream, handle)) = rodio::OutputStream::try_default() {
                                    if let Ok(sink) = rodio::Sink::try_new(&handle) {
                                        if let Ok(decoder) = rodio::Decoder::new(BufReader::new(file)) {
                                            sink.append(decoder);
                                            sink.sleep_until_end();
                                        }
                                    }
                                }
                            }
                        });
                    }
                }

                place_toast(&toast_window, &toast_webview, &reel_window, settings.toast_scale);
                toast_window.set_visible(true);
                toast_window.set_always_on_top(true);

                let current_id = notification_counter.fetch_add(1, Ordering::SeqCst) + 1;
                let proxy_clone = proxy.clone();
                thread::spawn(move || {
                    thread::sleep(Duration::from_millis(reading_time_ms));
                    let _ = proxy_clone.send_event(CustomEvent::HideToast(current_id));
                });
            },
            Event::UserEvent(CustomEvent::HideToast(id)) => {
                if !preview_active && notification_counter.load(Ordering::SeqCst) == id {
                    toast_window.set_visible(false);
                    history.clear();
                    last_author = None;
                }
            },
            Event::UserEvent(CustomEvent::ReelReady(item)) => {
                if item_dnd.is_checked() {
                    return;
                }
                if active_reel.is_none() {
                    active_reel = Some(item.clone());
                    let js = format!(
                        "loadReel({:?}, {:?}, {:?}, {:?}, {});",
                        item.stream_url, item.author, item.avatar, item.original_url, reel_queue.len()
                    );
                    let _ = reel_webview.evaluate_script(&js);
                    reel_window.set_visible(true);
                    reel_window.set_always_on_top(true);
                    place_toast(&toast_window, &toast_webview, &reel_window, settings.toast_scale);
                } else {
                    reel_queue.push_back(item);
                    let js = format!("updateQueueCount({});", reel_queue.len());
                    let _ = reel_webview.evaluate_script(&js);
                }
            },
            Event::UserEvent(CustomEvent::ReelClose) | Event::UserEvent(CustomEvent::ReelNext) | Event::UserEvent(CustomEvent::SkipReel) => {
                if let Some(next_item) = reel_queue.pop_front() {
                    active_reel = Some(next_item.clone());
                    let js = format!(
                        "loadReel({:?}, {:?}, {:?}, {:?}, {});",
                        next_item.stream_url, next_item.author, next_item.avatar, next_item.original_url, reel_queue.len()
                    );
                    let _ = reel_webview.evaluate_script(&js);
                    reel_window.set_visible(true);
                    reel_window.set_always_on_top(true);
                } else {
                    active_reel = None;
                    reel_window.set_visible(false);
                    let _ = reel_webview.evaluate_script("stopReel();");
                    place_toast(&toast_window, &toast_webview, &reel_window, settings.toast_scale);
                }
            },
            Event::UserEvent(CustomEvent::ClearReelQueue) => {
                reel_queue.clear();
                let _ = reel_webview.evaluate_script("updateQueueCount(0);");
            },
            Event::WindowEvent { window_id, event: WindowEvent::CloseRequested, .. } => {
                if window_id == settings_id {
                    settings_window.set_visible(false);
                    let _ = settings_webview.evaluate_script("setPreview(false);");
                    let _ = proxy.send_event(CustomEvent::SetPreview(false));
                    save_settings(&settings);
                } else {
                    *control_flow = ControlFlow::Exit;
                }
            },
            _ => (),
        }
    });
}

fn start_server(proxy: EventLoopProxy<CustomEvent>, hide_toast_if_reel: Arc<AtomicBool>) {
    thread::spawn(move || {
        let listener = TcpListener::bind("127.0.0.1:8999").expect("Failed to bind 127.0.0.1:8999");

        for stream in listener.incoming() {
            if let Ok(mut stream) = stream {
                let mut buffer = [0u8; 8192];
                if let Ok(bytes_read) = stream.read(&mut buffer) {
                    let request = String::from_utf8_lossy(&buffer[..bytes_read]);

                    if request.starts_with("GET /health/ytdlp") {
                        let body = check_ytdlp().to_string();
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nAccess-Control-Allow-Origin: *\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                            body.len(), body
                        );
                        let _ = stream.write_all(response.as_bytes());
                        continue;
                    }

                    if request.starts_with("OPTIONS") {
                        let response = "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: POST, OPTIONS\r\nAccess-Control-Allow-Headers: *\r\n\r\n";
                        let _ = stream.write_all(response.as_bytes());
                        continue;
                    }

                    if let Some(body_start) = request.find("\r\n\r\n") {
                        let body = &request[body_start + 4..];
                        if let Ok(payload) = serde_json::from_str::<ToastPayload>(body.trim()) {
                            let reel_urls = extract_reel_urls(&payload.content);

                            // Check if message content is only a reel link (or links)
                            let is_only_reel = !reel_urls.is_empty() && {
                                let mut remaining = payload.content.trim().to_string();
                                for url in &reel_urls {
                                    remaining = remaining.replace(url, "");
                                }
                                remaining.trim().is_empty()
                            };

                            let skip_toast = hide_toast_if_reel.load(Ordering::Relaxed) && is_only_reel;

                            // 1. Trigger toast notification if not skipped by jumpscare mode
                            if !skip_toast {
                                let _ = proxy.send_event(CustomEvent::ShowToast(payload.clone()));
                            }

                            // 2. Extract potential reels asynchronously
                            for url in reel_urls {
                                let proxy_worker = proxy.clone();
                                let author_clone = payload.author.clone();
                                let avatar_clone = payload.avatar.clone();
                                let is_test = payload.test;
                                thread::spawn(move || match get_mp4_with_ytdlp(&url) {
                                    Ok(stream_url) => {
                                        let item = ReelItem {
                                            author: author_clone,
                                            avatar: avatar_clone,
                                            original_url: url,
                                            stream_url,
                                        };
                                        let _ = proxy_worker.send_event(CustomEvent::ReelReady(item));
                                    }
                                    Err(e) if is_test => {
                                        let msg: String = e.chars().take(300).collect();
                                        let _ = proxy_worker.send_event(CustomEvent::ShowToast(ToastPayload {
                                            author: "Reel Test".to_string(),
                                            content: format!("❌ Reel extraction failed: {}", msg),
                                            avatar: avatar_clone,
                                            sound_path: None,
                                            accent_color: Some("#f38ba8".to_string()),
                                            position: None,
                                            test: true,
                                        }));
                                    }
                                    Err(_) => {}
                                });
                            }
                        }
                    }

                    let response = "HTTP/1.1 200 OK\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: 2\r\n\r\nOK";
                    let _ = stream.write_all(response.as_bytes());
                }
            }
        }
    });
}
