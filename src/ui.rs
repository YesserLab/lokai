//! The launcher window: conversation on top, prompt field below it, status footer at the bottom.
//! A fresh window is created on every open (Wayland can't re-show hidden windows) and
//! KWin centers it on the screen under the cursor.

use crate::config::{Approval, Config, TrayIcon, collapse_home, default_scratch_dir};
use crate::core::{Conn, Core, Decision, Item, State, ToolItem, ToolStatus};
use crate::desktop::{self, Cmd, PLACEMENT, group_thousands};
use crate::icons::{self, Icon};
use crate::theme::{self, Palette};
use eframe::egui::{
    self, Align, Button, Color32, ComboBox, CornerRadius, FontId, Frame, Id, Key, Label, Layout, Margin, Modifiers, Pos2, Rect, Response,
    RichText, ScrollArea, Sense, Stroke, StrokeKind, TextEdit, UiBuilder, Vec2, pos2, vec2,
};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

/// Transparent border around the panel (room for the drop shadow).
const MARGIN: f32 = 18.0;
const RADIUS: u8 = 18;
const PAD: Vec2 = vec2(18.0, 14.0);
/// Panel height when there is no conversation yet.
const COMPACT_HEIGHT: f32 = 176.0;
const PROMPT_MARGIN: Margin = Margin { left: 10, right: 5, top: 5, bottom: 5 };
const GAP: f32 = 10.0;
/// How long Esc has to be held to stop a running reply (a tap closes the window).
const ESC_HOLD: f64 = 0.6;

const SUGGESTIONS: [&str; 3] = ["What's filling up my home folder?", "Any errors since boot?", "Which packages can be updated?"];

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Chat,
    Settings,
}

#[derive(PartialEq, Eq)]
enum Outcome {
    Closed,
    Quit,
}

enum Action {
    Send(String),
    Stop,
    NewChat,
    Resolve(String, Decision),
    SetModel(String),
    SetApproval(Approval),
    OpenSettings,
    CloseSettings,
    SaveSettings,
    TestConnection,
    OpenScratch,
    Close,
}

type TestResult = Arc<Mutex<Option<Result<(Vec<String>, u128), String>>>>;

/// Runs the window loop on the main thread until Exit.
pub fn run(core: Arc<Core>, rx: mpsc::Receiver<Cmd>) {
    loop {
        let view = match rx.recv() {
            Ok(Cmd::Toggle | Cmd::ShowChat) => View::Chat,
            Ok(Cmd::ShowSettings) => View::Settings,
            Ok(Cmd::Quit) | Err(_) => return,
        };
        if open(&core, &rx, view) == Outcome::Quit {
            return;
        }
    }
}

fn panel_height(view: View, st: &State) -> f32 {
    if view == View::Chat && st.items.is_empty() { COMPACT_HEIGHT } else { st.config.height.max(COMPACT_HEIGHT + 100.0) }
}

fn open(core: &Arc<Core>, rx: &mpsc::Receiver<Cmd>, view: View) -> Outcome {
    let (width, height) = {
        let st = core.lock();
        (st.config.width.max(420.0), panel_height(view, &st))
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("LoKAI")
            .with_app_id("lokai")
            .with_inner_size([width + 2.0 * MARGIN, height + 2.0 * MARGIN])
            .with_decorations(false)
            .with_transparent(true)
            .with_resizable(false)
            .with_always_on_top()
            .with_icon(Arc::new(egui::IconData { rgba: theme::icon_rgba(64), width: 64, height: 64 })),
        centered: true,
        run_and_return: true,
        persist_window: false,
        ..Default::default()
    };
    let mut outcome = Outcome::Closed;
    let result = eframe::run_native(
        "lokai",
        options,
        Box::new(|cc| {
            theme::install_fonts(&cc.egui_ctx);
            egui_extras::install_image_loaders(&cc.egui_ctx);
            if desktop::WlCopy::available() {
                cc.egui_ctx.add_plugin(desktop::WlCopy);
            }
            let palette = Palette::detect();
            palette.apply(&cc.egui_ctx);
            core.set_repaint(Some(cc.egui_ctx.clone()));
            let logo = cc.egui_ctx.load_texture(
                "logo",
                egui::ColorImage::from_rgba_unmultiplied([64, 64], &theme::icon_rgba(64)),
                Default::default(),
            );
            let mut app = Launcher {
                core: core.clone(),
                rx,
                outcome: &mut outcome,
                p: palette,
                view: View::Chat,
                input: String::new(),
                md: CommonMarkCache::default(),
                logo,
                frame: 0,
                opened: Instant::now(),
                placement: Placement::Pending { at: 2, tries: 0 },
                had_focus: false,
                focus_input: true,
                requested_height: height,
                width,
                draft: None,
                test: Arc::new(Mutex::new(None)),
                testing: false,
                expanded: HashSet::new(),
                reasoning_open: HashSet::new(),
                copied: None,
                esc_hold: None,
                images: HashMap::new(),
                math: Default::default(),
                sent_history: Vec::new(),
            };
            if view == View::Settings {
                app.open_settings();
            }
            core.check_connection();
            Ok(Box::new(app))
        }),
    );
    core.set_repaint(None);
    if let Err(e) = result {
        eprintln!("lokai: cannot open window: {e}");
    }
    outcome
}

enum Placement {
    /// Ask KWin once the window has been mapped (frame number), after `tries` failed attempts.
    Pending {
        at: u64,
        tries: u32,
    },
    Waiting {
        since: Instant,
        tries: u32,
    },
    Done,
}

struct Launcher<'a> {
    core: Arc<Core>,
    rx: &'a mpsc::Receiver<Cmd>,
    outcome: &'a mut Outcome,
    p: Palette,
    view: View,
    input: String,
    md: CommonMarkCache,
    logo: egui::TextureHandle,
    frame: u64,
    opened: Instant,
    placement: Placement,
    had_focus: bool,
    focus_input: bool,
    requested_height: f32,
    width: f32,
    draft: Option<Config>,
    test: TestResult,
    testing: bool,
    expanded: HashSet<String>,
    reasoning_open: HashSet<usize>,
    /// The copy button that was clicked last, and when.
    copied: Option<(Id, f64)>,
    /// Esc went down while a reply was running: when, and whether holding it already stopped it.
    esc_hold: Option<(f64, bool)>,
    /// Images shown in the chat: file → (modification time, uri, bytes, width / height).
    /// Reloaded when the file changes.
    images: HashMap<PathBuf, (std::time::SystemTime, String, egui::load::Bytes, f32)>,
    /// Typeset LaTeX formulas.
    math: std::rc::Rc<crate::math::MathCache>,
    sent_history: Vec<String>,
}

impl Launcher<'_> {
    fn open_settings(&mut self) {
        self.draft = Some(self.core.lock().config.clone());
        *self.test.lock().unwrap() = None;
        self.view = View::Settings;
    }

    fn close(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn handle_commands(&mut self, ctx: &egui::Context) {
        while let Ok(cmd) = self.rx.try_recv() {
            match cmd {
                Cmd::Toggle => self.close(ctx),
                Cmd::ShowChat => {
                    self.view = View::Chat;
                    self.focus_input = true;
                    std::thread::spawn(desktop::place_window);
                }
                Cmd::ShowSettings => {
                    self.open_settings();
                    std::thread::spawn(desktop::place_window);
                }
                Cmd::Quit => {
                    *self.outcome = Outcome::Quit;
                    self.close(ctx);
                }
            }
        }
    }

    /// Drives the KWin placement; content stays invisible until the window sits in its final spot.
    fn update_placement(&mut self, ctx: &egui::Context) -> bool {
        match self.placement {
            Placement::Done => return true,
            Placement::Pending { at, tries } if self.frame >= at => {
                PLACEMENT.store(0, Ordering::SeqCst);
                std::thread::spawn(|| {
                    if !desktop::place_window() {
                        PLACEMENT.store(3, Ordering::SeqCst); // not running under KWin
                    }
                });
                self.placement = Placement::Waiting { since: Instant::now(), tries };
            }
            Placement::Waiting { since, tries } => match PLACEMENT.load(Ordering::SeqCst) {
                1 | 3 => self.placement = Placement::Done,
                2 if tries < 15 => self.placement = Placement::Pending { at: self.frame + 2, tries: tries + 1 },
                _ if since.elapsed() > Duration::from_millis(700) => self.placement = Placement::Done,
                _ => {}
            },
            _ => {}
        }
        if self.opened.elapsed() > Duration::from_millis(900) {
            self.placement = Placement::Done;
        }
        ctx.request_repaint_after(Duration::from_millis(16));
        matches!(self.placement, Placement::Done)
    }

    fn apply(&mut self, ctx: &egui::Context, actions: Vec<Action>) {
        for a in actions {
            match a {
                Action::Send(text) => {
                    if self.core.send(&text) {
                        if self.sent_history.last() != Some(&text) {
                            self.sent_history.push(text.clone());
                        }
                        if self.input.trim() == text.trim() {
                            self.input.clear();
                        }
                    }
                }
                Action::Stop => {
                    self.core.stop();
                    self.focus_input = true; // ready to type the next message
                }
                Action::NewChat => {
                    self.core.new_chat();
                    self.expanded.clear();
                    self.reasoning_open.clear();
                    self.focus_input = true;
                }
                Action::Resolve(id, d) => self.core.resolve(&id, d),
                Action::SetModel(m) => self.core.set_model(&m),
                Action::SetApproval(a) => self.core.set_approval(a),
                Action::OpenSettings => self.open_settings(),
                Action::CloseSettings => {
                    self.view = View::Chat;
                    self.draft = None;
                    self.focus_input = true;
                }
                Action::SaveSettings => {
                    if let Some(cfg) = self.draft.take()
                        && let Err(e) = self.core.set_config(cfg)
                    {
                        eprintln!("lokai: cannot save settings: {e}");
                    }
                    self.view = View::Chat;
                    self.focus_input = true;
                }
                Action::OpenScratch => desktop::open_path(&self.core.lock().config.ensure_scratch_dir()),
                Action::TestConnection => {
                    if let Some(cfg) = self.draft.clone() {
                        self.testing = true;
                        let slot = self.test.clone();
                        let ctx = ctx.clone();
                        *slot.lock().unwrap() = None;
                        std::thread::spawn(move || {
                            let r = match cfg.policy_error() {
                                Some(e) => Err(e),
                                None => crate::api::Client::new(&cfg).models().map(|(m, d)| (m, d.as_millis())),
                            };
                            *slot.lock().unwrap() = Some(r);
                            ctx.request_repaint();
                        });
                    }
                }
                Action::Close => self.close(ctx),
            }
        }
    }
}

impl eframe::App for Launcher<'_> {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0; 4]
    }

    /// Runs even when the compositor withholds frames (e.g. behind the lock screen),
    /// so commands like Exit and Toggle always get through.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_commands(ctx);
        // Launcher-like: hide when another window gets focus.
        let focused = ctx.input(|i| i.viewport().focused).unwrap_or(true);
        if focused {
            self.had_focus = true;
        } else if self.had_focus && self.view == View::Chat && self.core.lock().config.hide_on_focus_loss {
            self.close(ctx);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.frame += 1;
        if !self.update_placement(&ctx) {
            return; // stay transparent until KWin has moved us
        }
        self.draw(ui);
    }
}

impl Launcher<'_> {
    fn draw(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let core = self.core.clone();
        let st = core.lock();
        let mut actions = Vec::new();

        // Window height follows the content (compact bar vs. full conversation).
        let height = panel_height(self.view, &st);
        if (height - self.requested_height).abs() > 0.5 {
            self.requested_height = height;
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(vec2(self.width + 2.0 * MARGIN, height + 2.0 * MARGIN)));
        }

        // Keyboard shortcuts.
        ctx.input_mut(|i| {
            // Esc closes the window; a running reply goes on in the background. Holding Esc while
            // a reply runs stops it instead (and then letting go doesn't close the window).
            if i.consume_key(Modifiers::NONE, Key::Escape) && self.esc_hold.is_none() {
                if st.busy {
                    self.esc_hold = Some((i.time, false));
                } else {
                    actions.push(Action::Close);
                }
            }
            if let Some((since, stopped)) = self.esc_hold {
                if !i.key_down(Key::Escape) {
                    self.esc_hold = None;
                    if !stopped {
                        actions.push(Action::Close);
                    }
                } else if !stopped && i.time - since >= ESC_HOLD {
                    self.esc_hold = Some((since, true));
                    actions.push(Action::Stop);
                }
            }
            if i.consume_key(Modifiers::COMMAND, Key::N) {
                actions.push(Action::NewChat);
            }
            if i.consume_key(Modifiers::COMMAND, Key::Comma) {
                actions.push(Action::OpenSettings);
            }
        });

        // Panel with shadow.
        let p = self.p;
        let panel = ui.max_rect().shrink(MARGIN);
        let painter = ui.painter().clone();
        painter.add(
            egui::Shadow { offset: [0, 6], blur: 16, spread: 0, color: Color32::from_black_alpha(if p.dark { 150 } else { 55 }) }
                .as_shape(panel, CornerRadius::same(RADIUS)),
        );
        painter.rect(panel, CornerRadius::same(RADIUS), p.bg, Stroke::new(1.0, p.border), StrokeKind::Inside);
        let inner = panel.shrink2(PAD);
        let mut ui = ui.new_child(UiBuilder::new().max_rect(inner).layout(Layout::top_down(Align::Min)));
        ui.set_clip_rect(panel);

        // Chat log on top, then the prompt field, then the status/model bar. Laid out from the
        // bottom so the log gets exactly the space that is left.
        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            self.footer(ui, &st, &mut actions);
            ui.add_space(GAP);
            if self.view == View::Chat {
                self.prompt_bar(ui, &st, &mut actions);
                ui.add_space(GAP);
            }
            let body_h = ui.available_height();
            ui.allocate_ui_with_layout(vec2(ui.available_width(), body_h), Layout::top_down(Align::Min), |ui| {
                ui.set_min_height(body_h);
                match self.view {
                    View::Settings => self.settings(ui, &st, &mut actions),
                    View::Chat if st.items.is_empty() => self.empty_state(ui, &st, &mut actions),
                    View::Chat => self.conversation(ui, &st, &mut actions),
                }
            });
        });

        drop(st);
        self.apply(&ctx, actions);
        if self.testing && self.test.lock().unwrap().is_some() {
            self.testing = false;
        }
    }
}

// ---------------------------------------------------------------- pieces

/// A colored status dot followed by small text. The dot is centered on the text's digits and
/// lowercase letters (measured from the baseline), not on the line box, which includes descender space.
fn status_label(ui: &mut egui::Ui, dot: Color32, text: String, color: Color32) -> Response {
    const R: f32 = 3.5;
    let font = egui::TextStyle::Small.resolve(ui.style());
    let galley = ui.painter().layout_no_wrap(text, font.clone(), color);
    let (rect, resp) = ui.allocate_exact_size(vec2(2.0 * R + 6.0 + galley.size().x, galley.size().y), Sense::hover());
    let baseline = galley.rows.first().and_then(|r| r.row.glyphs.first().map(|g| r.pos.y + g.pos.y)).unwrap_or(galley.size().y * 0.75);
    let cy = rect.top() + baseline - font.size * 0.33;
    ui.painter().circle_filled(pos2(rect.left() + R, cy), R, dot);
    ui.painter().galley(pos2(rect.left() + 2.0 * R + 6.0, rect.top()), galley, color);
    resp
}

fn chevron(ui: &mut egui::Ui, open: bool, color: Color32) {
    icons::show(ui, if open { Icon::ChevronDown } else { Icon::ChevronRight }, 14.0, color);
}

/// Combo box with a Lucide chevron.
fn combo(id: &str) -> ComboBox<'_> {
    ComboBox::from_id_salt(id).icon(icons::combo_chevron)
}

/// Checkbox drawn with a Lucide check mark.
fn check(ui: &mut egui::Ui, p: &Palette, value: &mut bool, text: &str) -> Response {
    let font = egui::TextStyle::Body.resolve(ui.style());
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, p.text);
    let (rect, mut resp) = ui.allocate_exact_size(vec2(26.0 + galley.size().x, galley.size().y.max(18.0)), Sense::click());
    if resp.clicked() {
        *value = !*value;
        resp.mark_changed();
    }
    let b = Rect::from_min_size(pos2(rect.left(), rect.center().y - 9.0), vec2(18.0, 18.0));
    if *value {
        ui.painter().rect_filled(b, 5.0, if resp.hovered() { p.tint(p.accent, 225) } else { p.accent });
        icons::paint(ui, b.shrink(2.5), Icon::Check, p.on_accent);
    } else {
        ui.painter().rect(b, 5.0, p.surface2, Stroke::new(1.0, if resp.hovered() { p.muted } else { p.border }), StrokeKind::Inside);
    }
    ui.painter().galley(pos2(b.right() + 8.0, rect.center().y - galley.size().y / 2.0), galley, p.text);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Circular icon button (send / stop).
fn round_button(ui: &mut egui::Ui, p: &Palette, enabled: bool, stop: bool, tip: &str) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(34.0, 34.0), if enabled { Sense::click() } else { Sense::hover() });
    let fill = match (enabled, resp.hovered(), stop) {
        (false, _, _) => p.surface2,
        (true, true, true) => p.tint(p.err, 230),
        (true, false, true) => p.err,
        (true, true, false) => p.tint(p.accent, 225),
        (true, false, false) => p.accent,
    };
    let fg = if enabled { p.on_accent } else { p.faint };
    ui.painter().circle_filled(rect.center(), 17.0, fill);
    let (icon, size) = if stop { (Icon::Square, 15.0) } else { (Icon::ArrowUp, 19.0) };
    icons::paint(ui, Rect::from_center_size(rect.center(), Vec2::splat(size)), icon, fg);
    resp.on_hover_text(tip)
}

fn code_block(ui: &mut egui::Ui, p: &Palette, id: impl std::hash::Hash + std::fmt::Debug, text: &str, color: Color32, max_h: f32) {
    Frame::new().fill(p.surface2).corner_radius(8).inner_margin(Margin::symmetric(10, 8)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ScrollArea::vertical().id_salt(id).max_height(max_h).show(ui, |ui| {
            ui.add(Label::new(RichText::new(text).monospace().size(12.5).color(color)).wrap().selectable(true));
        });
    });
}

fn tool_title(name: &str) -> &str {
    match name {
        "run_command" => "Run",
        "read_file" => "Read",
        "write_file" => "Write",
        "edit_file" => "Edit",
        "list_directory" => "List",
        "search_files" => "Search",
        other => other,
    }
}

impl Launcher<'_> {
    fn prompt_bar(&mut self, ui: &mut egui::Ui, st: &State, actions: &mut Vec<Action>) {
        let p = self.p;
        let id = Id::unique("prompt");
        let has_focus = ui.memory(|m| m.has_focus(id));
        let border = if has_focus { p.tint(p.accent, 170) } else { p.border };
        Frame::new().fill(p.surface).stroke(Stroke::new(1.0, border)).corner_radius(14).inner_margin(PROMPT_MARGIN).show(ui, |ui| {
            ui.allocate_ui_with_layout(vec2(ui.available_width(), 36.0), Layout::left_to_right(Align::Center), |ui| {
                ui.add(egui::Image::new(&self.logo).fit_to_exact_size(vec2(24.0, 24.0)));
                ui.add_space(6.0);
                let approval = st.pending_approval();
                let enter = has_focus && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));
                // Up/Down recall previously sent prompts when the field is empty or showing one.
                if has_focus && !self.sent_history.is_empty() && (self.input.is_empty() || self.sent_history.contains(&self.input)) {
                    let pos = self.sent_history.iter().position(|h| *h == self.input);
                    if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp)) {
                        let i = pos.map_or(self.sent_history.len() - 1, |i| i.saturating_sub(1));
                        self.input = self.sent_history[i].clone();
                    } else if pos.is_some() && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowDown)) {
                        let i = pos.unwrap() + 1;
                        self.input = self.sent_history.get(i).cloned().unwrap_or_default();
                    }
                }
                let hint = if approval.is_some() {
                    "Press Enter to allow the pending tool call…"
                } else if st.busy {
                    "Working… (hold Esc to stop)"
                } else {
                    "Ask anything…"
                };
                let width = ui.available_width() - 34.0 - 10.0;
                let resp = ui.add(
                    TextEdit::multiline(&mut self.input)
                        .id(id)
                        .frame(Frame::NONE)
                        .desired_rows(1)
                        .desired_width(width)
                        .font(FontId::proportional(18.0))
                        .margin(Margin::symmetric(0, 4))
                        .hint_text(RichText::new(hint).color(p.faint).size(18.0)),
                );
                if self.focus_input {
                    resp.request_focus();
                    self.focus_input = false;
                }
                if enter {
                    match approval {
                        Some(id) if self.input.trim().is_empty() => actions.push(Action::Resolve(id, Decision::Allow)),
                        _ if !st.busy && !self.input.trim().is_empty() => {
                            actions.push(Action::Send(self.input.clone()));
                        }
                        _ => {}
                    }
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if st.busy {
                        let stop = round_button(ui, &p, true, true, "Stop (hold Esc)");
                        if stop.clicked() {
                            actions.push(Action::Stop);
                        }
                        // While Esc is held, a ring fills around the stop button.
                        if let Some((since, false)) = self.esc_hold {
                            let t = ((ui.input(|i| i.time) - since) / ESC_HOLD).clamp(0.0, 1.0) as f32;
                            let (c, r) = (stop.rect.center(), 19.5);
                            let points: Vec<Pos2> = (0..=48)
                                .map(|k| {
                                    let a = -std::f32::consts::FRAC_PI_2 + std::f32::consts::TAU * t * k as f32 / 48.0;
                                    c + vec2(a.cos(), a.sin()) * r
                                })
                                .collect();
                            ui.painter().add(egui::Shape::line(points, Stroke::new(2.5, p.err)));
                            ui.ctx().request_repaint();
                        }
                    } else if round_button(ui, &p, !self.input.trim().is_empty(), false, "Send (Enter)").clicked() {
                        actions.push(Action::Send(self.input.clone()));
                    }
                });
            });
        });
    }

    fn empty_state(&mut self, ui: &mut egui::Ui, st: &State, actions: &mut Vec<Action>) {
        let p = self.p;
        match st.conn {
            Conn::Error | Conn::Blocked => {
                ui.horizontal(|ui| {
                    icons::show(ui, Icon::Warning, 16.0, p.warn);
                    ui.add(Label::new(RichText::new(&st.conn_detail).color(p.muted)).truncate());
                });
                if ui.add(Button::new("Open settings").corner_radius(14)).clicked() {
                    actions.push(Action::OpenSettings);
                }
            }
            _ => {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
                    for s in SUGGESTIONS {
                        let b = Button::new(RichText::new(s).small().color(p.muted))
                            .fill(p.surface)
                            .stroke(Stroke::new(1.0, p.border))
                            .corner_radius(14);
                        if ui.add(b).clicked() {
                            actions.push(Action::Send(s.to_string()));
                        }
                    }
                });
            }
        }
    }

    fn conversation(&mut self, ui: &mut egui::Ui, st: &State, actions: &mut Vec<Action>) {
        let p = self.p;
        ScrollArea::vertical().id_salt("conversation").auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
            ui.set_width(ui.available_width() - 8.0);
            ui.spacing_mut().item_spacing.y = 10.0;
            for (idx, item) in st.items.iter().enumerate() {
                match item {
                    Item::User { text } => {
                        ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                            Frame::new()
                                .fill(p.user_bubble)
                                .corner_radius(CornerRadius { nw: 14, ne: 14, sw: 14, se: 4 })
                                .inner_margin(Margin::symmetric(12, 8))
                                .show(ui, |ui| {
                                    ui.set_max_width(ui.available_width() * 0.82);
                                    ui.add(Label::new(RichText::new(text).color(p.text)).wrap().selectable(true));
                                });
                        });
                    }
                    Item::Assistant { text, reasoning, streaming } => self.assistant(ui, idx, text, reasoning, *streaming),
                    Item::Tool(t) if t.name == "show_image" && t.status == ToolStatus::Done => self.image_block(ui, t),
                    Item::Tool(t) => self.tool_card(ui, t, actions),
                    Item::Error { text } => {
                        Frame::new()
                            .fill(p.tint(p.err, 26))
                            .stroke(Stroke::new(1.0, p.tint(p.err, 110)))
                            .corner_radius(10)
                            .inner_margin(Margin::symmetric(12, 8))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.horizontal_top(|ui| {
                                    icons::show(ui, Icon::Error, 16.0, p.err);
                                    ui.add(Label::new(RichText::new(text).color(p.err)).wrap().selectable(true));
                                });
                            });
                    }
                    Item::Info { text } => {
                        ui.vertical_centered(|ui| ui.label(RichText::new(text).small().color(p.muted)));
                    }
                }
            }
            if st.busy && !matches!(st.items.last(), Some(Item::Assistant { streaming: true, .. })) {
                ui.horizontal(|ui| {
                    icons::spinner(ui, 13.0, p.muted);
                    ui.label(RichText::new(&st.status).small().color(p.muted));
                });
            }
        });
    }

    fn assistant(&mut self, ui: &mut egui::Ui, idx: usize, text: &str, reasoning: &str, streaming: bool) {
        let p = self.p;
        if !reasoning.is_empty() {
            let open = self.reasoning_open.contains(&idx);
            let thinking = streaming && text.is_empty();
            let r = ui
                .horizontal(|ui| {
                    chevron(ui, open, p.faint);
                    if thinking {
                        icons::spinner(ui, 12.0, p.muted);
                    }
                    ui.label(RichText::new(if thinking { "Thinking" } else { "Thought process" }).small().color(p.muted));
                })
                .response
                .interact(Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            if r.clicked() && !self.reasoning_open.remove(&idx) {
                self.reasoning_open.insert(idx);
            }
            if open {
                Frame::new().inner_margin(Margin { left: 14, right: 0, top: 0, bottom: 2 }).show(ui, |ui| {
                    ui.add(Label::new(RichText::new(reasoning).small().italics().color(p.muted)).wrap().selectable(true));
                });
            }
        }
        if text.is_empty() {
            if streaming && reasoning.is_empty() {
                // three pulsing dots
                let t = ui.input(|i| i.time) as f32;
                let (rect, _) = ui.allocate_exact_size(vec2(36.0, 14.0), Sense::hover());
                for k in 0..3 {
                    let a = ((t * 4.0 - k as f32 * 0.7).sin() * 0.5 + 0.5) * 0.7 + 0.3;
                    ui.painter().circle_filled(
                        Pos2::new(rect.left() + 6.0 + k as f32 * 11.0, rect.center().y),
                        3.5,
                        p.tint(p.muted, (a * 255.0) as u8),
                    );
                }
                ui.ctx().request_repaint();
            }
            return;
        }
        for (k, segment) in split_fences(text).into_iter().enumerate() {
            match segment {
                Segment::Markdown(md) => {
                    let (math, p) = (self.math.clone(), self.p);
                    let render_math = move |ui: &mut egui::Ui, tex: &str, inline: bool| math.show(ui, tex, inline, p.text, p.muted);
                    CommonMarkViewer::new().render_math_fn(Some(&render_math)).show_with_id(
                        Id::unique(("md", idx, k)),
                        ui,
                        &mut self.md,
                        &crate::math::normalize_delimiters(md),
                    );
                }
                Segment::Code { lang, code } => self.code_snippet(ui, Id::unique(("code", idx, k)), lang, &code),
            }
        }
    }

    fn recently_copied(&self, ui: &egui::Ui, id: Id) -> bool {
        let now = ui.input(|i| i.time);
        let copied = self.copied.is_some_and(|(i, t)| i == id && now - t < 1.5);
        if copied {
            ui.ctx().request_repaint_after(Duration::from_millis(200));
        }
        copied
    }

    /// Lucide copy icon that turns into a green check for a moment after a click.
    fn copy_button(&mut self, ui: &mut egui::Ui, id: Id, text: &str, hover_bg: Color32) {
        let p = self.p;
        let copied = self.recently_copied(ui, id);
        let (rect, resp) = ui.allocate_exact_size(vec2(24.0, 22.0), Sense::click());
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, if copied { "Copied" } else { "Copy" }));
        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        let color = if copied {
            p.ok
        } else if resp.hovered() {
            p.text
        } else {
            p.muted
        };
        if resp.hovered() {
            ui.painter().rect_filled(rect, 6.0, hover_bg);
        }
        icons::paint(ui, Rect::from_center_size(rect.center(), Vec2::splat(14.0)), if copied { Icon::Check } else { Icon::Copy }, color);
        if resp.clicked() {
            ui.ctx().copy_text(text.to_string());
            self.copied = Some((id, ui.input(|i| i.time)));
        }
    }

    /// A fenced code block: language label, Lucide copy button, highlighted code.
    fn code_snippet(&mut self, ui: &mut egui::Ui, id: Id, lang: &str, code: &str) {
        let p = self.p;
        ui.add_space(2.0);
        Frame::new()
            .fill(p.surface2)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(8)
            .inner_margin(Margin { left: 10, right: 4, top: 3, bottom: 8 })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(if lang.is_empty() { "code" } else { lang }).small().color(p.faint));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        self.copy_button(ui, id, code, p.surface);
                    });
                });
                let theme = egui_extras::syntax_highlighting::CodeTheme::from_style(ui.style());
                let job = egui_extras::syntax_highlighting::highlight(ui.ctx(), ui.style(), &theme, code, lang);
                ScrollArea::horizontal().id_salt(id).show(ui, |ui| {
                    ui.add(Label::new(job).selectable(true).extend());
                });
            });
        ui.add_space(2.0);
    }

    /// The image a `show_image` call displayed: fitted to the width, click to open it in a viewer.
    fn image_block(&mut self, ui: &mut egui::Ui, t: &ToolItem) {
        let p = self.p;
        let path = PathBuf::from(&t.detail);
        let Some((source, aspect)) = self.image_source(ui.ctx(), &path) else {
            ui.horizontal(|ui| {
                icons::show(ui, Icon::Warning, 14.0, p.faint);
                ui.label(RichText::new(format!("Image no longer available: {}", collapse_home(&path))).small().color(p.faint));
            });
            return;
        };
        // As wide as the chat; only a picture that would then be taller than the visible part of
        // the chat is scaled down, so it can always be seen whole.
        let width = ui.available_width();
        let max_height = (ui.clip_rect().height() - 90.0).max(160.0); // room for the caption
        let size = if width / aspect > max_height { vec2(max_height * aspect, max_height) } else { vec2(width, width / aspect) };
        let image = egui::Image::new(source).fit_to_exact_size(size).corner_radius(8).sense(Sense::click());
        let resp = ui
            .add(image)
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text(format!("{} · click to open", collapse_home(&path)));
        if resp.clicked() {
            desktop::open_path(&path);
        }
        if !t.summary.is_empty() {
            ui.add_space(-4.0);
            ui.label(RichText::new(&t.summary).small().color(p.muted));
        }
    }

    /// Bytes of an image file for egui and its width/height ratio, cached until the file changes
    /// (a redrawn plot shows up).
    fn image_source(&mut self, ctx: &egui::Context, path: &Path) -> Option<(egui::ImageSource<'static>, f32)> {
        let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
        if let Some((m, uri, bytes, aspect)) = self.images.get(path)
            && *m == modified
        {
            return Some((egui::ImageSource::Bytes { uri: uri.clone().into(), bytes: bytes.clone() }, *aspect));
        }
        let data = std::fs::read(path).ok()?;
        let (w, h) = theme::image_size(&data)?;
        let aspect = w / h;
        let ext = match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
            Some("svg") => "svg",
            Some("jpg" | "jpeg") => "jpg",
            _ => "png",
        };
        let stamp = modified.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let uri = format!("bytes://lokai-image/{:x}-{stamp}.{ext}", {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            path.hash(&mut h);
            h.finish()
        });
        let bytes = egui::load::Bytes::from(data);
        if let Some((_, old, _, _)) = self.images.insert(path.to_path_buf(), (modified, uri.clone(), bytes.clone(), aspect)) {
            ctx.forget_image(&old);
        }
        Some((egui::ImageSource::Bytes { uri: uri.into(), bytes }, aspect))
    }

    fn tool_card(&mut self, ui: &mut egui::Ui, t: &ToolItem, actions: &mut Vec<Action>) {
        let p = self.p;
        let approval = t.status == ToolStatus::Approval;
        let open = approval || self.expanded.contains(&t.id);
        let border = match t.status {
            ToolStatus::Approval => p.tint(p.warn, 160),
            ToolStatus::Error => p.tint(p.err, 110),
            _ => p.border,
        };
        Frame::new().fill(p.surface).stroke(Stroke::new(1.0, border)).corner_radius(10).inner_margin(Margin::symmetric(10, 7)).show(
            ui,
            |ui| {
                ui.set_width(ui.available_width());
                let header = ui.horizontal(|ui| {
                    match t.status {
                        ToolStatus::Running => {
                            icons::spinner(ui, 14.0, p.accent);
                        }
                        ToolStatus::Done => {
                            icons::show(ui, Icon::Check, 14.0, p.ok);
                        }
                        ToolStatus::Error => {
                            icons::show(ui, Icon::X, 14.0, p.err);
                        }
                        ToolStatus::Denied => {
                            icons::show(ui, Icon::Ban, 14.0, p.faint);
                        }
                        ToolStatus::Approval => {
                            icons::show(ui, Icon::Warning, 14.0, p.warn);
                        }
                    }
                    ui.label(RichText::new(tool_title(&t.name)).strong().color(p.text));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        chevron(ui, open, p.faint);
                        ui.add(Label::new(RichText::new(&t.summary).monospace().size(12.5).color(p.muted)).truncate());
                    });
                });
                let click = ui
                    .interact(header.response.rect, Id::unique(("tool", &t.id)), Sense::click())
                    .on_hover_cursor(egui::CursorIcon::PointingHand);
                if click.clicked() && !approval && !self.expanded.remove(&t.id) {
                    self.expanded.insert(t.id.clone());
                }
                if open {
                    if !t.detail.is_empty() {
                        code_block(ui, &p, ("detail", &t.id), &t.detail, p.text, 180.0);
                    }
                    if !t.output.is_empty() {
                        let color = if t.status == ToolStatus::Error { p.err } else { p.text };
                        code_block(ui, &p, ("output", &t.id), &t.output, color, 220.0);
                    }
                }
                if approval {
                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Allow this?").color(p.warn));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.add(Button::new(RichText::new("Deny").color(p.text)).corner_radius(8)).clicked() {
                                actions.push(Action::Resolve(t.id.clone(), Decision::Deny));
                            }
                            if ui
                                .add(
                                    Button::new(RichText::new(format!("Always allow {}", tool_title(&t.name))).color(p.text))
                                        .corner_radius(8),
                                )
                                .on_hover_text("Don't ask again for this tool until you start a new chat")
                                .clicked()
                            {
                                actions.push(Action::Resolve(t.id.clone(), Decision::Always));
                            }
                            if ui
                                .add(Button::new(RichText::new("Allow").color(p.on_accent)).fill(p.accent).corner_radius(8))
                                .on_hover_text("Enter")
                                .clicked()
                            {
                                actions.push(Action::Resolve(t.id.clone(), Decision::Allow));
                            }
                        });
                    });
                }
            },
        );
    }

    fn footer(&mut self, ui: &mut egui::Ui, st: &State, actions: &mut Vec<Action>) {
        let p = self.p;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let (color, label) = match st.conn {
                Conn::Connected => (p.ok, st.latency_ms.map_or("Connected".to_string(), |ms| format!("{ms} ms"))),
                Conn::Checking | Conn::Unknown => (p.warn, "Connecting…".to_string()),
                Conn::Error => (p.err, "Offline".to_string()),
                Conn::Blocked => (p.err, "Blocked".to_string()),
            };
            let conn = status_label(ui, color, label, p.muted);
            if !st.conn_detail.is_empty() {
                conn.on_hover_text(&st.conn_detail);
            } else {
                conn.on_hover_text(format!("Server: {}", st.config.host()));
            }
            ui.label(RichText::new("·").small().color(p.faint));

            let mut models = st.models.clone();
            if !st.config.model.is_empty() && !models.contains(&st.config.model) {
                models.insert(0, st.config.model.clone());
            }
            let current = if st.config.model.is_empty() { "No model".to_string() } else { short_model(&st.config.model, 30) };
            combo("model")
                .selected_text(RichText::new(current).small().color(p.muted))
                .width(0.0)
                .show_ui(ui, |ui| {
                    if models.is_empty() {
                        ui.label(RichText::new("No models reported by the server").small().color(p.muted));
                    }
                    for m in &models {
                        if ui.selectable_label(*m == st.config.model, m).clicked() {
                            actions.push(Action::SetModel(m.clone()));
                        }
                    }
                })
                .response
                .on_hover_text(&st.config.model);

            if st.tps > 0.0 {
                ui.label(RichText::new("·").small().color(p.faint));
                ui.label(RichText::new(format!("{:.1} tok/s", st.tps)).small().color(if st.generating { p.accent } else { p.muted }));
            }
            if st.context_tokens > 0 {
                ui.label(RichText::new("·").small().color(p.faint));
                ui.label(RichText::new(format!("{} ctx", group_thousands(st.context_tokens))).small().color(p.muted));
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                let hover_bg = p.surface2;
                if icons::button(ui, Icon::Settings, p.muted, hover_bg, "Settings (Ctrl+,)").clicked() {
                    actions.push(if self.view == View::Settings { Action::CloseSettings } else { Action::OpenSettings });
                }
                if !st.items.is_empty() && icons::button(ui, Icon::Trash, p.muted, hover_bg, "Clear session (Ctrl+N)").clicked() {
                    actions.push(Action::NewChat);
                }
                if st.config.tools_enabled {
                    combo("approval")
                        .selected_text(RichText::new(st.config.approval.short()).small().color(p.muted))
                        .width(0.0)
                        .show_ui(ui, |ui| {
                            for a in Approval::ALL {
                                if ui.selectable_label(a == st.config.approval, a.label()).clicked() {
                                    actions.push(Action::SetApproval(a));
                                }
                            }
                        })
                        .response
                        .on_hover_text("When to ask before the AI uses a tool");
                    let scratch = collapse_home(&st.config.scratch_dir());
                    if icons::button(ui, Icon::FolderOpen, p.muted, hover_bg, &format!("Open the scratch folder ({scratch})")).clicked() {
                        actions.push(Action::OpenScratch);
                    }
                    if ui.available_width() > 70.0 {
                        let cwd = collapse_home(&st.cwd);
                        let shown = if st.cwd == st.config.scratch_dir() { "scratch".to_string() } else { cwd.clone() };
                        ui.add(Label::new(RichText::new(shown).small().color(p.muted)).truncate())
                            .on_hover_text(format!("Working directory for commands: {cwd}"));
                    }
                }
            });
        });
    }

    fn settings(&mut self, ui: &mut egui::Ui, st: &State, actions: &mut Vec<Action>) {
        let p = self.p;
        let Some(d) = self.draft.as_mut() else { return };
        let section = |ui: &mut egui::Ui, title: &str| {
            ui.add_space(6.0);
            ui.label(RichText::new(title).heading().color(p.text));
            ui.add_space(2.0);
        };
        let row_label = |ui: &mut egui::Ui, s: &str| {
            ui.label(RichText::new(s).color(p.muted));
        };
        let footer_h = 40.0;
        ScrollArea::vertical().id_salt("settings").max_height(ui.available_height() - footer_h).auto_shrink([false, false]).show(
            ui,
            |ui| {
                ui.set_width(ui.available_width() - 10.0);
                let field_w = ui.available_width() - 190.0;

                section(ui, "Server");
                egui::Grid::new("server").num_columns(2).spacing([16.0, 8.0]).min_col_width(130.0).show(ui, |ui| {
                    row_label(ui, "URL");
                    ui.add(TextEdit::singleline(&mut d.server_url).desired_width(field_w).hint_text("http://192.168.1.50:8080"));
                    ui.end_row();
                    row_label(ui, "API key");
                    ui.add(TextEdit::singleline(&mut d.api_key).password(true).desired_width(field_w).hint_text("optional"));
                    ui.end_row();
                    row_label(ui, "Model");
                    ui.horizontal(|ui| {
                        let tested = self.test.lock().unwrap().clone();
                        let mut models = match &tested {
                            Some(Ok((m, _))) => m.clone(),
                            _ => st.models.clone(),
                        };
                        if !d.model.is_empty() && !models.contains(&d.model) {
                            models.insert(0, d.model.clone());
                        }
                        combo("settings-model")
                            .selected_text(if d.model.is_empty() { "Select…" } else { &d.model })
                            .width(field_w - 150.0)
                            .show_ui(ui, |ui| {
                                for m in models {
                                    ui.selectable_value(&mut d.model, m.clone(), m);
                                }
                            });
                        if ui.add_enabled(!self.testing, Button::new("Test connection")).clicked() {
                            actions.push(Action::TestConnection);
                        }
                    });
                    ui.end_row();
                    row_label(ui, "");
                    ui.horizontal(|ui| match (self.testing, self.test.lock().unwrap().as_ref()) {
                        (true, _) => {
                            icons::spinner(ui, 13.0, p.muted);
                            ui.label(RichText::new("Testing…").small().color(p.muted));
                        }
                        (_, Some(Ok((m, ms)))) => {
                            icons::show(ui, Icon::Check, 13.0, p.ok);
                            ui.label(RichText::new(format!("Connected in {ms} ms · {} model(s)", m.len())).small().color(p.ok));
                        }
                        (_, Some(Err(e))) => {
                            icons::show(ui, Icon::X, 13.0, p.err);
                            ui.add(Label::new(RichText::new(e).small().color(p.err)).wrap());
                        }
                        _ => {
                            ui.label(RichText::new("/v1 is added when the URL has no path").small().color(p.faint));
                        }
                    });
                    ui.end_row();
                    row_label(ui, "Privacy");
                    check(ui, &p, &mut d.restrict_to_lan, "Local network only (loopback, private IPs, LAN host names)");
                    ui.end_row();
                });

                section(ui, "Generation");
                egui::Grid::new("gen").num_columns(2).spacing([16.0, 8.0]).min_col_width(130.0).show(ui, |ui| {
                    row_label(ui, "Temperature");
                    ui.add(egui::Slider::new(&mut d.temperature, 0.0..=2.0).step_by(0.05));
                    ui.end_row();
                    row_label(ui, "Max tokens");
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut d.max_tokens).range(0..=1_000_000).speed(64));
                        ui.label(RichText::new("0 = server default").small().color(p.faint));
                    });
                    ui.end_row();
                    row_label(ui, "Idle timeout");
                    ui.add(egui::DragValue::new(&mut d.idle_timeout_secs).range(10..=3600).suffix(" s"));
                    ui.end_row();
                    row_label(ui, "Extra instructions");
                    ui.add(
                        TextEdit::multiline(&mut d.system_prompt)
                            .desired_width(field_w)
                            .desired_rows(3)
                            .hint_text("Appended to the system prompt, e.g. “Answer in German”"),
                    );
                    ui.end_row();
                });

                section(ui, "Tools");
                egui::Grid::new("tools").num_columns(2).spacing([16.0, 8.0]).min_col_width(130.0).show(ui, |ui| {
                    row_label(ui, "Enabled");
                    check(ui, &p, &mut d.tools_enabled, "Let the AI run commands and read/write files");
                    ui.end_row();
                    row_label(ui, "Approval");
                    combo("settings-approval").selected_text(d.approval.label()).width(220.0).show_ui(ui, |ui| {
                        for a in Approval::ALL {
                            ui.selectable_value(&mut d.approval, a, a.label());
                        }
                    });
                    ui.end_row();
                    row_label(ui, "Start directory");
                    ui.add(TextEdit::singleline(&mut d.working_dir).desired_width(field_w).hint_text("scratch directory (default)"));
                    ui.end_row();
                    row_label(ui, "Scratch directory");
                    ui.horizontal(|ui| {
                        ui.add(
                            TextEdit::singleline(&mut d.scratch_dir)
                                .desired_width(field_w - 90.0)
                                .hint_text(collapse_home(&default_scratch_dir())),
                        )
                        .on_hover_text("Temporary and generated files go here ($TMPDIR for commands)");
                        let open = Button::image_and_text(Icon::FolderOpen.image(p.text).fit_to_exact_size(vec2(14.0, 14.0)), "Open");
                        if ui.add(open).on_hover_text("Open in the file manager").clicked() {
                            desktop::open_path(&d.ensure_scratch_dir());
                        }
                    });
                    ui.end_row();
                    row_label(ui, "Command timeout");
                    ui.add(egui::DragValue::new(&mut d.command_timeout_secs).range(1..=3600).suffix(" s"));
                    ui.end_row();
                    row_label(ui, "Max tool output");
                    ui.add(egui::DragValue::new(&mut d.max_output_chars).range(1000..=1_000_000).speed(500).suffix(" chars"));
                    ui.end_row();
                    row_label(ui, "Max tool rounds");
                    ui.add(egui::DragValue::new(&mut d.max_tool_rounds).range(1..=500));
                    ui.end_row();
                });

                section(ui, "Window");
                egui::Grid::new("window").num_columns(2).spacing([16.0, 8.0]).min_col_width(130.0).show(ui, |ui| {
                    row_label(ui, "Size");
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut d.width).range(480.0..=3000.0).suffix(" px"));
                        ui.label("×");
                        ui.add(egui::DragValue::new(&mut d.height).range(300.0..=2000.0).suffix(" px"));
                    });
                    ui.end_row();
                    row_label(ui, "Behaviour");
                    check(ui, &p, &mut d.hide_on_focus_loss, "Hide when another window gets focus");
                    ui.end_row();
                    row_label(ui, "Tray icon");
                    ui.horizontal(|ui| {
                        combo("settings-tray").selected_text(d.tray_icon.label()).width(150.0).show_ui(ui, |ui| {
                            for t in TrayIcon::ALL {
                                ui.selectable_value(&mut d.tray_icon, t, t.label());
                            }
                        });
                        if d.tray_icon == TrayIcon::Custom {
                            ui.add(
                                TextEdit::singleline(&mut d.tray_icon_custom)
                                    .desired_width(field_w - 160.0)
                                    .hint_text("icon name (e.g. utilities-terminal) or .svg/.png path"),
                            );
                        }
                    });
                    ui.end_row();
                    row_label(ui, "History");
                    check(ui, &p, &mut d.keep_history, "Keep the conversation across restarts");
                    ui.end_row();
                    row_label(ui, "Shortcut");
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Meta+Alt+Space by default").color(p.text));
                        if ui.button("Change…").on_hover_text("System Settings → Keyboard → Shortcuts → LoKAI").clicked() {
                            let _ = std::process::Command::new("systemsettings").arg("kcm_keys").spawn();
                        }
                    });
                    ui.end_row();
                });
            },
        );
        ui.add_space(8.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui
                .add(Button::new(RichText::new("Save").color(p.on_accent)).fill(p.accent).corner_radius(8).min_size(vec2(80.0, 30.0)))
                .clicked()
            {
                actions.push(Action::SaveSettings);
            }
            if ui.add(Button::new("Cancel").corner_radius(8).min_size(vec2(80.0, 30.0))).clicked() {
                actions.push(Action::CloseSettings);
            }
        });
    }
}

#[derive(Debug, PartialEq)]
enum Segment<'a> {
    Markdown(&'a str),
    Code { lang: &'a str, code: String },
}

/// Splits fenced code blocks (``` or ~~~) out of a Markdown answer so they can be drawn with
/// our own widget. An unterminated fence (still streaming) runs to the end.
fn split_fences(text: &str) -> Vec<Segment<'_>> {
    let mut out = Vec::new();
    let mut md_start = 0;
    let mut pos = 0;
    let mut lines = text.split_inclusive('\n');
    while let Some(line) = lines.next() {
        let start = pos;
        pos += line.len();
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        let Some(fence @ ('`' | '~')) = trimmed.chars().next() else { continue };
        let n = trimmed.chars().take_while(|&c| c == fence).count();
        let info = trimmed[n..].trim();
        if n < 3 || (fence == '`' && info.contains('`')) {
            continue;
        }
        let mut code = String::new();
        for l in lines.by_ref() {
            pos += l.len();
            let t = l.trim();
            if t.len() >= n && t.chars().all(|c| c == fence) {
                break;
            }
            let spaces = l.len() - l.trim_start_matches(' ').len();
            code.push_str(&l[spaces.min(indent)..]);
        }
        if !text[md_start..start].trim().is_empty() {
            out.push(Segment::Markdown(&text[md_start..start]));
        }
        out.push(Segment::Code { lang: info.split_whitespace().next().unwrap_or(""), code: code.trim_end_matches('\n').to_string() });
        md_start = pos;
    }
    if !text[md_start..].trim().is_empty() {
        out.push(Segment::Markdown(&text[md_start..]));
    }
    out
}

/// "org/Some-Model-27B-GGUF:Q4_K_M" → "Some-Model-27B-GGUF:Q4_K_M", ellipsized to `max` chars.
fn short_model(name: &str, max: usize) -> String {
    let short = name.rsplit('/').next().unwrap_or(name);
    if short.chars().count() <= max { short.to_string() } else { format!("{}…", short.chars().take(max - 1).collect::<String>()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fences() {
        let s = split_fences("Run this:\n\n```sh\nls -la\n```\nDone, use `x` inline.\n  ~~~\n  indented\n");
        assert_eq!(
            s,
            vec![
                Segment::Markdown("Run this:\n\n"),
                Segment::Code { lang: "sh", code: "ls -la".into() },
                Segment::Markdown("Done, use `x` inline.\n"),
                Segment::Code { lang: "", code: "indented".into() },
            ]
        );
        assert_eq!(split_fences("plain ``` not a fence"), vec![Segment::Markdown("plain ``` not a fence")]);
    }
}

/// Renders the launcher offscreen into PNGs for visual review:
/// `LOKAI_SNAPSHOT_DIR=/tmp/shots cargo test --release -- --ignored render_snapshots`
#[cfg(test)]
mod snapshots {
    use super::*;
    use crate::core::{Conn, ToolItem, ToolStatus};
    use egui_kittest::Harness;

    /// An answer with inline and display math in every delimiter style, and one formula that
    /// can't be typeset.
    const MATH_ANSWER: &str = r"**Euler's identity** links five constants: $e^{i\pi} + 1 = 0$. It follows from $e^{ix} = \cos x + i \sin x$ at $x = \pi$.

The **Basel problem** asks for the sum of reciprocal squares:

$$\sum_{n=1}^{\infty} \frac{1}{n^2} = \frac{\pi^2}{6} \approx 1.645$$

The same works with \( \zeta(s) \) for even \(s\), and a rotation matrix:
\[
R(\theta) = \begin{pmatrix} \cos\theta & -\sin\theta \\ \sin\theta & \cos\theta \end{pmatrix}
\]
Unsupported LaTeX stays readable: $\frac{1}{$.";

    fn render(name: &str, view: View, fill: impl Fn(&mut State)) {
        let dir = std::path::PathBuf::from(std::env::var("LOKAI_SNAPSHOT_DIR").unwrap_or_else(|_| "target/snapshots".into()));
        render_to(&dir.join(format!("{name}.png")), view, fill);
    }

    /// Asking for a plot: the model installs matplotlib, writes and runs a script, and shows the
    /// result (assets/demo-plot.svg, made with matplotlib).
    fn plot_scene(st: &mut State) {
        let tool = |id: &str, name: &str, summary: &str, detail: String| {
            Item::Tool(ToolItem {
                id: id.into(),
                name: name.into(),
                summary: summary.into(),
                detail,
                output: String::new(),
                status: ToolStatus::Done,
            })
        };
        let plot = std::env::current_dir().unwrap().join("assets/demo-plot.svg");
        st.items = vec![
            Item::User { text: "Plot sin(x) and cos(x) from −2π to 2π".into() },
            tool("p1", "run_command", "uv pip install matplotlib", "$ uv pip install matplotlib".into()),
            tool("p2", "write_file", "plot_trig.py (24 lines)", String::new()),
            tool("p3", "run_command", "python plot_trig.py", "$ python plot_trig.py".into()),
            tool("p4", "show_image", "sin(x) and cos(x) on [−2π, 2π]", plot.display().to_string()),
            Item::Assistant {
                text: "Here are **sin(x)** and **cos(x)** over [−2π, 2π]. The cosine is the sine shifted left by π/2.".into(),
                reasoning: String::new(),
                streaming: false,
            },
        ];
        st.tps = 41.2;
        st.context_tokens = 3874;
    }

    /// Renders one view of a clean session: fresh config and data directories, and a server
    /// address nobody listens on, so nothing real (server, models, history) shows up in the image.
    fn render_to(path: &std::path::Path, view: View, fill: impl Fn(&mut State)) {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).unwrap();
        }
        let (mut harness, tmp, _) = harness(&path.display().to_string(), view, fill);
        harness.run_steps(6);
        let image = harness.render().expect("render");
        image.save(path).unwrap();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The launcher in a test harness, for `view` of a clean session filled in by `fill`.
    /// Returns it with the temporary directory that holds its config and data, and its core.
    fn harness(name: &str, view: View, fill: impl Fn(&mut State)) -> (Harness<'static>, PathBuf, Arc<Core>) {
        let tmp = std::env::temp_dir().join(format!("lokai-snap-{}-{}", std::process::id(), name.replace('/', "_")));
        std::fs::create_dir_all(tmp.join("config/lokai")).unwrap();
        std::fs::write(tmp.join("config/lokai/config.toml"), "server_url = \"http://127.0.0.1:1\"\n").unwrap();
        // SAFETY: set before any other thread reads them; keeps the real config untouched.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", tmp.join("config"));
            std::env::set_var("XDG_DATA_HOME", tmp.join("data"));
        }
        let core = Core::new();
        // Let the first connection check fail (nothing listens on port 1); the next one is 30 s away.
        let start = Instant::now();
        while !matches!(core.lock().conn, Conn::Error) {
            assert!(start.elapsed() < Duration::from_secs(5), "connection check did not finish");
            std::thread::sleep(Duration::from_millis(10));
        }
        {
            let mut st = core.lock();
            st.config.model = "Qwen3-32B-Q4_K_M".into();
            st.cwd = st.config.scratch_dir();
            st.conn = Conn::Connected;
            st.conn_detail.clear();
            st.latency_ms = Some(3);
            st.models = vec![st.config.model.clone(), "Qwen3-Coder-30B-A3B".into()];
            fill(&mut st);
        }
        let rx: &'static mpsc::Receiver<Cmd> = Box::leak(Box::new(mpsc::channel().1));
        let height = panel_height(view, &core.lock());
        let width = 780.0;
        let mut app: Option<Launcher> = None;
        let ui_core = core.clone();
        let harness = Harness::builder()
            .with_size(vec2(width + 2.0 * MARGIN, height + 2.0 * MARGIN))
            .with_pixels_per_point(1.5)
            .with_accessibility_check(false)
            .build_ui(move |ui| {
                let app = app.get_or_insert_with(|| {
                    let ctx = ui.ctx();
                    theme::install_fonts(ctx);
                    egui_extras::install_image_loaders(ctx);
                    let p = Palette::detect();
                    p.apply(ctx);
                    let logo = ctx.load_texture(
                        "logo",
                        egui::ColorImage::from_rgba_unmultiplied([64, 64], &theme::icon_rgba(64)),
                        Default::default(),
                    );
                    let mut l = Launcher {
                        core: ui_core.clone(),
                        rx,
                        outcome: Box::leak(Box::new(Outcome::Closed)),
                        p,
                        view: View::Chat,
                        input: String::new(),
                        md: CommonMarkCache::default(),
                        logo,
                        frame: 10,
                        opened: Instant::now(),
                        placement: Placement::Done,
                        had_focus: true,
                        focus_input: true,
                        requested_height: height,
                        width,
                        draft: None,
                        test: Arc::new(Mutex::new(None)),
                        testing: false,
                        expanded: HashSet::new(),
                        reasoning_open: HashSet::new(),
                        copied: None,
                        esc_hold: None,
                        images: HashMap::new(),
                        math: Default::default(),
                        sent_history: Vec::new(),
                    };
                    if view == View::Settings {
                        l.open_settings();
                    }
                    l.expanded.insert("c2".into());
                    l
                });
                app.draw(ui);
            });
        (harness, tmp, core)
    }

    /// While a reply runs, tapping Esc only closes the window (the reply goes on); holding it stops
    /// the reply.
    #[test]
    fn esc_tap_closes_and_hold_stops() {
        let (mut h, tmp, core) = harness("keys", View::Chat, |st| {
            st.items = vec![Item::User { text: "A question".into() }];
            st.busy = true;
        });
        h.run_steps(2);
        h.key_press(Key::Escape); // down and up in the same frame: a tap
        h.run_steps(2);
        assert!(core.lock().busy, "tapping Esc stopped the reply");
        let prompt = Id::unique("prompt");
        h.ctx.memory_mut(|m| m.surrender_focus(prompt));
        h.run_steps(1);
        assert!(!h.ctx.memory(|m| m.has_focus(prompt)));
        h.key_down(Key::Escape);
        h.run_steps(4); // 0.25 s per step
        assert!(!core.lock().busy, "holding Esc didn't stop the reply");
        h.key_up(Key::Escape);
        h.run_steps(2);
        assert!(h.ctx.memory(|m| m.has_focus(prompt)), "the input field isn't focused after stopping");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The README screenshots: `just screenshot` writes assets/screenshot.png and screenshot-plot.png.
    #[test]
    #[ignore]
    fn readme_screenshot() {
        let dir = PathBuf::from(std::env::var("LOKAI_SCREENSHOT_DIR").unwrap_or_else(|_| "assets".into()));
        render_to(&dir.join("screenshot-plot.png"), View::Chat, |st| {
            plot_scene(st);
            st.config.height = 776.0;
        });
        render_to(&dir.join("screenshot.png"), View::Chat, |st| {
            st.items = vec![
                Item::User { text: "Why is my disk almost full?".into() },
                Item::Assistant { text: String::new(), reasoning: "Check which folders in home are the largest.".into(), streaming: false },
                Item::Tool(ToolItem {
                    id: "c1".into(),
                    name: "run_command".into(),
                    summary: "du -sh ~/* ~/.cache | sort -rh | head -4".into(),
                    detail: "$ du -sh ~/* ~/.cache | sort -rh | head -4".into(),
                    output: "41G\t/home/user/Videos\n12G\t/home/user/.cache\n6.3G\t/home/user/Downloads\n2.1G\t/home/user/Documents".into(),
                    status: ToolStatus::Done,
                }),
                Item::Assistant {
                    text: "Most of it is **Videos (41 GB)**. The next biggest is **`~/.cache` (12 GB)**, which is safe to clear; \
                           apps rebuild it when needed. To clear just the thumbnails:\n\n```sh\nrm -rf ~/.cache/thumbnails\n```"
                        .into(),
                    reasoning: String::new(),
                    streaming: false,
                },
                Item::User { text: "Go ahead".into() },
                Item::Tool(ToolItem {
                    id: "c3".into(),
                    name: "run_command".into(),
                    summary: "rm -rf ~/.cache/thumbnails".into(),
                    detail: "$ rm -rf ~/.cache/thumbnails\n# in scratch".into(),
                    output: String::new(),
                    status: ToolStatus::Approval,
                }),
            ];
            st.config.height = 660.0;
            st.busy = true;
            st.tps = 38.4;
            st.context_tokens = 2914;
            st.status = "Waiting for your approval".into();
        });
    }

    #[test]
    #[ignore]
    fn render_snapshots() {
        render("empty", View::Chat, |_| {});
        render("settings", View::Settings, |st| {
            st.items.push(Item::User { text: "x".into() });
        });
        render("plot", View::Chat, plot_scene);
        // Holding Esc during a reply: the ring around the stop button fills up.
        let (mut h, tmp, _) = harness("holding-esc", View::Chat, |st| {
            st.items = vec![Item::User { text: "A long question".into() }];
            st.busy = true;
            st.status = "Generating…".into();
        });
        h.run_steps(2);
        h.key_down(Key::Escape);
        h.run_steps(2); // 0.5 s of the 0.6 s
        let dir = PathBuf::from(std::env::var("LOKAI_SNAPSHOT_DIR").unwrap_or_else(|_| "target/snapshots".into()));
        h.render().expect("render").save(dir.join("holding-esc.png")).unwrap();
        let _ = std::fs::remove_dir_all(&tmp);
        render("math", View::Chat, |st| {
            st.items = vec![
                Item::User { text: "Explain Euler's identity and the Basel problem".into() },
                Item::Assistant { text: MATH_ANSWER.into(), reasoning: String::new(), streaming: false },
            ];
        });
        render("settings-full", View::Settings, |st| {
            st.items.push(Item::User { text: "x".into() });
            st.config.height = 1250.0;
            st.config.tray_icon = TrayIcon::Custom;
        });
        render("conversation", View::Chat, |st| {
            st.items = vec![
                Item::User { text: "Which process is using the most memory, and how much free disk space is left on /home?".into() },
                Item::Assistant {
                    text: String::new(),
                    reasoning: "The user wants memory usage per process and disk space. I'll run ps and df.".into(),
                    streaming: false,
                },
                Item::Tool(ToolItem {
                    id: "c1".into(),
                    name: "run_command".into(),
                    summary: "ps -eo pid,comm,rss --sort=-rss | head -5".into(),
                    detail: "$ ps -eo pid,comm,rss --sort=-rss | head -5".into(),
                    output: String::new(),
                    status: ToolStatus::Done,
                }),
                Item::Tool(ToolItem {
                    id: "c2".into(),
                    name: "run_command".into(),
                    summary: "df -h /home".into(),
                    detail: "$ df -h /home\n# in ~/Projects/lokai".into(),
                    output: "Filesystem      Size  Used Avail Use% Mounted on\n/dev/nvme0n1p3  1.8T  1.1T  690G  62% /home".into(),
                    status: ToolStatus::Done,
                }),
                Item::Assistant {
                    text: "**Firefox** is the biggest consumer at about **3.2 GB** RSS, followed by `plasmashell` (610 MB).\n\n\
                           `/home` has **690 GB free** (62 % used):\n\n```sh\ndf -h /home\n```\n\n- Close unused tabs to free memory\n- Run `ncdu ~` to find large folders"
                        .into(),
                    reasoning: String::new(),
                    streaming: false,
                },
                Item::User { text: "Clean the pacman cache".into() },
                Item::Tool(ToolItem {
                    id: "c3".into(),
                    name: "run_command".into(),
                    summary: "paccache -rk2".into(),
                    detail: "$ paccache -rk2\n# in ~".into(),
                    output: String::new(),
                    status: ToolStatus::Approval,
                }),
            ];
            st.busy = true;
            st.tps = 42.8;
            st.context_tokens = 3871;
            st.status = "Waiting for your approval".into();
        });
    }
}
