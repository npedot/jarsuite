//! Desktop window over the same operations as the CLI: status, update, activate,
//! rollback, clean and launching tools.
//!
//! One operation runs at a time on a worker thread. It reports its steps through a
//! channel and its transfers (archive, JRE, dependencies) through a
//! [`jarexec::http::ProgressSink`], so the window stays responsive while it works.

use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use eframe::egui;
use indicatif::HumanBytes;

use crate::install::{Activated, InUse, Root, State, Update};

/// Opens the window and returns when it is closed.
pub fn run(root: Root) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([960.0, 680.0])
            .with_min_inner_size([420.0, 360.0]),
        ..Default::default()
    };
    eframe::run_native(
        "jarsuite",
        options,
        Box::new(|cc| Ok(Box::new(App::new(root, cc.egui_ctx.clone())))),
    )
    .map_err(|e| anyhow::anyhow!("opening the window: {e}"))
}

/// What the window shows of the installation, read again after every operation.
#[derive(Default)]
struct Snapshot {
    state: State,
    suite: Option<String>,
    tools: Vec<ToolInfo>,
    installed: Vec<String>,
    error: Option<String>,
}

impl Snapshot {
    fn load(root: &Root) -> Self {
        let mut snap = Snapshot::default();
        let loaded = (|| -> Result<()> {
            snap.state = root.state()?;
            snap.installed = root.installed()?;
            if let Some(current) = &snap.state.current {
                let manifest = root.release_manifest(current)?;
                snap.suite = Some(manifest.suite.name);
                snap.tools = manifest
                    .tools
                    .iter()
                    .map(|tool| ToolInfo::load(root, current, tool))
                    .collect();
            }
            Ok(())
        })();
        snap.error = loaded.err().map(|e| format!("{e:#}"));
        snap
    }
}

/// A tool of the active release, with what its lock tells about it.
#[derive(Clone)]
struct ToolInfo {
    name: String,
    /// From the JAR's file name, `<name>-<version>.jar`.
    version: Option<String>,
    java: Option<String>,
}

impl ToolInfo {
    fn load(root: &Root, release: &str, name: &str) -> Self {
        let lock = std::fs::read_to_string(root.release_dir(release).join(format!("{name}.lock")))
            .ok()
            .and_then(|text| jarexec::lockfile::Lockfile::parse(&text).ok());
        let version = lock.as_ref().and_then(|l| {
            l.app
                .jar
                .strip_prefix(&format!("{name}-"))?
                .strip_suffix(".jar")
                .map(Into::into)
        });
        Self {
            name: name.into(),
            version,
            java: lock.map(|l| l.java.version),
        }
    }
}

enum Msg {
    Step(String),
    Done(Result<String, String>),
    /// The published release, from a check.
    Published(String),
    /// Exit code and end of standard error of a launched tool.
    ToolExited(String, Result<(i32, String), String>),
}

/// Current transfer, written by the worker's progress sink and read by the window.
#[derive(Default)]
struct Transfer {
    label: Mutex<String>,
    total: AtomicU64,
    done: AtomicU64,
}

struct Sink {
    transfer: Arc<Transfer>,
    ctx: egui::Context,
}

impl jarexec::http::ProgressSink for Sink {
    fn start(&self, label: &str, total: u64) {
        *self.transfer.label.lock().unwrap() = label.to_string();
        self.transfer.done.store(0, Ordering::Relaxed);
        self.transfer.total.store(total, Ordering::Relaxed);
        self.ctx.request_repaint();
    }

    fn inc(&self, bytes: u64) {
        self.transfer.done.fetch_add(bytes, Ordering::Relaxed);
        self.ctx.request_repaint();
    }

    fn finish(&self) {
        self.transfer.total.store(0, Ordering::Relaxed);
        self.ctx.request_repaint();
    }
}

struct App {
    root: Arc<Root>,
    ctx: egui::Context,
    snap: Snapshot,
    published: Option<String>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    transfer: Arc<Transfer>,
    /// Step of the operation in progress, `None` when idle.
    busy: Option<String>,
    running: Vec<String>,
    log: Vec<(bool, String)>,
    manifest_input: String,
    /// Arguments typed for each tool, and the tool whose field is open.
    args: HashMap<String, String>,
    args_open: Option<String>,
    settings: bool,
    /// Published release whose update dialog was answered with Later.
    dismissed: Option<String>,
    force: bool,
}

impl App {
    fn new(root: Root, ctx: egui::Context) -> Self {
        theme::apply(&ctx);
        let (tx, rx) = mpsc::channel();
        let snap = Snapshot::load(&root);
        let mut app = Self {
            manifest_input: snap.state.url.clone().unwrap_or_default(),
            root: Arc::new(root),
            ctx,
            snap,
            published: None,
            tx,
            rx,
            transfer: Arc::default(),
            busy: None,
            running: Vec::new(),
            log: Vec::new(),
            args: HashMap::new(),
            args_open: None,
            settings: false,
            dismissed: None,
            force: false,
        };
        if app.snap.state.url.is_some() {
            app.check();
        }
        app
    }

    /// Runs `job` on a worker thread; its `Ok` text goes to the log as a success.
    fn start(
        &mut self,
        step: &str,
        job: impl FnOnce(&Root, &dyn Fn(&str)) -> Result<String> + Send + 'static,
    ) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some(step.to_string());
        let (root, tx, ctx) = (self.root.clone(), self.tx.clone(), self.ctx.clone());
        let sink = Arc::new(Sink {
            transfer: self.transfer.clone(),
            ctx: ctx.clone(),
        });
        std::thread::spawn(move || {
            let step = |s: &str| {
                let _ = tx.send(Msg::Step(s.to_string()));
                ctx.request_repaint();
            };
            let result =
                jarexec::http::report_progress_to(sink, || job(&root, &step)).map_err(|e| {
                    if e.is::<InUse>() {
                        format!("{e}")
                    } else {
                        format!("{e:#}")
                    }
                });
            let _ = tx.send(Msg::Done(result));
            ctx.request_repaint();
        });
    }

    fn check(&mut self) {
        let tx = self.tx.clone();
        self.start("checking the published release", move |root, _| {
            let (_, manifest) = root.fetch_manifest()?;
            let _ = tx.send(Msg::Published(manifest.suite.release.clone()));
            Ok(format!("published release: {}", manifest.suite.release))
        });
    }

    fn update(&mut self) {
        self.start("updating", |root, step| {
            let cache = root.cache();
            let outcome = root.update(
                |lock| jarexec::launch::prepare(lock, &cache, None, false).map(|_| ()),
                step,
            )?;
            match outcome {
                Update::UpToDate(r) => Ok(format!("release {r} is already active")),
                Update::Ready(r) => match root.activate(None, false) {
                    Ok(done) => Ok(activated(&done)),
                    Err(e) if e.is::<InUse>() => Ok(format!(
                        "release {r} is ready, but {e}: then press Activate"
                    )),
                    Err(e) => Err(e),
                },
            }
        });
    }

    fn launch(&mut self, tool: String) {
        let args: Vec<OsString> = self
            .args
            .get(&tool)
            .map(String::as_str)
            .unwrap_or_default()
            .split_whitespace()
            .map(OsString::from)
            .collect();
        self.running.push(tool.clone());
        self.log.push((true, format!("started {tool}")));
        let (root, tx, ctx) = (self.root.clone(), self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let result =
                crate::run::run_captured(&root, &tool, &args).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::ToolExited(tool, result));
            ctx.request_repaint();
        });
    }

    fn receive(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Step(step) => self.busy = Some(step),
                Msg::Published(release) => self.published = Some(release),
                Msg::Done(result) => {
                    self.busy = None;
                    self.transfer.total.store(0, Ordering::Relaxed);
                    self.log.push(match result {
                        Ok(text) => (true, text),
                        Err(text) => (false, text),
                    });
                    self.snap = Snapshot::load(&self.root);
                }
                Msg::ToolExited(tool, result) => {
                    if let Some(i) = self.running.iter().position(|t| *t == tool) {
                        self.running.remove(i);
                    }
                    self.log.push(match result {
                        Ok((0, _)) => (true, format!("{tool} finished")),
                        Ok((code, stderr)) if stderr.is_empty() => {
                            (false, format!("{tool} exited with code {code}"))
                        }
                        Ok((code, stderr)) => {
                            (false, format!("{tool} exited with code {code}:\n{stderr}"))
                        }
                        Err(e) => (false, format!("{tool}: {e}")),
                    });
                }
            }
        }
    }

    /// Suite name, and the settings button.
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let title = self.snap.suite.as_deref().unwrap_or("jarsuite");
            ui.label(
                egui::RichText::new(title)
                    .font(theme::bold(22.0))
                    .color(theme::GRAY_900),
            );
            theme::pill(ui, "LAUNCHER", theme::VIOLET_700, theme::VIOLET_50);
            ui.label(egui::RichText::new("release train").color(theme::GRAY_500));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let label = if self.settings {
                    "Close settings"
                } else {
                    "Settings"
                };
                if ui.add(theme::secondary(label)).clicked() {
                    self.settings = !self.settings;
                }
            });
        });
    }

    /// Releases at a glance, and the one action that matters now.
    fn status_row(&mut self, ui: &mut egui::Ui) {
        let state = &self.snap.state;
        let update = self.update_available();
        let mut segments = vec![("ACTIVE", state.current.clone(), false)];
        if let Some(p) = &self.published {
            segments.push(("PUBLISHED", Some(p.clone()), update.is_some()));
        }
        if state.pending.is_some() {
            segments.push(("READY", state.pending.clone(), true));
        }
        segments.push(("PREVIOUS", state.previous.clone(), false));

        ui.horizontal(|ui| {
            theme::card_frame()
                .corner_radius(8)
                .inner_margin(egui::Margin::symmetric(4, 4))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        for (i, (name, release, highlight)) in segments.iter().enumerate() {
                            if i > 0 {
                                ui.separator();
                            }
                            ui.add_space(6.0);
                            ui.label(theme::caption(name));
                            let color = if *highlight {
                                theme::VIOLET_700
                            } else {
                                theme::GRAY_900
                            };
                            ui.label(
                                egui::RichText::new(release.as_deref().unwrap_or("–")).color(color),
                            );
                            ui.add_space(6.0);
                        }
                    });
                });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let idle = self.busy.is_none();
                if let Some(pending) = self.snap.state.pending.clone() {
                    let force = self.force;
                    if ui
                        .add_enabled(idle, theme::primary(&format!("Activate {pending}")))
                        .clicked()
                    {
                        self.start("activating", move |root, _| {
                            Ok(activated(&root.activate(None, force)?))
                        });
                    }
                } else if let Some(published) = update {
                    let label = if self.snap.state.current.is_some() {
                        format!("Update to {published}")
                    } else {
                        format!("Install {published}")
                    };
                    if ui.add_enabled(idle, theme::primary(&label)).clicked() {
                        self.update();
                    }
                } else if ui
                    .add_enabled(
                        idle && self.snap.state.url.is_some(),
                        theme::secondary("Check for updates"),
                    )
                    .clicked()
                {
                    self.check();
                }
            });
        });
        if let Some(error) = &self.snap.error {
            ui.colored_label(theme::RED, error);
        }
    }

    /// The published release when it is not the active one.
    fn update_available(&self) -> Option<String> {
        self.published
            .clone()
            .filter(|p| Some(p) != self.snap.state.current.as_ref())
    }

    /// Manifest, installation folder and the less frequent operations.
    fn settings_card(&mut self, ui: &mut egui::Ui) {
        let idle = self.busy.is_none();
        theme::card(ui, |ui| {
            ui.label(theme::caption("MANIFEST"));
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.manifest_input)
                        .hint_text("https://…/suite.toml")
                        .margin(egui::vec2(8.0, 6.0))
                        .desired_width(ui.available_width() - 90.0),
                );
                let input = self.manifest_input.trim().to_string();
                if ui
                    .add_enabled(idle && !input.is_empty(), theme::primary("Follow"))
                    .clicked()
                {
                    self.published = None;
                    self.dismissed = None;
                    self.start("following the manifest", move |root, _| {
                        Ok(format!("following {}", root.init(&input)?))
                    });
                }
            });
            ui.add_space(6.0);
            ui.label(theme::caption("INSTALLED IN"));
            ui.label(
                egui::RichText::new(self.root.dir().display().to_string()).color(theme::GRAY_600),
            );
            if let Some(format) = self.snap.state.data_format {
                ui.label(
                    egui::RichText::new(format!("Shared data in format {format}"))
                        .color(theme::GRAY_500),
                );
            }
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                let force = self.force;
                if ui
                    .add_enabled(
                        idle && self.snap.state.previous.is_some(),
                        theme::secondary("Roll back"),
                    )
                    .on_hover_text("Go back to the previous release")
                    .clicked()
                {
                    self.start("rolling back", move |root, _| {
                        Ok(activated(&root.rollback(force)?))
                    });
                }
                if ui.add_enabled(idle, theme::secondary("Clean")).clicked() {
                    self.start("cleaning", |root, _| {
                        let (removed, cache) = root.clean()?;
                        let mut lines: Vec<String> = removed
                            .iter()
                            .map(|r| format!("removed release {r}"))
                            .collect();
                        lines.extend(cache_line(cache.as_ref()));
                        if lines.is_empty() {
                            lines.push("nothing to clean".into());
                        }
                        Ok(lines.join("\n"))
                    });
                }
                ui.checkbox(&mut self.force, "Allow a lower data format")
                    .on_hover_text(
                        "Activate or roll back even if the shared data may already be in a \
                         newer format. Only with a backup of the data.",
                    );
            });
        });
    }

    /// One card per tool of the active release, as many per row as fit.
    fn tool_cards(&mut self, ui: &mut egui::Ui) {
        if self.snap.tools.is_empty() {
            theme::card(ui, |ui| {
                ui.label(
                    egui::RichText::new("No release installed yet")
                        .font(theme::bold(18.0))
                        .color(theme::GRAY_900),
                );
                let hint = if self.snap.state.url.is_some() {
                    "Install the published release to see its tools here."
                } else {
                    "Enter the address of the suite's manifest in Settings."
                };
                ui.label(egui::RichText::new(hint).color(theme::GRAY_500));
            });
            return;
        }
        const GAP: f32 = 12.0;
        let width = ui.available_width();
        let columns = (((width + GAP) / (280.0 + GAP)).floor() as usize).max(1);
        let card_width = (width - GAP * (columns - 1) as f32) / columns as f32;
        let names: Vec<&str> = self.snap.tools.iter().map(|t| t.name.as_str()).collect();
        let shared = shared_prefix(&names);
        let tools = self.snap.tools.clone();
        let mut launch = None;
        for (row, chunk) in tools.chunks(columns).enumerate() {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = GAP;
                for (col, tool) in chunk.iter().enumerate() {
                    let index = row * columns + col;
                    // the row is horizontal: each card stacks its own content
                    let layout = egui::Layout::top_down(egui::Align::Min);
                    ui.allocate_ui_with_layout(egui::vec2(card_width, 0.0), layout, |ui| {
                        if self.tool_card(ui, index, tool, shared) {
                            launch = Some(tool.name.clone());
                        }
                    });
                }
            });
            ui.add_space(GAP - ui.spacing().item_spacing.y);
        }
        if let Some(tool) = launch {
            self.launch(tool);
        }
    }

    /// Returns whether Launch was pressed.
    fn tool_card(
        &mut self,
        ui: &mut egui::Ui,
        index: usize,
        tool: &ToolInfo,
        shared: usize,
    ) -> bool {
        let (fg, bg) = theme::ACCENTS[index % theme::ACCENTS.len()];
        let runs = self.running.iter().filter(|t| **t == tool.name).count();
        let mut launched = false;
        theme::card(ui, |ui| {
            ui.horizontal(|ui| {
                theme::pill(ui, &format!("{:02}", index + 1), fg, bg);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if runs > 0 {
                        let text = if runs == 1 {
                            "RUNNING".to_string()
                        } else {
                            format!("RUNNING ×{runs}")
                        };
                        theme::pill(ui, &text, theme::BLUE, theme::BLUE_50);
                    }
                });
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let own = &tool.name[shared..];
                let initial = own.chars().next().unwrap_or('?').to_ascii_uppercase();
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(44.0, 44.0), egui::Sense::hover());
                ui.painter().rect_filled(rect, 10.0, bg);
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    initial,
                    theme::bold(22.0),
                    fg,
                );
                ui.vertical(|ui| {
                    let mut name = egui::text::LayoutJob::default();
                    let font = theme::bold(20.0);
                    let part = |color| egui::TextFormat::simple(font.clone(), color);
                    name.append(&tool.name[..shared], 0.0, part(fg));
                    name.append(own, 0.0, part(theme::GRAY_900));
                    ui.label(name);
                    let mut details = Vec::new();
                    if let Some(v) = &tool.version {
                        details.push(format!("v{v}"));
                    }
                    if let Some(j) = &tool.java {
                        details.push(format!("Java {j}"));
                    }
                    ui.label(egui::RichText::new(details.join(" · ")).color(theme::GRAY_500));
                });
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| match self.update_available() {
                Some(p) if self.snap.state.current.is_some() => {
                    theme::pill(ui, "UPDATE AVAILABLE", theme::AMBER, theme::AMBER_50);
                    ui.label(egui::RichText::new(format!("release {p}")).color(theme::GRAY_500));
                }
                _ => {
                    theme::pill(ui, "INSTALLED", theme::EMERALD, theme::EMERALD_50);
                    if let Some(c) = &self.snap.state.current {
                        ui.label(
                            egui::RichText::new(format!("in release {c}")).color(theme::GRAY_500),
                        );
                    }
                }
            });
            ui.add_space(8.0);
            let open = self.args_open.as_deref() == Some(tool.name.as_str());
            if open {
                let args = self.args.entry(tool.name.clone()).or_default();
                ui.add(
                    egui::TextEdit::singleline(args)
                        .hint_text("arguments, separated by spaces")
                        .margin(egui::vec2(8.0, 6.0))
                        .desired_width(f32::INFINITY),
                );
            }
            ui.horizontal(|ui| {
                let more = 36.0;
                let width = ui.available_width() - more - ui.spacing().item_spacing.x;
                if ui
                    .add_enabled(
                        self.busy.is_none(),
                        theme::primary("Launch").min_size(egui::vec2(width, 34.0)),
                    )
                    .clicked()
                {
                    launched = true;
                }
                let has_args = self
                    .args
                    .get(&tool.name)
                    .is_some_and(|a| !a.trim().is_empty());
                if ui
                    .add(
                        theme::secondary(if has_args { "•••" } else { "…" })
                            .min_size(egui::vec2(more, 34.0)),
                    )
                    .on_hover_text("Arguments")
                    .clicked()
                {
                    self.args_open = if open { None } else { Some(tool.name.clone()) };
                }
            });
        });
        launched
    }

    /// Asks once per published release whether to update now.
    fn update_dialog(&mut self, ctx: &egui::Context) {
        let Some(published) = self.update_available() else {
            return;
        };
        if self.busy.is_some()
            || self.snap.state.pending.is_some()
            || self.dismissed.as_ref() == Some(&published)
        {
            return;
        }
        let suite = self
            .snap
            .suite
            .clone()
            .unwrap_or_else(|| "the suite".into());
        let first = self.snap.state.current.is_none();
        let mut choice = None;
        egui::Modal::new(egui::Id::unique("update"))
            .frame(theme::card_frame().inner_margin(24))
            .show(ctx, |ui| {
                ui.set_width(380.0);
                ui.label(
                    egui::RichText::new(format!("Release {published} is available"))
                        .font(theme::bold(20.0))
                        .color(theme::GRAY_900),
                );
                ui.add_space(6.0);
                let text = if first {
                    format!("Install {suite} {published} now?")
                } else {
                    format!(
                        "A new release of {suite} is published. Update now? Running tools \
                         keep working: the switch waits until they are closed."
                    )
                };
                ui.label(egui::RichText::new(text).color(theme::GRAY_600));
                ui.add_space(14.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let action = if first { "Install" } else { "Update now" };
                    if ui.add(theme::primary(action)).clicked() {
                        choice = Some(true);
                    }
                    if ui.add(theme::secondary("Later")).clicked() {
                        choice = Some(false);
                    }
                });
            });
        match choice {
            Some(true) => self.update(),
            Some(false) => self.dismissed = Some(published),
            None => {}
        }
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        let Some(step) = &self.busy else {
            if !self.running.is_empty() {
                ui.label(
                    egui::RichText::new(format!(
                        "Running: {}. Activation waits until they are closed.",
                        self.running.join(", ")
                    ))
                    .color(theme::GRAY_600),
                );
            }
            return;
        };
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(egui::RichText::new(step).color(theme::GRAY_700));
        });
        let total = self.transfer.total.load(Ordering::Relaxed);
        if total > 0 {
            let done = self.transfer.done.load(Ordering::Relaxed).min(total);
            let label = self.transfer.label.lock().unwrap().clone();
            ui.add(
                egui::ProgressBar::new(done as f32 / total as f32)
                    .fill(theme::VIOLET_600)
                    .corner_radius(100)
                    .text(
                        egui::RichText::new(format!(
                            "{label}: {} / {}",
                            HumanBytes(done),
                            HumanBytes(total)
                        ))
                        .color(egui::Color32::WHITE),
                    ),
            );
        }
    }

    fn log(&self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for (ok, line) in &self.log {
                    let color = if *ok { theme::GRAY_600 } else { theme::RED };
                    ui.label(
                        egui::RichText::new(line)
                            .monospace()
                            .size(12.0)
                            .color(color),
                    );
                }
            });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.receive();
        let ctx = ui.ctx().clone();
        egui::Panel::top("top")
            .frame(
                egui::Frame::new()
                    .fill(theme::WHITE)
                    .inner_margin(egui::Margin::symmetric(20, 12)),
            )
            .show(ui, |ui| self.top_bar(ui));
        egui::Panel::bottom("status")
            .resizable(true)
            .min_size(72.0)
            .frame(
                egui::Frame::new()
                    .fill(theme::WHITE)
                    .inner_margin(egui::Margin::symmetric(20, 10)),
            )
            .show(ui, |ui| {
                self.status_bar(ui);
                self.log(ui);
            });
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::GRAY_50)
                    .inner_margin(egui::Margin::same(20)),
            )
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    self.status_row(ui);
                    ui.add_space(12.0);
                    if self.settings || self.snap.state.url.is_none() {
                        self.settings_card(ui);
                        ui.add_space(12.0);
                    }
                    self.tool_cards(ui);
                });
            });
        self.update_dialog(&ctx);
    }
}

/// Length of the name prefix all tools share ("kubi" in kubirag and kubibound), drawn
/// in the card's color; 0 when nothing would be left of some name.
fn shared_prefix(names: &[&str]) -> usize {
    let [first, rest @ ..] = names else {
        return 0;
    };
    if rest.is_empty() {
        return 0;
    }
    let mut len = first.len();
    for name in rest {
        len = first
            .char_indices()
            .zip(name.chars())
            .take_while(|((_, a), b)| a == b)
            .last()
            .map_or(0, |((i, a), _)| i + a.len_utf8())
            .min(len);
    }
    if names.iter().any(|n| n.len() == len) {
        0
    } else {
        len
    }
}

/// Colors and shapes of the window: light, violet accents, rounded corners.
mod theme {
    use std::sync::Arc;

    use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Margin, RichText, Stroke};

    pub const WHITE: Color32 = Color32::WHITE;
    pub const GRAY_50: Color32 = Color32::from_rgb(0xf9, 0xfa, 0xfb);
    pub const GRAY_100: Color32 = Color32::from_rgb(0xf3, 0xf4, 0xf6);
    pub const GRAY_200: Color32 = Color32::from_rgb(0xe5, 0xe7, 0xeb);
    pub const GRAY_500: Color32 = Color32::from_rgb(0x6b, 0x72, 0x80);
    pub const GRAY_600: Color32 = Color32::from_rgb(0x4b, 0x55, 0x63);
    pub const GRAY_700: Color32 = Color32::from_rgb(0x37, 0x41, 0x51);
    pub const GRAY_900: Color32 = Color32::from_rgb(0x11, 0x18, 0x27);
    pub const VIOLET_50: Color32 = Color32::from_rgb(0xf5, 0xf3, 0xff);
    pub const VIOLET_100: Color32 = Color32::from_rgb(0xed, 0xe9, 0xfe);
    pub const VIOLET_600: Color32 = Color32::from_rgb(0x7c, 0x3a, 0xed);
    pub const VIOLET_700: Color32 = Color32::from_rgb(0x6d, 0x28, 0xd9);
    pub const BLUE: Color32 = Color32::from_rgb(0x25, 0x63, 0xeb);
    pub const BLUE_50: Color32 = Color32::from_rgb(0xef, 0xf6, 0xff);
    pub const AMBER: Color32 = Color32::from_rgb(0xd9, 0x77, 0x06);
    pub const AMBER_50: Color32 = Color32::from_rgb(0xff, 0xfb, 0xeb);
    pub const EMERALD: Color32 = Color32::from_rgb(0x05, 0x96, 0x69);
    pub const EMERALD_50: Color32 = Color32::from_rgb(0xec, 0xfd, 0xf5);
    pub const RED: Color32 = Color32::from_rgb(0xdc, 0x26, 0x26);

    /// (text, background) of each tool card, in order.
    pub const ACCENTS: [(Color32, Color32); 6] = [
        (VIOLET_600, VIOLET_50),
        (
            Color32::from_rgb(0x0d, 0x94, 0x88),
            Color32::from_rgb(0xf0, 0xfd, 0xfa),
        ),
        (BLUE, BLUE_50),
        (AMBER, AMBER_50),
        (EMERALD, EMERALD_50),
        (
            Color32::from_rgb(0xdb, 0x27, 0x77),
            Color32::from_rgb(0xfd, 0xf2, 0xf8),
        ),
    ];

    pub fn apply(ctx: &egui::Context) {
        load_fonts(ctx);
        ctx.set_theme(egui::Theme::Light);
        ctx.all_styles_mut(|style| {
            let v = &mut style.visuals;
            *v = egui::Visuals::light();
            v.panel_fill = GRAY_50;
            v.window_fill = WHITE;
            v.window_stroke = Stroke::new(1.0, GRAY_200);
            v.window_corner_radius = CornerRadius::same(14);
            v.menu_corner_radius = CornerRadius::same(8);
            v.extreme_bg_color = WHITE;
            v.faint_bg_color = GRAY_100;
            v.hyperlink_color = VIOLET_700;
            v.selection.bg_fill = VIOLET_100;
            v.selection.stroke = Stroke::new(1.0, VIOLET_700);
            let w = &mut v.widgets;
            for state in [
                &mut w.noninteractive,
                &mut w.inactive,
                &mut w.hovered,
                &mut w.active,
                &mut w.open,
            ] {
                state.corner_radius = CornerRadius::same(8);
            }
            w.noninteractive.bg_stroke = Stroke::new(1.0, GRAY_200);
            w.noninteractive.fg_stroke = Stroke::new(1.0, GRAY_700);
            w.inactive.bg_fill = WHITE;
            w.inactive.weak_bg_fill = WHITE;
            w.inactive.bg_stroke = Stroke::new(1.0, GRAY_200);
            w.inactive.fg_stroke = Stroke::new(1.0, GRAY_700);
            w.hovered.bg_fill = VIOLET_50;
            w.hovered.weak_bg_fill = VIOLET_50;
            w.hovered.bg_stroke = Stroke::new(1.0, VIOLET_600);
            w.hovered.fg_stroke = Stroke::new(1.0, GRAY_900);
            w.hovered.expansion = 0.0;
            w.active.bg_fill = VIOLET_100;
            w.active.weak_bg_fill = VIOLET_100;
            w.active.bg_stroke = Stroke::new(1.0, VIOLET_700);
            w.active.expansion = 0.0;
            style.spacing.button_padding = egui::vec2(14.0, 6.0);
            style.spacing.item_spacing = egui::vec2(8.0, 8.0);
            style.spacing.interact_size.y = 30.0;
        });
    }

    /// The system's sans-serif with its bold, when found (egui's own font has no bold):
    /// Segoe UI on Windows, Arial on macOS, DejaVu or Noto on Linux.
    fn load_fonts(ctx: &egui::Context) {
        const CANDIDATES: &[(&str, &str)] = &[
            (
                r"C:\Windows\Fonts\segoeui.ttf",
                r"C:\Windows\Fonts\segoeuib.ttf",
            ),
            (
                "/System/Library/Fonts/Supplemental/Arial.ttf",
                "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
            ),
            (
                "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
                "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
            ),
            (
                "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
                "/usr/share/fonts/truetype/noto/NotoSans-Bold.ttf",
            ),
        ];
        let mut fonts = egui::FontDefinitions::default();
        let mut fallback = fonts.families[&FontFamily::Proportional].clone();
        let found = CANDIDATES.iter().find_map(|(regular, bold)| {
            Some((std::fs::read(regular).ok()?, std::fs::read(bold).ok()?))
        });
        let mut bold = Vec::new();
        if let Some((regular, heavy)) = found {
            for (name, data) in [("system", regular), ("system-bold", heavy)] {
                fonts
                    .font_data
                    .insert(name.into(), Arc::new(egui::FontData::from_owned(data)));
            }
            fallback.insert(0, "system".into());
            fonts
                .families
                .insert(FontFamily::Proportional, fallback.clone());
            bold.push("system-bold".into());
        }
        bold.extend(fallback);
        fonts.families.insert(FontFamily::Name("bold".into()), bold);
        ctx.set_fonts(fonts);
    }

    /// Bold text, or the regular font when the system has no bold one.
    pub fn bold(size: f32) -> FontId {
        FontId::new(size, FontFamily::Name("bold".into()))
    }

    /// A card as wide as the space left, borders included.
    pub fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
        let frame = card_frame();
        let width = ui.available_width() - frame.total_margin().sum().x;
        frame
            .show(ui, |ui| {
                ui.set_width(width);
                add(ui)
            })
            .inner
    }

    /// White card with a light border.
    pub fn card_frame() -> egui::Frame {
        egui::Frame::new()
            .fill(WHITE)
            .stroke(Stroke::new(1.0, GRAY_200))
            .corner_radius(14)
            .inner_margin(Margin::same(16))
    }

    /// Main action: violet, white text.
    pub fn primary(text: &str) -> egui::Button<'static> {
        egui::Button::new(RichText::new(text).font(bold(14.0)).color(WHITE))
            .fill(VIOLET_700)
            .stroke(Stroke::NONE)
            .corner_radius(8)
            .min_size(egui::vec2(0.0, 32.0))
    }

    /// Other actions: white with a border, violet when hovered.
    pub fn secondary(text: &str) -> egui::Button<'static> {
        egui::Button::new(RichText::new(text).color(GRAY_700))
            .corner_radius(8)
            .min_size(egui::vec2(0.0, 32.0))
    }

    /// Small uppercase label above or beside a value.
    pub fn caption(text: &str) -> RichText {
        RichText::new(text).font(bold(11.0)).color(GRAY_500)
    }

    /// Rounded badge: colored text on a pale background.
    pub fn pill(ui: &mut egui::Ui, text: &str, fg: Color32, bg: Color32) {
        egui::Frame::new()
            .fill(bg)
            .corner_radius(100)
            .inner_margin(Margin::symmetric(8, 2))
            .show(ui, |ui| {
                ui.label(RichText::new(text).font(bold(11.0)).color(fg));
            });
    }
}

fn activated(done: &Activated) -> String {
    let mut lines = vec![format!("release {} is active", done.release)];
    lines.extend(done.removed.iter().map(|r| format!("removed release {r}")));
    lines.extend(cache_line(done.cache.as_ref()));
    lines.join("\n")
}

fn cache_line(cache: Option<&jarexec::clean::Report>) -> Option<String> {
    match cache {
        Some(r) if r.files > 0 => Some(format!(
            "removed {} unused files ({}) from the cache",
            r.files,
            HumanBytes(r.bytes)
        )),
        Some(_) => None,
        None => Some("cache cleanup skipped: a tool is being prepared".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::shared_prefix;

    #[test]
    fn shared_prefix_leaves_something_of_every_name() {
        assert_eq!(shared_prefix(&["kubirag", "kubibound"]), 4);
        assert_eq!(shared_prefix(&["tool-a", "tool-b"]), 5);
        assert_eq!(shared_prefix(&["alpha", "beta"]), 0);
        assert_eq!(shared_prefix(&["tool", "toolbox"]), 0);
        assert_eq!(shared_prefix(&["only"]), 0);
        assert_eq!(shared_prefix(&[]), 0);
    }
}
