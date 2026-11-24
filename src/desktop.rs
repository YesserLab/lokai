//! Plasma integration: single-instance D-Bus service, global shortcut (KGlobalAccel),
//! window placement on the cursor's screen (KWin script) and the tray menu (StatusNotifierItem).

use crate::core::{Conn, Core};
use eframe::egui;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;
use zbus::blocking::{Connection, Proxy};

pub const BUS_NAME: &str = "org.loki.LoKI";
const OBJ_PATH: &str = "/org/loki/LoKI";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    Toggle,
    ShowChat,
    ShowSettings,
    Quit,
}

/// Sends commands to the UI thread and wakes it up.
#[derive(Clone)]
pub struct Commander {
    tx: mpsc::Sender<Cmd>,
    core: Arc<Core>,
}

impl Commander {
    pub fn new(tx: mpsc::Sender<Cmd>, core: Arc<Core>) -> Self {
        Self { tx, core }
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
        self.core.notify();
    }
}

// ---------------------------------------------------------------- single instance

/// Result of the last placement attempt: 0 = pending, 1 = placed, 2 = window not found yet.
pub static PLACEMENT: AtomicU8 = AtomicU8::new(0);

struct Service {
    cmd: Commander,
}

#[zbus::interface(name = "org.loki.LoKI")]
impl Service {
    fn toggle(&self) {
        self.cmd.send(Cmd::Toggle);
    }
    fn show(&self) {
        self.cmd.send(Cmd::ShowChat);
    }
    fn settings(&self) {
        self.cmd.send(Cmd::ShowSettings);
    }
    fn quit(&self) {
        self.cmd.send(Cmd::Quit);
    }
    /// No-op, used to detect a running instance.
    fn ping(&self) {}
    /// Called back by the KWin placement script.
    fn placed(&self, found: bool) {
        PLACEMENT.store(if found { 1 } else { 2 }, Ordering::SeqCst);
        self.cmd.core.notify();
    }
}

/// Forwards `method` to an already running instance. Returns false if none is running.
pub fn forward(method: &str) -> bool {
    let Ok(conn) = Connection::session() else {
        return false;
    };
    conn.call_method(Some(BUS_NAME), OBJ_PATH, Some(BUS_NAME), method, &()).is_ok()
}

/// Claims the bus name and serves the control interface. Keep the connection alive.
pub fn serve(cmd: Commander) -> zbus::Result<Connection> {
    zbus::blocking::connection::Builder::session()?.name(BUS_NAME)?.serve_at(OBJ_PATH, Service { cmd })?.build()
}

// ---------------------------------------------------------------- global shortcut

const COMPONENT: &str = "loki";
// Qt key codes: Qt::MetaModifier | Qt::AltModifier | Qt::Key_Space
const DEFAULT_KEY: i32 = 0x1000_0000 | 0x0800_0000 | 0x20;

/// Registers "Toggle LoKI" with KGlobalAccel (default Meta+Alt+Space, changeable in
/// System Settings → Keyboard → Shortcuts) and toggles the launcher when it fires.
pub fn spawn_shortcut(cmd: Commander) {
    std::thread::Builder::new()
        .name("shortcut".into())
        .spawn(move || {
            if let Err(e) = shortcut_loop(&cmd) {
                eprintln!("loki: global shortcut unavailable: {e}");
            }
        })
        .unwrap();
}

fn shortcut_loop(cmd: &Commander) -> zbus::Result<()> {
    let conn = Connection::session()?;
    let accel = Proxy::new(&conn, "org.kde.kglobalaccel", "/kglobalaccel", "org.kde.KGlobalAccel")?;
    let id = vec![COMPONENT, "toggle", "LoKI", "Toggle LoKI"];
    accel.call_method("doRegister", &(&id,))?;
    // QKeySequence travels as (ai). Flags: 1 = IsDefault, 2 = SetPresent (keeps a user-changed key).
    let keys = vec![(vec![DEFAULT_KEY, 0, 0, 0],)];
    accel.call_method("setShortcutKeys", &(&id, &keys, 1u32))?;
    accel.call_method("setShortcutKeys", &(&id, &keys, 2u32))?;
    let path: zbus::zvariant::OwnedObjectPath = accel.call("getComponent", &(COMPONENT,))?;
    let component = Proxy::new(&conn, "org.kde.kglobalaccel", path, "org.kde.kglobalaccel.Component")?;
    for _ in component.receive_signal("globalShortcutPressed")? {
        cmd.send(Cmd::Toggle);
    }
    Ok(())
}

// ---------------------------------------------------------------- placement

const PLACE_SCRIPT: &str = r#"
(function () {
    const w = workspace.windowList().filter(c => c.resourceClass === "loki" || c.resourceName === "loki").pop();
    if (w) {
        const output = workspace.screenAt(workspace.cursorPos) || workspace.activeScreen;
        // Centered on the screen under the cursor; re-centered whenever the launcher changes size.
        const center = function () {
            const area = workspace.clientArea(KWin.MaximizeArea, output, workspace.currentDesktop);
            const g = w.frameGeometry;
            const x = Math.round(area.x + (area.width - g.width) / 2);
            const y = Math.round(area.y + (area.height - g.height) / 2);
            if (g.x !== x || g.y !== y) {
                w.frameGeometry = { x: x, y: y, width: g.width, height: g.height };
            }
        };
        center();
        w.frameGeometryChanged.connect(center);
        w.keepAbove = true;
        w.skipTaskbar = true;
        w.skipPager = true;
        w.skipSwitcher = true;
        workspace.activeWindow = w;
    }
    callDBus("org.loki.LoKI", "/org/loki/LoKI", "org.loki.LoKI", "Placed", !!w);
})();
"#;

const PLACE_PLUGIN: &str = "loki-place";

fn kwin_scripting(conn: &Connection) -> zbus::Result<Proxy<'_>> {
    Proxy::new(conn, "org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting")
}

/// Asks KWin to center the launcher on the screen under the cursor and focus it. The script stays
/// loaded (replaced on the next open) so it can keep the window centered when its height changes.
/// The result arrives asynchronously through [`PLACEMENT`]. Returns false if KWin is not available.
pub fn place_window() -> bool {
    let run = || -> zbus::Result<()> {
        let path =
            std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir).join("loki-place.js");
        std::fs::write(&path, PLACE_SCRIPT).map_err(|e| zbus::Error::Failure(e.to_string()))?;
        let conn = Connection::session()?;
        let scripting = kwin_scripting(&conn)?;
        let _: bool = scripting.call("unloadScript", &(PLACE_PLUGIN,))?;
        let id: i32 = scripting.call("loadScript", &(path.to_string_lossy().as_ref(), PLACE_PLUGIN))?;
        let script = Proxy::new(&conn, "org.kde.KWin", format!("/Scripting/Script{id}"), "org.kde.kwin.Script")?;
        script.call_method("run", &())?;
        Ok(())
    };
    PLACEMENT.store(0, Ordering::SeqCst);
    run().is_ok()
}

/// Removes the placement script from KWin (on exit).
pub fn unload_place_script() {
    if let Ok(conn) = Connection::session()
        && let Ok(scripting) = kwin_scripting(&conn)
    {
        let _: zbus::Result<bool> = scripting.call("unloadScript", &(PLACE_PLUGIN,));
    }
}

// ---------------------------------------------------------------- tray

struct Tray {
    core: Arc<Core>,
    cmd: Commander,
    art: TrayArt,
}

/// The tray icon as handed to Plasma: an icon name, looked up in our own icon directory first
/// (Plasma recolors monochrome SVGs that use the `ColorScheme-Text` class), plus pixmaps as fallback.
#[derive(Default)]
struct TrayArt {
    key: String,
    name: String,
    dir: String,
    pixmaps: Vec<ksni::Icon>,
}

const TRAY_SIZES: [u32; 4] = [22, 32, 48, 64];

/// Lucide's outline with Plasma's color-scheme stylesheet, so the panel can recolor it. Qt's SVG
/// renderer doesn't inherit `currentColor` from the root element (it draws black), so every shape
/// gets the `ColorScheme-Text` class and its own stroke, like Breeze's icons.
fn monochrome_svg(color: &str) -> String {
    let mut svg = crate::icons::Icon::Bot.svg().replacen(" class=\"lucide lucide-bot-message-square\"", "", 1);
    let end = svg.find('>').map_or(0, |i| i + 1);
    svg.insert_str(end, &format!("<style type=\"text/css\" id=\"current-color-scheme\">.ColorScheme-Text {{ color:{color}; }}</style>"));
    for tag in ["path", "circle", "rect", "line", "polyline", "polygon", "ellipse"] {
        svg = svg.replace(&format!("<{tag} "), &format!("<{tag} class=\"ColorScheme-Text\" style=\"fill:none;stroke:currentColor\" "));
    }
    svg
}

/// StatusNotifierItem pixmaps (ARGB32) at every tray size, from an RGBA renderer.
fn pixmaps(render: impl Fn(u32) -> Option<Vec<u8>>) -> Vec<ksni::Icon> {
    TRAY_SIZES
        .iter()
        .filter_map(|&size| Some(ksni::Icon { width: size as i32, height: size as i32, data: crate::theme::rgba_to_argb(&render(size)?) }))
        .collect()
}

impl TrayArt {
    fn key(cfg: &crate::config::Config) -> String {
        format!("{:?}|{}", cfg.tray_icon, cfg.tray_icon_custom.trim())
    }

    /// Plasma only finds icon files that were in our icon directory when the tray registered, so
    /// only the monochrome icon (which Plasma must recolor) goes by name, and its file is written
    /// on every start. The color logo and custom image files are sent as pixmaps, which Plasma
    /// redraws whenever they change.
    fn new(cfg: &crate::config::Config) -> Self {
        use crate::config::TrayIcon;
        let dir = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir).join("loki/tray");
        let mono = Self::write_monochrome(&dir);
        let logo = || pixmaps(|s| Some(crate::theme::icon_rgba(s)));
        let custom = cfg.tray_icon_custom.trim();
        let (name, pixmaps) = match cfg.tray_icon {
            TrayIcon::Custom if !custom.is_empty() => {
                let path = crate::config::expand_home(custom);
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
                if path.is_file() && matches!(ext.as_str(), "svg" | "png") {
                    let data = std::fs::read(&path).unwrap_or_default();
                    let images = if ext == "svg" {
                        pixmaps(|s| crate::theme::render_svg(&String::from_utf8_lossy(&data), s))
                    } else {
                        pixmaps(|s| crate::theme::render_png(&data, s))
                    };
                    (String::new(), if images.is_empty() { logo() } else { images })
                } else {
                    (custom.to_string(), logo()) // an icon from the icon theme, looked up by Plasma
                }
            }
            TrayIcon::Color => (String::new(), logo()),
            TrayIcon::Monochrome | TrayIcon::Custom => mono,
        };
        Self { key: Self::key(cfg), name, dir: dir.to_string_lossy().into_owned(), pixmaps }
    }

    /// Writes the monochrome SVG (named after its content, so Plasma never shows a stale cached
    /// copy) and removes older versions. Returns its icon name and fallback pixmaps.
    fn write_monochrome(dir: &std::path::Path) -> (String, Vec<ksni::Icon>) {
        use std::hash::{Hash, Hasher};
        let color = if crate::theme::Palette::detect().dark { "#fcfcfc" } else { "#232629" };
        let svg = monochrome_svg(color);
        let mut h = std::collections::hash_map::DefaultHasher::new();
        svg.hash(&mut h);
        let name = format!("loki-mono-{:x}", h.finish());
        let _ = std::fs::create_dir_all(dir);
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                if e.file_name().to_string_lossy() != format!("{name}.svg") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        let file = dir.join(format!("{name}.svg"));
        if !file.is_file() {
            let _ = std::fs::write(&file, svg.as_bytes());
        }
        (name, pixmaps(|s| crate::theme::render_svg(&svg, s)))
    }
}

fn conn_line(core: &Core) -> (String, &'static str) {
    let st = core.lock();
    match st.conn {
        Conn::Connected => (
            match st.latency_ms {
                Some(ms) => format!("Connected · {ms} ms"),
                None => "Connected".into(),
            },
            "network-connect",
        ),
        Conn::Checking | Conn::Unknown => ("Connecting…".into(), "network-connect"),
        Conn::Error => ("Server unreachable".into(), "network-disconnect"),
        Conn::Blocked => ("Blocked: not a local address".into(), "dialog-warning"),
    }
}

fn speed_text(core: &Core) -> String {
    let st = core.lock();
    if st.tps <= 0.0 {
        "—".into()
    } else if st.generating {
        format!("{:.1} tok/s (generating)", st.tps)
    } else {
        format!("{:.1} tok/s", st.tps)
    }
}

pub fn group_thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

impl ksni::Tray for Tray {
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        "loki".into()
    }

    fn title(&self) -> String {
        "LoKI".into()
    }

    fn icon_name(&self) -> String {
        self.art.name.clone()
    }

    fn icon_theme_path(&self) -> String {
        self.art.dir.clone()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.art.pixmaps.clone()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let (conn, _) = conn_line(&self.core);
        let st = self.core.lock();
        let mut description = format!("{conn}\n{}", if st.config.model.is_empty() { "No model selected" } else { &st.config.model });
        if st.busy {
            description.push_str(&format!("\n{}", if st.status.is_empty() { "Working…" } else { &st.status }));
        }
        ksni::ToolTip { title: "LoKI".into(), description, ..Default::default() }
    }

    fn secondary_activate(&mut self, _x: i32, _y: i32) {
        self.cmd.send(Cmd::Toggle);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::{MenuItem, StandardItem};
        let info = |label: String, icon: &str| -> MenuItem<Self> {
            StandardItem { label, icon_name: icon.into(), enabled: false, ..Default::default() }.into()
        };
        let action = |label: &str, icon: &str, f: fn(&mut Self)| -> MenuItem<Self> {
            StandardItem { label: label.into(), icon_name: icon.into(), activate: Box::new(f), ..Default::default() }.into()
        };
        let (conn, conn_icon) = conn_line(&self.core);
        let (model, context, busy, status) = {
            let st = self.core.lock();
            (st.config.model.clone(), st.context_tokens, st.busy, st.status.clone())
        };
        let mut items = vec![
            info(conn, conn_icon),
            info(format!("Model: {}", if model.is_empty() { "none selected" } else { &model }), ""),
            info(format!("Speed: {}", speed_text(&self.core)), ""),
            info(if context > 0 { format!("Context: {} tokens", group_thousands(context)) } else { "Context: empty session".into() }, ""),
        ];
        if busy {
            items.push(info(format!("Activity: {}", if status.is_empty() { "Working…" } else { &status }), ""));
        }
        items.extend([
            MenuItem::Separator,
            action("Open Chat", "dialog-messages", |t| t.cmd.send(Cmd::ShowChat)),
            action("Clear Session", "edit-clear-history", |t| t.core.new_chat()),
            action("Settings…", "configure", |t| t.cmd.send(Cmd::ShowSettings)),
            MenuItem::Separator,
            action("Exit", "application-exit", |t| t.cmd.send(Cmd::Quit)),
        ]);
        items
    }
}

/// Starts the tray icon and keeps its menu/tooltip in sync with the state (checked every second).
pub fn spawn_tray(core: Arc<Core>, cmd: Commander) {
    use ksni::blocking::TrayMethods;
    let art = TrayArt::new(&core.lock().config);
    let handle = match (Tray { core: core.clone(), cmd, art }).spawn() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("loki: tray icon unavailable: {e}");
            return;
        }
    };
    std::thread::Builder::new()
        .name("tray".into())
        .spawn(move || {
            let mut last = String::new();
            let mut art_key = TrayArt::key(&core.lock().config);
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let sig = {
                    let st = core.lock();
                    format!(
                        "{:?}{:?}{}{:.0}{}{}{}{}",
                        st.conn,
                        st.latency_ms,
                        st.config.model,
                        st.tps * 10.0,
                        st.generating,
                        st.context_tokens,
                        st.busy,
                        st.status
                    )
                };
                let cfg = core.lock().config.clone();
                let new_art = (TrayArt::key(&cfg) != art_key).then(|| TrayArt::new(&cfg));
                if let Some(art) = &new_art {
                    art_key = art.key.clone();
                }
                if sig != last || new_art.is_some() {
                    last = sig;
                    if handle
                        .update(|t| {
                            if let Some(art) = new_art {
                                t.art = art;
                            }
                        })
                        .is_none()
                    {
                        return;
                    }
                }
            }
        })
        .unwrap();
}

/// Opens a folder in the user's file manager.
pub fn open_folder(dir: &std::path::Path) {
    use std::process::{Command, Stdio};
    if let Ok(mut child) = Command::new("xdg-open").arg(dir).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        std::thread::spawn(move || child.wait());
    }
}

/// Hands copied text to `wl-copy`, which keeps serving it after the launcher window is gone
/// (a Wayland selection otherwise dies with the window that set it). Without `wl-copy`,
/// egui's own clipboard handles it.
pub struct WlCopy;

impl WlCopy {
    pub fn available() -> bool {
        std::env::var_os("WAYLAND_DISPLAY").is_some()
            && std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("wl-copy").is_file()))
    }

    fn copy(text: &str) -> bool {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let Ok(mut child) = Command::new("wl-copy").stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() else {
            return false;
        };
        let ok = child.stdin.take().is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
        // wl-copy forks to serve the selection; reap the parent without blocking the UI.
        std::thread::spawn(move || child.wait());
        ok
    }
}

impl egui::Plugin for WlCopy {
    fn debug_name(&self) -> &'static str {
        "wl-copy"
    }

    fn output_hook(&mut self, _ctx: &egui::Context, output: &mut egui::FullOutput) {
        output.platform_output.commands.retain(|cmd| match cmd {
            egui::OutputCommand::CopyText(text) => !Self::copy(text),
            _ => true,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_art() {
        let tmp = std::env::temp_dir().join(format!("loki-tray-{}", std::process::id()));
        // SAFETY: test-only; nothing else reads XDG_RUNTIME_DIR in this test binary.
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", &tmp) };
        let opaque = |art: &TrayArt| art.pixmaps.len() == TRAY_SIZES.len() && art.pixmaps[0].data.chunks(4).any(|c| c[0] > 200);
        let mut cfg = crate::config::Config::default();

        // Monochrome: an SVG file that Plasma recolors, every shape carrying the color class.
        let mono = TrayArt::new(&cfg);
        assert!(mono.name.starts_with("loki-mono-") && opaque(&mono));
        let svg = std::fs::read_to_string(tmp.join(format!("loki/tray/{}.svg", mono.name))).unwrap();
        assert!(svg.contains(r#"id="current-color-scheme""#));
        assert_eq!(
            svg.matches("<path ").count(),
            svg.matches(r#"<path class="ColorScheme-Text" style="fill:none;stroke:currentColor""#).count()
        );

        // Color and custom files: pixmaps only, and the monochrome file is still there for a switch back.
        cfg.tray_icon = crate::config::TrayIcon::Color;
        let color = TrayArt::new(&cfg);
        assert!(color.name.is_empty() && opaque(&color));
        assert!(tmp.join(format!("loki/tray/{}.svg", mono.name)).is_file());

        cfg.tray_icon = crate::config::TrayIcon::Custom;
        let svg_file = tmp.join("mine.svg");
        std::fs::write(&svg_file, crate::icons::Icon::Bot.svg().replace("currentColor", "#ff0000")).unwrap();
        cfg.tray_icon_custom = svg_file.to_string_lossy().into();
        let custom = TrayArt::new(&cfg);
        assert!(custom.name.is_empty() && opaque(&custom));
        assert!(custom.pixmaps[0].data.chunks(4).any(|c| c[0] > 200 && c[1] > 200 && c[2] < 50)); // red

        let png_file = tmp.join("mine.png");
        let mut png = resvg::tiny_skia::Pixmap::new(8, 8).unwrap();
        png.fill(resvg::tiny_skia::Color::from_rgba8(0, 0, 255, 255));
        png.save_png(&png_file).unwrap();
        cfg.tray_icon_custom = png_file.to_string_lossy().into();
        let custom = TrayArt::new(&cfg);
        assert!(custom.name.is_empty() && custom.pixmaps[0].data.chunks(4).all(|c| c == [255, 0, 0, 255])); // blue, opaque

        // An icon theme name goes to Plasma by name.
        cfg.tray_icon_custom = "utilities-terminal".into();
        assert_eq!(TrayArt::new(&cfg).name, "utilities-terminal");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Touches the real clipboard: `cargo test -- --ignored wl_copy_roundtrip`
    #[test]
    #[ignore]
    fn wl_copy_roundtrip() {
        assert!(WlCopy::available());
        let ctx = egui::Context::default();
        ctx.add_plugin(WlCopy);
        let mut out = ctx.run_ui(Default::default(), |ui| ui.ctx().copy_text("loki clipboard test".into()));
        out.textures_delta.clear();
        assert!(!out.platform_output.commands.iter().any(|c| matches!(c, egui::OutputCommand::CopyText(_))));
        std::thread::sleep(Duration::from_millis(300));
        let pasted = std::process::Command::new("wl-paste").arg("-n").output().unwrap();
        assert_eq!(String::from_utf8_lossy(&pasted.stdout), "loki clipboard test");
    }
}
