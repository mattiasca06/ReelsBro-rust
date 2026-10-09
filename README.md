# ReelsBro-rust

A small Windows tray app that shows Discord notifications as custom toast popups, and plays Instagram, TikTok and YouTube reels that people send you in DMs.

Discord messages get to the app through a BetterDiscord plugin that posts them to a local server on port 8999.

## What it does

- Shows pings and DMs as toasts in the top center of the screen. Messages from the same person stack into one chat-style window.
- Plays a sound when a notification comes in.
- Detects reel links in messages and opens a small player in the top right, using yt-dlp to get the video. If the toast and the player overlap, the player pushes the toast to the left.
- Has a size settings window (tray icon menu) with sliders for toast and reel size, a live preview, and light/dark/auto theme. Sizes follow your display scaling. Settings are saved in `%APPDATA%\rice-dunst-reels\settings.json`.

## Requirements

- Windows 10 or 11 with WebView2 (already there on most machines)
- Rust toolchain
- [yt-dlp](https://github.com/yt-dlp/yt-dlp) on your PATH, for reels
- [BetterDiscord](https://betterdiscord.app/)

## Build and run

```
cargo run --release
```

To just build it:

```
cargo build --release
```

The exe ends up in `target/release`.

## Plugin

Copy `DunstToasts.plugin.js` into your BetterDiscord plugins folder and enable it. Before using it, change `soundPath` at the top of the file to an audio file that exists on your machine.

The plugin settings page has test buttons: a single ping, a fake DM spam, a yt-dlp installed and up to date check, and a fake reel DM.

## Debug hooks

There is a feature flag that runs a scripted resize scenario on startup, which is handy for checking the layout:

```
cargo run --features debug-hooks
```

You can change the sequence with `RICE_HOOK_TOAST`, `RICE_HOOK_REEL` (comma separated percents) and `RICE_HOOK_STEP_MS`. A normal build doesn't include any of it.
