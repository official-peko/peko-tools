//! The `peko run --devtools` window: live build diagnostics, the app's route
//! history, and the web console, in a native panel next to the running app.
//!
//! The window is hosted by the CLI, not the app, for one reason: the CLI
//! process persists across every rebuild while the app is killed and respawned.
//! A failed rebuild does not relaunch the app, so an app-hosted panel would be
//! gone exactly when the build error needs reading. The CLI also already owns
//! the build diagnostics. winit requires the event loop on the process main
//! thread, so the window runs there and the dev loop runs on a background
//! thread, streaming events over the channel below.

use std::collections::VecDeque;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, Sender};

use eframe::egui;
use peko_core::diagnostics::DiagnosticList;

/// Lightweight syntax highlighters that build an egui `LayoutJob`, so the source
/// and JSON payloads read clearly without a heavy highlighting dependency.
mod highlight {
    use eframe::egui::{
        Color32, FontId,
        text::{LayoutJob, TextFormat},
    };

    const KEY: Color32 = Color32::from_rgb(0x9c, 0xdc, 0xfe);
    const STRING: Color32 = Color32::from_rgb(0xce, 0x91, 0x78);
    const NUMBER: Color32 = Color32::from_rgb(0xb5, 0xce, 0xa8);
    const LITERAL: Color32 = Color32::from_rgb(0x56, 0x9c, 0xd6);
    const PUNCT: Color32 = Color32::from_rgb(0x85, 0x85, 0x85);
    const PLAIN: Color32 = Color32::from_rgb(0xd4, 0xd4, 0xd4);
    const TAG: Color32 = Color32::from_rgb(0x56, 0x9c, 0xd6);
    const ATTR: Color32 = Color32::from_rgb(0x9c, 0xdc, 0xfe);
    const TEXT: Color32 = Color32::from_rgb(0xd4, 0xd4, 0xd4);
    const COMMENT: Color32 = Color32::from_rgb(0x6a, 0x99, 0x55);

    fn piece(job: &mut LayoutJob, text: &str, color: Color32) {
        job.append(
            text,
            0.0,
            TextFormat {
                color,
                font_id: FontId::monospace(12.0),
                ..Default::default()
            },
        );
    }

    fn at(chars: &[char], i: usize, needle: &str) -> bool {
        needle
            .chars()
            .enumerate()
            .all(|(k, c)| chars.get(i + k) == Some(&c))
    }

    /// Highlight a JSON payload.
    pub fn json(text: &str) -> LayoutJob {
        let chars: Vec<char> = text.chars().collect();
        let mut job = LayoutJob::default();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if c == '"' {
                let mut token = String::from('"');
                i += 1;
                while i < chars.len() {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        token.push(chars[i]);
                        token.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                    token.push(chars[i]);
                    let closing = chars[i] == '"';
                    i += 1;
                    if closing {
                        break;
                    }
                }
                let mut j = i;
                while j < chars.len() && chars[j].is_whitespace() {
                    j += 1;
                }
                let is_key = chars.get(j) == Some(&':');
                piece(&mut job, &token, if is_key { KEY } else { STRING });
            } else if c.is_ascii_digit()
                || (c == '-' && chars.get(i + 1).is_some_and(char::is_ascii_digit))
            {
                let mut token = String::new();
                token.push(c);
                i += 1;
                while i < chars.len()
                    && (chars[i].is_ascii_digit() || "+-.eE".contains(chars[i]))
                {
                    token.push(chars[i]);
                    i += 1;
                }
                piece(&mut job, &token, NUMBER);
            } else if c.is_alphabetic() {
                let mut token = String::new();
                while i < chars.len() && chars[i].is_alphabetic() {
                    token.push(chars[i]);
                    i += 1;
                }
                let color = if token == "true" || token == "false" || token == "null" {
                    LITERAL
                } else {
                    PLAIN
                };
                piece(&mut job, &token, color);
            } else if "{}[]:,".contains(c) {
                piece(&mut job, &c.to_string(), PUNCT);
                i += 1;
            } else {
                piece(&mut job, &c.to_string(), PLAIN);
                i += 1;
            }
        }
        job
    }

    /// Highlight HTML source.
    pub fn html(text: &str) -> LayoutJob {
        let chars: Vec<char> = text.chars().collect();
        let mut job = LayoutJob::default();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '<' {
                if at(&chars, i, "<!--") {
                    let mut token = String::new();
                    while i < chars.len() {
                        token.push(chars[i]);
                        if at(&chars, i, "-->") {
                            token.push(chars[i + 1]);
                            token.push(chars[i + 2]);
                            i += 3;
                            break;
                        }
                        i += 1;
                    }
                    piece(&mut job, &token, COMMENT);
                    continue;
                }
                piece(&mut job, "<", PUNCT);
                i += 1;
                if chars.get(i) == Some(&'/') {
                    piece(&mut job, "/", PUNCT);
                    i += 1;
                }
                let mut name = String::new();
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '-') {
                    name.push(chars[i]);
                    i += 1;
                }
                piece(&mut job, &name, TAG);
                while i < chars.len() && chars[i] != '>' {
                    let c = chars[i];
                    if c == '"' || c == '\'' {
                        let quote = c;
                        let mut token = String::new();
                        token.push(c);
                        i += 1;
                        while i < chars.len() {
                            token.push(chars[i]);
                            let closing = chars[i] == quote;
                            i += 1;
                            if closing {
                                break;
                            }
                        }
                        piece(&mut job, &token, STRING);
                    } else if c.is_alphabetic() {
                        let mut token = String::new();
                        while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '-') {
                            token.push(chars[i]);
                            i += 1;
                        }
                        piece(&mut job, &token, ATTR);
                    } else {
                        piece(&mut job, &c.to_string(), PUNCT);
                        i += 1;
                    }
                }
                if i < chars.len() {
                    piece(&mut job, ">", PUNCT);
                    i += 1;
                }
            } else {
                let mut token = String::new();
                while i < chars.len() && chars[i] != '<' {
                    token.push(chars[i]);
                    i += 1;
                }
                piece(&mut job, &token, TEXT);
            }
        }
        job
    }

    const VOID_ELEMENTS: &[&str] = &[
        "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param",
        "source", "track", "wbr",
    ];

    /// Re-indent HTML so nested elements read on their own lines. A best-effort
    /// pretty printer: void, self-closing, comment, and declaration tags do not
    /// change nesting depth.
    pub fn reindent(text: &str) -> String {
        let chars: Vec<char> = text.chars().collect();
        let mut out = String::new();
        let mut depth: usize = 0;
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '<' {
                let start = i;
                while i < chars.len() && chars[i] != '>' {
                    i += 1;
                }
                if i < chars.len() {
                    i += 1;
                }
                let tag: String = chars[start..i].iter().collect();
                let is_close = tag.starts_with("</");
                let is_comment = tag.starts_with("<!--");
                let is_decl = tag.starts_with("<!");
                let is_self_close = tag.ends_with("/>");
                let name: String = tag
                    .trim_start_matches('<')
                    .trim_start_matches('/')
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '-')
                    .collect::<String>()
                    .to_lowercase();
                let is_void = VOID_ELEMENTS.contains(&name.as_str());
                if is_close {
                    depth = depth.saturating_sub(1);
                    out.push_str(&"  ".repeat(depth));
                    out.push_str(&tag);
                    out.push('\n');
                } else {
                    out.push_str(&"  ".repeat(depth));
                    out.push_str(&tag);
                    out.push('\n');
                    if !is_comment && !is_decl && !is_self_close && !is_void {
                        depth += 1;
                    }
                }
            } else {
                let start = i;
                while i < chars.len() && chars[i] != '<' {
                    i += 1;
                }
                let content: String = chars[start..i].iter().collect();
                let trimmed = content.trim();
                if !trimmed.is_empty() {
                    out.push_str(&"  ".repeat(depth));
                    out.push_str(trimmed);
                    out.push('\n');
                }
            }
        }
        out
    }
}

/// A build diagnostic in the form the window renders.
pub struct DevDiagnostic {
    pub severity: String,
    pub file: String,
    pub line: usize,
    pub column: usize,
    pub message: String,
}

/// A web console line forwarded from the app.
pub struct DevConsoleLine {
    pub level: String,
    pub text: String,
}

/// An event from the dev loop to the window.
pub enum DevEvent {
    /// A short status line (building, built, dev URL, errors).
    Status(String),
    /// The full diagnostics of the latest build, replacing the panel.
    Diagnostics(Vec<DevDiagnostic>),
    /// A route the app navigated to.
    Route(String),
    /// A console line from the web UI.
    Console(DevConsoleLine),
    /// The result of an interactive request: an evaluated console expression
    /// (kind "eval") or the page source (kind "source").
    EvalResult { kind: String, ok: bool, text: String },
    /// A piece of bridge traffic for the inspector: dir is call, reply, or
    /// event; label is the method or event name; data is the JSON payload.
    Trace {
        dir: String,
        label: String,
        data: String,
    },
}

/// One entry in the bridge inspector.
struct TraceEntry {
    dir: String,
    label: String,
    data: String,
}

/// The tab shown in the central area of the window.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Diagnostics,
    Routes,
    Bridge,
    Source,
}

/// An action from the window to the dev loop.
pub enum DevAction {
    Rebuild,
    RestartApp,
    /// Evaluate an expression in the running page.
    Eval(String),
    /// Fetch the current page source.
    ViewSource,
    Shutdown,
}

/// The dev loop's end of the devtools channel: it sends events and reads
/// actions, and shares a shutdown flag the window sets when it closes.
pub struct DevChannel {
    events: Sender<DevEvent>,
    pub actions: Receiver<DevAction>,
    pub shutdown: Arc<AtomicBool>,
}

impl DevChannel {
    /// Send an event to the window, ignoring a closed window.
    pub fn send(&self, event: DevEvent) {
        let _ = self.events.send(event);
    }

    /// Set the status line.
    pub fn status(&self, text: impl Into<String>) {
        self.send(DevEvent::Status(text.into()));
    }

    /// Record a route the app navigated to.
    pub fn route(&self, path: impl Into<String>) {
        self.send(DevEvent::Route(path.into()));
    }

    /// Replace the diagnostics panel with a build's diagnostics.
    pub fn diagnostics(&self, list: &DiagnosticList) {
        let items = list
            .get_diagnostics()
            .iter()
            .map(|d| DevDiagnostic {
                severity: d.diagnostic_type.to_string(),
                file: d
                    .file
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_else(|| d.file.display().to_string()),
                line: d.start.line,
                column: d.start.column,
                message: d.message.clone(),
            })
            .collect();
        self.send(DevEvent::Diagnostics(items));
    }

    /// Clear the diagnostics panel (a clean build).
    pub fn clear_diagnostics(&self) {
        self.send(DevEvent::Diagnostics(Vec::new()));
    }

    /// A clone of the event sender, for a thread that streams events (the app
    /// stdout reader that forwards console lines) beyond this channel's borrow.
    pub fn sender(&self) -> Sender<DevEvent> {
        self.events.clone()
    }
}

/// Build the paired channel ends: the loop's `DevChannel`, and the window's
/// event receiver, action sender, and shared shutdown flag.
pub fn channel() -> (DevChannel, Receiver<DevEvent>, Sender<DevAction>, Arc<AtomicBool>) {
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let (action_tx, action_rx) = std::sync::mpsc::channel();
    let shutdown = Arc::new(AtomicBool::new(false));
    let dev = DevChannel {
        events: event_tx,
        actions: action_rx,
        shutdown: shutdown.clone(),
    };
    (dev, event_rx, action_tx, shutdown)
}

struct DevtoolsApp {
    events: Receiver<DevEvent>,
    actions: Sender<DevAction>,
    status: String,
    diagnostics: Vec<DevDiagnostic>,
    routes: VecDeque<String>,
    console: VecDeque<DevConsoleLine>,
    console_input: String,
    source: String,
    traces: VecDeque<TraceEntry>,
    tab: Tab,
}

impl DevtoolsApp {
    fn push_console(&mut self, level: &str, text: String) {
        self.console.push_back(DevConsoleLine {
            level: level.to_string(),
            text,
        });
        while self.console.len() > 500 {
            self.console.pop_front();
        }
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                DevEvent::Status(text) => self.status = text,
                DevEvent::Diagnostics(items) => {
                    // Surface a failing build by switching to the panel.
                    if !items.is_empty() {
                        self.tab = Tab::Diagnostics;
                    }
                    self.diagnostics = items;
                }
                DevEvent::Route(path) => {
                    self.routes.push_back(path);
                    while self.routes.len() > 200 {
                        self.routes.pop_front();
                    }
                }
                DevEvent::Console(line) => {
                    self.console.push_back(line);
                    while self.console.len() > 500 {
                        self.console.pop_front();
                    }
                }
                DevEvent::EvalResult { kind, ok, text } => {
                    if kind == "source" {
                        self.source = text;
                        self.tab = Tab::Source;
                    } else {
                        self.push_console(if ok { "result" } else { "error" }, text);
                    }
                }
                DevEvent::Trace { dir, label, data } => {
                    self.traces.push_back(TraceEntry { dir, label, data });
                    while self.traces.len() > 500 {
                        self.traces.pop_front();
                    }
                }
            }
        }
    }

    fn submit_console(&mut self) {
        let code = self.console_input.trim().to_string();
        if code.is_empty() {
            return;
        }
        self.push_console("input", format!("> {code}"));
        let _ = self.actions.send(DevAction::Eval(code));
        self.console_input.clear();
    }

    fn show_diagnostics(&self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .id_salt("peko-diag-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if self.diagnostics.is_empty() {
                    ui.weak("No diagnostics.");
                }
                for diagnostic in &self.diagnostics {
                    let color = if diagnostic.severity == "error" {
                        egui::Color32::from_rgb(0xe8, 0x11, 0x23)
                    } else {
                        egui::Color32::from_rgb(0xd2, 0x9a, 0x00)
                    };
                    ui.horizontal_wrapped(|ui| {
                        ui.colored_label(color, &diagnostic.severity);
                        ui.monospace(format!(
                            "{}:{}:{}",
                            diagnostic.file, diagnostic.line, diagnostic.column
                        ));
                        ui.label(&diagnostic.message);
                    });
                }
            });
    }

    fn show_routes(&self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .id_salt("peko-routes-scroll")
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if self.routes.is_empty() {
                    ui.weak("No navigations yet.");
                }
                for route in &self.routes {
                    ui.monospace(route);
                }
            });
    }

    fn show_bridge(&self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .id_salt("peko-bridge-scroll")
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if self.traces.is_empty() {
                    ui.weak("No bridge traffic yet.");
                }
                for entry in &self.traces {
                    let (color, arrow) = match entry.dir.as_str() {
                        "call" => (egui::Color32::from_rgb(0x4a, 0x9e, 0xff), "-> "),
                        "reply" => (egui::Color32::from_rgb(0xb5, 0xce, 0xa8), "<- "),
                        _ => (egui::Color32::from_rgb(0xd2, 0x9a, 0x00), "ev "),
                    };
                    ui.horizontal_wrapped(|ui| {
                        ui.colored_label(color, format!("{arrow}{}", entry.label));
                        ui.add(egui::Label::new(highlight::json(&entry.data)));
                    });
                }
            });
    }

    fn show_source(&self, ui: &mut egui::Ui) {
        egui::ScrollArea::both()
            .id_salt("peko-source-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if self.source.is_empty() {
                    ui.weak("Click View source to load the current page DOM.");
                } else {
                    let pretty = highlight::reindent(&self.source);
                    ui.add(egui::Label::new(highlight::html(&pretty)).selectable(true));
                }
            });
    }
}

impl eframe::App for DevtoolsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();

        egui::TopBottomPanel::top("peko-devtools-status").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("Peko devtools");
                ui.separator();
                ui.label(&self.status);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Restart app").clicked() {
                        let _ = self.actions.send(DevAction::RestartApp);
                    }
                    if ui.button("Rebuild").clicked() {
                        let _ = self.actions.send(DevAction::Rebuild);
                    }
                    if ui.button("View source").clicked() {
                        let _ = self.actions.send(DevAction::ViewSource);
                    }
                });
            });
            ui.add_space(4.0);
        });

        egui::TopBottomPanel::bottom("peko-devtools-console")
            .resizable(true)
            .default_height(220.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.strong("Console");
                    if ui.button("Clear").clicked() {
                        self.console.clear();
                    }
                });
                // The interactive prompt: an expression to evaluate in the page.
                ui.horizontal(|ui| {
                    ui.label(">");
                    let input = ui.add(
                        egui::TextEdit::singleline(&mut self.console_input)
                            .hint_text("evaluate an expression in the page")
                            .desired_width(f32::INFINITY),
                    );
                    if input.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        self.submit_console();
                        input.request_focus();
                    }
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .id_salt("peko-console-scroll")
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for line in &self.console {
                            // Evaluated results are usually JSON, so highlight
                            // them; other lines take a level color.
                            if line.level == "result" {
                                ui.add(egui::Label::new(highlight::json(&line.text)));
                                continue;
                            }
                            let color = match line.level.as_str() {
                                "error" => egui::Color32::from_rgb(0xe8, 0x11, 0x23),
                                "warn" => egui::Color32::from_rgb(0xd2, 0x9a, 0x00),
                                "input" => egui::Color32::from_rgb(0x4a, 0x9e, 0xff),
                                _ => ui.style().visuals.text_color(),
                            };
                            ui.colored_label(color, egui::RichText::new(&line.text).monospace());
                        }
                    });
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(
                    &mut self.tab,
                    Tab::Diagnostics,
                    format!("Diagnostics ({})", self.diagnostics.len()),
                );
                ui.selectable_value(&mut self.tab, Tab::Routes, "Routes");
                ui.selectable_value(
                    &mut self.tab,
                    Tab::Bridge,
                    format!("Bridge ({})", self.traces.len()),
                );
                ui.selectable_value(&mut self.tab, Tab::Source, "Source");
            });
            ui.separator();

            match self.tab {
                Tab::Diagnostics => self.show_diagnostics(ui),
                Tab::Routes => self.show_routes(ui),
                Tab::Bridge => self.show_bridge(ui),
                Tab::Source => self.show_source(ui),
            }
        });

        // Drain the channel regularly even when there is no OS input.
        ctx.request_repaint_after(std::time::Duration::from_millis(120));
    }
}

/// Open the devtools window on the current (process main) thread. Returns when
/// the window closes; the caller then signals the dev loop to stop.
pub fn run_window(events: Receiver<DevEvent>, actions: Sender<DevAction>) -> ExitCode {
    let app = DevtoolsApp {
        events,
        actions,
        status: "starting...".to_string(),
        diagnostics: Vec::new(),
        routes: VecDeque::new(),
        console: VecDeque::new(),
        console_input: String::new(),
        source: String::new(),
        traces: VecDeque::new(),
        tab: Tab::Bridge,
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size((780.0, 580.0))
            .with_title("Peko devtools"),
        ..Default::default()
    };

    match eframe::run_native(
        "Peko devtools",
        options,
        Box::new(move |_cc| Ok(Box::new(app) as Box<dyn eframe::App>)),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}
