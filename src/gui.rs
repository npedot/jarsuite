//! Desktop window over the same operations as the CLI: status, update, activate,
//! rollback, clean and launching tools.
//!
//! One operation runs at a time on a worker thread. It reports its steps through a
//! channel and its transfers (archive, JRE, dependencies) through a
//! [`jarexec::http::ProgressSink`], so the window stays responsive while it works.

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
            .with_inner_size([560.0, 520.0])
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
    tools: Vec<String>,
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
                snap.tools = manifest.tools;
            }
            Ok(())
        })();
        snap.error = loaded.err().map(|e| format!("{e:#}"));
        snap
    }
}

enum Msg {
    Step(String),
    Done(Result<String, String>),
    /// The published release, from a check.
    Published(String),
    ToolExited(String, Result<i32, String>),
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
    args_input: String,
    force: bool,
}

impl App {
    fn new(root: Root, ctx: egui::Context) -> Self {
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
            args_input: String::new(),
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
            .args_input
            .split_whitespace()
            .map(OsString::from)
            .collect();
        self.running.push(tool.clone());
        self.log.push((true, format!("started {tool}")));
        let (root, tx, ctx) = (self.root.clone(), self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let result = crate::run::run(&root, &tool, &args).map_err(|e| format!("{e:#}"));
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
                        Ok(0) => (true, format!("{tool} finished")),
                        Ok(code) => (false, format!("{tool} exited with code {code}")),
                        Err(e) => (false, format!("{tool}: {e}")),
                    });
                }
            }
        }
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        let title = self.snap.suite.as_deref().unwrap_or("jarsuite");
        ui.heading(title);
        ui.label(egui::RichText::new(self.root.dir().display().to_string()).weak());
        ui.add_space(4.0);

        let idle = self.busy.is_none();
        egui::CollapsingHeader::new("Manifest")
            .default_open(self.snap.state.url.is_none())
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.manifest_input)
                            .hint_text("https://…/suite.toml")
                            .desired_width(ui.available_width() - 70.0),
                    );
                    let input = self.manifest_input.trim().to_string();
                    if ui
                        .add_enabled(idle && !input.is_empty(), egui::Button::new("Follow"))
                        .clicked()
                    {
                        self.published = None;
                        self.start("following the manifest", move |root, _| {
                            Ok(format!("following {}", root.init(&input)?))
                        });
                    }
                });
            });
    }

    fn releases(&mut self, ui: &mut egui::Ui) {
        let state = &self.snap.state;
        let show = |r: &Option<String>| r.clone().unwrap_or_else(|| "–".into());
        egui::Grid::new("releases")
            .num_columns(2)
            .spacing([16.0, 4.0])
            .show(ui, |ui| {
                ui.label("Active");
                ui.strong(show(&state.current));
                ui.end_row();
                ui.label("Published");
                let published = match (&self.published, &state.current) {
                    (Some(p), Some(c)) if p == c => format!("{p} (up to date)"),
                    (Some(p), _) => format!("{p} (update available)"),
                    (None, _) => "–".into(),
                };
                ui.label(published);
                ui.end_row();
                ui.label("Ready to activate");
                ui.label(show(&state.pending));
                ui.end_row();
                ui.label("Previous");
                ui.label(show(&state.previous));
                ui.end_row();
                if let Some(format) = state.data_format {
                    ui.label("Data format");
                    ui.label(format.to_string());
                    ui.end_row();
                }
            });
        if let Some(error) = &self.snap.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }

        ui.add_space(8.0);
        let idle = self.busy.is_none();
        let has_url = self.snap.state.url.is_some();
        let pending = self.snap.state.pending.is_some();
        let previous = self.snap.state.previous.is_some();
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(idle && has_url, egui::Button::new("Check"))
                .clicked()
            {
                self.check();
            }
            if ui
                .add_enabled(idle && has_url, egui::Button::new("Update"))
                .on_hover_text(
                    "Download, verify and prepare the published release, then activate it",
                )
                .clicked()
            {
                self.update();
            }
            let force = self.force;
            if ui
                .add_enabled(idle && pending, egui::Button::new("Activate"))
                .clicked()
            {
                self.start("activating", move |root, _| {
                    Ok(activated(&root.activate(None, force)?))
                });
            }
            if ui
                .add_enabled(idle && previous, egui::Button::new("Roll back"))
                .on_hover_text("Go back to the previous release")
                .clicked()
            {
                self.start("rolling back", move |root, _| {
                    Ok(activated(&root.rollback(force)?))
                });
            }
            if ui.add_enabled(idle, egui::Button::new("Clean")).clicked() {
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
        });
        ui.checkbox(&mut self.force, "Allow a lower data format")
            .on_hover_text(
                "Activate or roll back even if the shared data may already be in a newer \
                 format. Only with a backup of the data.",
            );
    }

    fn tools(&mut self, ui: &mut egui::Ui) {
        ui.heading("Tools");
        if self.snap.tools.is_empty() {
            ui.label("No active release: press Update.");
            return;
        }
        ui.horizontal(|ui| {
            ui.label("Arguments");
            ui.add(
                egui::TextEdit::singleline(&mut self.args_input)
                    .hint_text("passed to the tool, separated by spaces")
                    .desired_width(f32::INFINITY),
            );
        });
        let mut launch = None;
        for tool in &self.snap.tools {
            ui.horizontal(|ui| {
                let runs = self.running.iter().filter(|t| *t == tool).count();
                if ui
                    .add_enabled(self.busy.is_none(), egui::Button::new("Launch"))
                    .clicked()
                {
                    launch = Some(tool.clone());
                }
                ui.label(tool);
                if runs > 0 {
                    ui.label(egui::RichText::new(format!("running ({runs})")).weak());
                }
            });
        }
        if let Some(tool) = launch {
            self.launch(tool);
        }
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        let Some(step) = &self.busy else {
            if !self.running.is_empty() {
                ui.label(format!(
                    "Running: {}. Activation waits until they are closed.",
                    self.running.join(", ")
                ));
            }
            return;
        };
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(step);
        });
        let total = self.transfer.total.load(Ordering::Relaxed);
        if total > 0 {
            let done = self.transfer.done.load(Ordering::Relaxed).min(total);
            let label = self.transfer.label.lock().unwrap().clone();
            ui.add(
                egui::ProgressBar::new(done as f32 / total as f32).text(format!(
                    "{label}: {} / {}",
                    HumanBytes(done),
                    HumanBytes(total)
                )),
            );
        }
    }

    fn log(&self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for (ok, line) in &self.log {
                    if *ok {
                        ui.label(line);
                    } else {
                        ui.colored_label(ui.visuals().error_fg_color, line);
                    }
                }
            });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.receive();
        egui::Panel::bottom("status")
            .resizable(true)
            .min_size(80.0)
            .show(ui, |ui| {
                ui.add_space(4.0);
                self.status_bar(ui);
                self.log(ui);
            });
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                self.header(ui);
                ui.separator();
                self.releases(ui);
                ui.separator();
                self.tools(ui);
            });
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
