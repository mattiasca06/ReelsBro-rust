//! Debug-only test hooks. Compiled only with `cargo run --features debug-hooks`.
//!
//! Drives the app through a scripted scenario so layout/rendering can be checked without clicking:
//! turns the size preview on, then applies toast and reel scales in sequence.
//!
//! Configure with env vars (comma-separated percents):
//!   RICE_HOOK_TOAST=40,100,150   RICE_HOOK_REEL=100,250   RICE_HOOK_STEP_MS=400

use crate::CustomEvent;
use std::thread;
use std::time::Duration;
use tao::event_loop::EventLoopProxy;

fn percents(var: &str, default: &[f64]) -> Vec<f64> {
    match std::env::var(var) {
        Ok(v) => v.split(',').filter_map(|s| s.trim().parse().ok()).collect(),
        Err(_) => default.to_vec(),
    }
}

pub fn install(proxy: EventLoopProxy<CustomEvent>) {
    let toast = percents("RICE_HOOK_TOAST", &[40.0, 100.0, 150.0]);
    let reel = percents("RICE_HOOK_REEL", &[]);
    let step = Duration::from_millis(
        std::env::var("RICE_HOOK_STEP_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(400),
    );

    // RICE_HOOK_REPLY=<text>: show a toast tied to channel "123456", then submit <text> as a reply to it.
    // Check the result with `curl http://127.0.0.1:8999/outbox`.
    if let Ok(text) = std::env::var("RICE_HOOK_REPLY") {
        let proxy = proxy.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(1500));
            let _ = proxy.send_event(CustomEvent::ShowToast(crate::ToastPayload {
                author: "HookUser".into(),
                content: "hook message".into(),
                avatar: "https://cdn.discordapp.com/embed/avatars/0.png".into(),
                sound_path: None,
                accent_color: None,
                position: None,
                test: true,
                channel_id: Some("123456".into()),
            }));
            thread::sleep(step);
            let _ = proxy.send_event(CustomEvent::Reply(text));
        });
        return;
    }

    thread::spawn(move || {
        thread::sleep(Duration::from_millis(1500));
        let _ = proxy.send_event(CustomEvent::SetPreview(true));
        for p in toast {
            thread::sleep(step);
            let _ = proxy.send_event(CustomEvent::SetToastScale(p));
        }
        for p in reel {
            thread::sleep(step);
            let _ = proxy.send_event(CustomEvent::SetReelScale(p));
        }
    });
}
