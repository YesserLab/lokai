//! LoKI (lokale künstliche Intelligenz) — a launcher-style chat client for local AI models,
//! with tools, for KDE Plasma.

mod api;
mod compat;
mod config;
mod core;
mod desktop;
mod icons;
mod python;
mod theme;
mod tools;
mod ui;

use desktop::{Cmd, Commander};

const HELP: &str = "\
LoKI (lokale künstliche Intelligenz) — chat with your local AI model from a launcher window.

Usage: loki [OPTION]

  (no option)    start (or show the running instance's chat)
  --background   start with only the tray icon (for autostart)
  --toggle       show or hide the chat window
  --settings     open the settings
  --quit         quit the running instance
  -h, --help     show this help

The chat toggles with Meta+Alt+Space (change it in System Settings → Keyboard → Shortcuts → LoKI).
Settings live in ~/.config/loki/config.toml.";

fn main() {
    let arg = std::env::args().nth(1);
    let (method, initial) = match arg.as_deref() {
        None => ("Show", Some(Cmd::ShowChat)),
        Some("--background") => ("", None),
        Some("--toggle") => ("Toggle", Some(Cmd::ShowChat)),
        Some("--settings") => ("Settings", Some(Cmd::ShowSettings)),
        Some("--quit") => ("Quit", None),
        Some("-h" | "--help") => {
            println!("{HELP}");
            return;
        }
        Some(other) => {
            eprintln!("loki: unknown option {other}\n\n{HELP}");
            std::process::exit(2);
        }
    };

    // Single instance: hand the request to the running one.
    if desktop::forward(if method.is_empty() { "Ping" } else { method }) || method == "Quit" {
        return;
    }

    // The window only speaks Wayland (KDE Plasma 6); X11 isn't supported.
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        eprintln!("loki: LoKI needs a Wayland session (WAYLAND_DISPLAY is not set)");
        std::process::exit(1);
    }

    let core = core::Core::new();
    let (tx, rx) = std::sync::mpsc::channel();
    let cmd = Commander::new(tx, core.clone());
    let _bus = match desktop::serve(cmd.clone()) {
        Ok(conn) => Some(conn),
        Err(e) => {
            eprintln!("loki: D-Bus service unavailable ({e}); --toggle and window placement won't work");
            None
        }
    };
    desktop::spawn_tray(core.clone(), cmd.clone());
    desktop::spawn_shortcut(cmd.clone());
    if let Some(c) = initial {
        cmd.send(c);
    }
    ui::run(core.clone(), rx);
    desktop::unload_place_script();
    core.stop();
}
