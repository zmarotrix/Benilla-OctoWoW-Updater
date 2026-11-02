//! The OctoWoW updater for Benilla: a small window that brings its copy of the OctoWoW game files
//! up to date, then starts the game. Benilla itself is not updated here; a new Benilla comes from
//! its own releases. It writes only inside its own folder ([`install`]).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod install;
mod sync;
mod torrent;
mod verify;
mod webseed;

use eframe::egui;
use install::Install;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use sync::{human, GameFiles, Plan};
use torrent::Torrent;

pub const USER_AGENT: &str = concat!("Benilla-Launcher/", env!("CARGO_PKG_VERSION"));

#[derive(Clone, Copy, Default, PartialEq)]
enum Stage {
    #[default]
    Working,
    /// No game files yet: the player picks download or copy.
    NeedGameFiles,
    Ready,
}

#[derive(Default)]
struct Status {
    stage: Stage,
    line: String,
    progress: Option<f32>,
    detail: String,
    error: Option<String>,
    game: String,
    needed: u64,
    log: Vec<String>,
}

enum Job {
    Check,
    Download,
    /// Copy from another install: the one given, or one the player picks.
    Import(Option<PathBuf>),
    Repair,
}

/// What the worker thread reports through: the window, which redraws on every change, or with
/// `--update` the terminal.
#[derive(Clone)]
struct Report {
    status: Arc<Mutex<Status>>,
    ctx: Option<egui::Context>,
}

impl Report {
    fn set(&self, f: impl FnOnce(&mut Status)) {
        f(&mut self.status.lock().unwrap());
        if let Some(ctx) = &self.ctx {
            ctx.request_repaint();
        }
    }

    fn line(&self, text: impl Into<String>) {
        let text = text.into();
        if self.ctx.is_none() {
            println!("{text}");
        }
        self.set(|s| {
            s.log.push(text.clone());
            (s.line, s.progress, s.detail) = (text, None, String::new());
        });
    }

    fn log(&self, text: String) {
        if self.ctx.is_none() {
            println!("  {text}");
        }
        self.set(|s| s.log.push(text));
    }

    fn bar(&self, done: u64, total: u64, speed: u64) {
        let frac = if total == 0 {
            0.0
        } else {
            done as f32 / total as f32
        };
        let mut detail = format!("{} of {}", human(done), human(total));
        if speed > 0 {
            detail += &format!(" · {}/s", human(speed));
        }
        self.set(|s| (s.progress, s.detail) = (Some(frac), detail));
    }
}

struct Worker {
    install: Install,
    files: GameFiles,
    report: Report,
    cancel: Arc<AtomicBool>,
    release: Option<(Torrent, Plan)>,
}

impl Worker {
    fn run(mut self, jobs: Receiver<Job>) {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        for job in jobs {
            self.cancel.store(false, Ordering::Relaxed);
            self.report
                .set(|s| (s.stage, s.error) = (Stage::Working, None));
            let result = rt.block_on(async {
                match job {
                    Job::Check => self.check_game(false).await,
                    Job::Download => self.download_game(false).await,
                    Job::Import(from) => self.import(from).await,
                    Job::Repair => self.check_game(true).await,
                }
            });
            if let Err(e) = result {
                self.report.log(format!("Error: {e}"));
                self.report.set(|s| s.error = Some(e));
            }
            self.settle();
        }
    }

    /// The stage once a job is over: ready to play when there's something to play.
    fn settle(&self) {
        let has_data = self
            .release
            .as_ref()
            .map_or(self.install.data_dir().is_dir(), |(_, p)| !p.not_installed);
        let stage = if has_data {
            Stage::Ready
        } else {
            Stage::NeedGameFiles
        };
        self.report.set(|s| {
            s.stage = stage;
            (s.progress, s.detail) = (None, String::new());
            s.line = match stage {
                Stage::Ready if s.error.is_none() => "Ready to play".into(),
                Stage::Ready => "Ready to play, but something went wrong (see below)".into(),
                _ => "Benilla needs the OctoWoW game files".into(),
            };
        });
    }

    async fn fetch(&self) -> Result<Torrent, String> {
        self.report.line("Checking the OctoWoW game files…");
        self.files.fetch_release().await
    }

    /// Plans against the release on a blocking thread (it may hash gigabytes).
    async fn analyze(&self, t: Torrent, deep: bool) -> Result<(Torrent, Plan), String> {
        if deep {
            self.report.line("Checking every game file…");
        }
        let (files, report, cancel) = (
            GameFiles::new(self.install.state_dir(), self.install.game_dir()),
            self.report.clone(),
            self.cancel.clone(),
        );
        tokio::task::spawn_blocking(move || {
            let plan = files.analyze(
                &t,
                deep,
                &|m| report.log(m),
                &|d, total| report.bar(d, total, 0),
                &cancel,
            )?;
            Ok((t, plan))
        })
        .await
        .map_err(|e| e.to_string())?
    }

    async fn check_game(&mut self, deep: bool) -> Result<(), String> {
        let t = match self.fetch().await {
            Ok(t) => t,
            Err(e) => {
                self.report
                    .set(|s| s.game = "Game files: couldn't check for updates".into());
                return Err(e);
            }
        };
        let (t, plan) = self.analyze(t, deep).await?;
        let not_installed = plan.not_installed;
        self.report.set(|s| s.needed = plan.download_bytes);
        self.release = Some((t, plan));
        if not_installed {
            self.report
                .set(|s| s.game = "Game files: not installed".into());
            return Ok(());
        }
        self.download_game(deep).await
    }

    async fn download_game(&mut self, verified: bool) -> Result<(), String> {
        let Some((t, plan)) = &self.release else {
            return Err("Nothing to download yet.".into());
        };
        if !plan.up_to_date() {
            self.report.line(if plan.release_changed {
                "Updating the game files…"
            } else {
                "Downloading the game files…"
            });
            let report = self.report.clone();
            let got = self
                .files
                .download(t, plan, self.cancel.clone(), &|m| report.log(m), &|p| {
                    report.bar(p.done, p.total, p.speed)
                })
                .await;
            if let Err(e) = got {
                self.report
                    .set(|s| s.game = "Game files: update not finished".into());
                return Err(e);
            }
            self.report.log("Game files are up to date".into());
        }
        self.files.save_baseline(verified)?;
        if let Some((_, plan)) = &mut self.release {
            plan.files.iter_mut().for_each(|(_, f)| f.download = false);
            plan.obsolete.clear();
            plan.not_installed = false;
        }
        self.report
            .set(|s| s.game = "Game files: up to date".into());
        Ok(())
    }

    async fn import(&mut self, from: Option<PathBuf>) -> Result<(), String> {
        let picked = match from {
            Some(path) => path,
            None => {
                self.report.line("Pick the OctoWoW install to copy from…");
                let Some(picked) = rfd::AsyncFileDialog::new()
                    .set_title("Pick an OctoWoW install (the folder with WoW.exe)")
                    .pick_folder()
                    .await
                else {
                    return Ok(());
                };
                picked.path().to_path_buf()
            }
        };
        let source = install::client_root_of(&picked).ok_or_else(|| {
            format!(
                "{} isn't an OctoWoW install (it has no Data folder).",
                picked.display()
            )
        })?;
        let t = match self.release.take() {
            Some((t, _)) => t,
            None => self.fetch().await?,
        };
        let files: Vec<(String, u64)> = t
            .files
            .iter()
            .filter(|f| install::wanted(&f.path))
            .map(|f| (f.path.clone(), f.length))
            .collect();
        self.report
            .line(format!("Copying game files from {}…", source.display()));
        let (dest, report, cancel) = (
            self.install.game_dir(),
            self.report.clone(),
            self.cancel.clone(),
        );
        let copied = tokio::task::spawn_blocking(move || {
            install::import(
                &source,
                &dest,
                &files,
                &|d, total| report.bar(d, total, 0),
                &cancel,
            )
        })
        .await
        .map_err(|e| e.to_string())??;
        self.report.log(format!(
            "Copied {copied} file(s); checking them against the release"
        ));
        // A full check: a copy says nothing about whether those files were intact.
        let (t, plan) = self.analyze(t, true).await?;
        self.release = Some((t, plan));
        self.download_game(true).await
    }
}

struct App {
    install: Install,
    status: Arc<Mutex<Status>>,
    jobs: Sender<Job>,
    cancel: Arc<AtomicBool>,
}

impl App {
    fn new(cc: &eframe::CreationContext, install: Install) -> Self {
        let status = Arc::new(Mutex::new(Status {
            game: "Game files: checking…".into(),
            ..Default::default()
        }));
        let cancel = Arc::new(AtomicBool::new(false));
        let (jobs, rx) = channel();
        let worker = Worker {
            files: GameFiles::new(install.state_dir(), install.game_dir()),
            install: install.clone(),
            report: Report {
                status: status.clone(),
                ctx: Some(cc.egui_ctx.clone()),
            },
            cancel: cancel.clone(),
            release: None,
        };
        std::thread::spawn(move || worker.run(rx));
        let _ = jobs.send(Job::Check);
        Self {
            install,
            status,
            jobs,
            cancel,
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut status = self.status.lock().unwrap();
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(6.0);
            ui.heading("OctoWoW Updater for Benilla");
            ui.label(egui::RichText::new(&status.game).weak());
            ui.separator();
            ui.add_space(4.0);
            ui.label(egui::RichText::new(&status.line).strong().size(16.0));
            if let Some(p) = status.progress {
                ui.add(egui::ProgressBar::new(p).show_percentage());
                ui.label(&status.detail);
            }
            if let Some(e) = &status.error {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            ui.add_space(8.0);

            let big = |text: &str| egui::Button::new(egui::RichText::new(text).size(18.0)).min_size(egui::vec2(160.0, 40.0));
            match status.stage {
                Stage::Working => {
                    if ui.button("Cancel").clicked() {
                        self.cancel.store(true, Ordering::Relaxed);
                    }
                    ui.label(egui::RichText::new("Downloads pick up where they stopped next time.").weak());
                }
                Stage::NeedGameFiles => {
                    ui.label(format!("Copying from an OctoWoW install you already have is much faster than downloading {}. That install is only read, never changed.", human(status.needed.max(1))));
                    ui.horizontal(|ui| {
                        if ui.add(big("Copy from an install…")).clicked() {
                            let _ = self.jobs.send(Job::Import(None));
                        }
                        if ui.add(big("Download")).clicked() {
                            let _ = self.jobs.send(Job::Download);
                        }
                    });
                }
                Stage::Ready => {
                    ui.horizontal(|ui| {
                        let can_play = self.install.game_exe().is_file();
                        if ui.add_enabled(can_play, big("Play")).clicked() {
                            match self.install.launch() {
                                Ok(()) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                                Err(e) => status.error = Some(e),
                            }
                        }
                        ui.vertical(|ui| {
                            if ui.button("Check for updates").clicked() {
                                let _ = self.jobs.send(Job::Check);
                            }
                            if ui.button("Repair game files").on_hover_text("Checks every game file and re-downloads any that are damaged").clicked() {
                                let _ = self.jobs.send(Job::Repair);
                            }
                        });
                    });
                    if !self.install.game_exe().is_file() {
                        ui.label(format!("To play, put {} from a Benilla release in this folder:", install::game_exe_name()));
                        ui.hyperlink(install::BENILLA_RELEASES);
                    }
                }
            }

            ui.add_space(8.0);
            egui::CollapsingHeader::new("Details").show(ui, |ui| {
                ui.label(egui::RichText::new(format!("Install folder: {}", self.install.root.display())).weak());
                egui::ScrollArea::vertical().max_height(140.0).stick_to_bottom(true).show(ui, |ui| {
                    for l in &status.log {
                        ui.label(egui::RichText::new(l).monospace().size(11.0));
                    }
                });
            });
        });
    }
}

/// `--update [--download | --import <install>]`: the window's work without the window, for a
/// terminal or SSH session. A first install needs `--download` or `--import`.
fn headless(args: &[String]) -> std::process::ExitCode {
    let install = match Install::locate() {
        Ok(i) => i,
        Err(e) => {
            eprintln!("{e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let status = Arc::new(Mutex::new(Status::default()));
    let (jobs, rx) = channel();
    let _ = jobs.send(Job::Check);
    if let Some(at) = args.iter().position(|a| a == "--import") {
        let Some(from) = args.get(at + 1) else {
            eprintln!("--import needs the folder of an OctoWoW install");
            return std::process::ExitCode::FAILURE;
        };
        let _ = jobs.send(Job::Import(Some(PathBuf::from(from))));
    } else if args.iter().any(|a| a == "--download") {
        let _ = jobs.send(Job::Download);
    }
    drop(jobs);
    let worker = Worker {
        files: GameFiles::new(install.state_dir(), install.game_dir()),
        install,
        report: Report {
            status: status.clone(),
            ctx: None,
        },
        cancel: Arc::new(AtomicBool::new(false)),
        release: None,
    };
    worker.run(rx);
    let s = status.lock().unwrap();
    println!("{}\n{}", s.line, s.game);
    if s.stage == Stage::NeedGameFiles {
        println!("Run again with --download, or --import <an OctoWoW install> to copy its files.");
    }
    if s.error.is_some() || s.stage != Stage::Ready {
        std::process::ExitCode::FAILURE
    } else {
        std::process::ExitCode::SUCCESS
    }
}

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--update") {
        return headless(&args);
    }
    match window() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Couldn't open the launcher window: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn window() -> eframe::Result {
    let install = Install::locate();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("OctoWoW Updater for Benilla")
            .with_inner_size([600.0, 380.0])
            .with_min_inner_size([480.0, 300.0]),
        ..Default::default()
    };
    eframe::run_native(
        "OctoWoW Updater for Benilla",
        options,
        Box::new(move |cc| match install {
            Ok(install) => Ok(Box::new(App::new(cc, install)) as Box<dyn eframe::App>),
            Err(e) => Ok(Box::new(Refusal(e)) as Box<dyn eframe::App>),
        }),
    )
}

/// The window when the launcher's folder is one it must not write into.
struct Refusal(String);

impl eframe::App for Refusal {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("OctoWoW Updater for Benilla");
            ui.add_space(8.0);
            ui.colored_label(ui.visuals().error_fg_color, &self.0);
            ui.add_space(8.0);
            if ui.button("Close").clicked() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    }
}
