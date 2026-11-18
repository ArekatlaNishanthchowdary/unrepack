//! unrepack-gui: a window over `unrepack.exe` for people who don't use a
//! command line. It runs the CLI next to it as a hidden child process and
//! shows its output, so every check and refusal is the CLI's own.
//! ponytail: no shared library with the CLI; the child process is the API.

#![windows_subsystem = "windows"]

use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;

use eframe::egui::{self, Color32, RichText};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const MAX_LOG: usize = 5000;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Keep,
    DeleteEach,
    FreeAsRead,
}

/// The CLI's arguments for one run. `--yes` because the window asks instead.
fn cli_args(repack: &str, out: &str, decoders: &str, mode: Mode, check: bool) -> Vec<String> {
    let mut a: Vec<String> = vec!["extract".into(), repack.into(), "-o".into(), out.into()];
    if !decoders.is_empty() {
        a.extend(["--decoders".into(), decoders.into()]);
    }
    a.push(
        match mode {
            // Nothing can be lost when the repack is kept, and extracting
            // checks every CRC anyway, so skip the separate checking pass.
            Mode::Keep => "--no-verify",
            Mode::DeleteEach => "--delete-each",
            Mode::FreeAsRead => "--free-as-read",
        }
        .into(),
    );
    a.push(if check { "--check" } else { "--yes" }.into());
    a
}

/// `@progress <phase> <done> <total>`, which the CLI prints once a second
/// when `UNREPACK_GUI` is set.
fn parse_progress(line: &str) -> Option<(String, u64, u64)> {
    let mut it = line.strip_prefix("@progress ")?.split_whitespace();
    let phase = it.next()?.to_string();
    Some((phase, it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

fn human(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let (mut v, mut u) = (b as f64, 0);
    while v >= 1000.0 && u < 4 {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 {
        format!("{b} B")
    } else {
        format!("{v:.2} {}", U[u])
    }
}

/// The repack's decoders sit next to its data files only if someone put them
/// there; most repacks keep them inside Setup.exe.
fn has_decoders(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|rd| {
        rd.filter_map(|e| e.ok()).any(|e| {
            let n = e.file_name().to_string_lossy().to_lowercase();
            n == "arc.ini" || n == "cls.ini" || (n.starts_with("cls-") && n.ends_with(".dll"))
        })
    })
}

fn is_admin() -> bool {
    unsafe { windows_sys::Win32::UI::Shell::IsUserAnAdmin() != 0 }
}

fn restart_as_admin() -> bool {
    use std::os::windows::ffi::OsStrExt;
    let wide = |s: &std::ffi::OsStr| s.encode_wide().chain([0]).collect::<Vec<u16>>();
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let (verb, file) = (wide("runas".as_ref()), wide(exe.as_os_str()));
    let r = unsafe {
        windows_sys::Win32::UI::Shell::ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1, // SW_SHOWNORMAL
        )
    };
    r as usize > 32
}

struct Run {
    child: Child,
    rx: Receiver<String>,
    check_only: bool,
    mode: Mode,
    /// The CLI reached the pass that changes the repack.
    extracting: bool,
    stopped: bool,
    exited: Option<bool>,
    pipes_closed: bool,
}

enum Outcome {
    Ok(String),
    Failed(String),
    Stopped(String),
}

#[derive(Clone, Copy, PartialEq)]
enum Ask {
    Extract,
    Stop,
    Close,
}

struct App {
    repack: String,
    out: String,
    decoders: String,
    mode: Mode,
    log: Vec<String>,
    progress: Option<(String, u64, u64)>,
    run: Option<Run>,
    outcome: Option<Outcome>,
    ask: Option<Ask>,
    closing: bool,
    admin: bool,
    /// The last run unpacked the game, so there is a folder to open.
    unpacked: bool,
}

impl App {
    fn new() -> Self {
        App {
            repack: String::new(),
            out: String::new(),
            decoders: String::new(),
            mode: Mode::Keep,
            log: Vec::new(),
            progress: None,
            run: None,
            outcome: None,
            ask: None,
            closing: false,
            admin: is_admin(),
            unpacked: false,
        }
    }

    fn picked_repack(&mut self, dir: &Path) {
        self.repack = dir.display().to_string();
        if has_decoders(dir) {
            self.decoders = self.repack.clone();
        }
        if self.out.is_empty() {
            // "Game [DODI Repack]" -> "Game", next to the repack: the same
            // drive, so freeing the repack helps.
            let name = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let name = name.split(" [").next().unwrap_or(&name).trim().to_string();
            if let Some(parent) = dir.parent().filter(|_| !name.is_empty()) {
                self.out = parent
                    .join(format!("{name} (unpacked)"))
                    .display()
                    .to_string();
            }
        }
    }

    fn start(&mut self, ctx: &egui::Context, check: bool) {
        if self.repack.trim().is_empty() || self.out.trim().is_empty() {
            self.outcome = Some(Outcome::Failed(
                "Choose the repack folder and where to install first.".into(),
            ));
            return;
        }
        let args = cli_args(
            self.repack.trim(),
            self.out.trim(),
            self.decoders.trim(),
            self.mode,
            check,
        );
        self.log.clear();
        self.progress = None;
        self.outcome = None;
        self.unpacked = false;
        let exe = match std::env::current_exe() {
            Ok(e) => e.with_file_name("unrepack.exe"),
            Err(e) => {
                self.outcome = Some(Outcome::Failed(e.to_string()));
                return;
            }
        };
        let spawned = Command::new(&exe)
            .args(&args)
            .env("UNREPACK_GUI", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A console without a window: decoders the CLI starts share it,
            // so none of them flashes a window either.
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                self.outcome = Some(Outcome::Failed(format!(
                    "Can't start {}: {e}. Keep unrepack-gui.exe next to unrepack.exe.",
                    exe.display()
                )));
                return;
            }
        };
        let (tx, rx) = mpsc::channel();
        let pipes: [Option<Box<dyn Read + Send>>; 2] = [
            child.stdout.take().map(|s| Box::new(s) as _),
            child.stderr.take().map(|s| Box::new(s) as _),
        ];
        for mut pipe in pipes.into_iter().flatten() {
            let (tx, ctx) = (tx.clone(), ctx.clone());
            // Bytes, not lines: relayed decoder output may not be UTF-8, and
            // a reader that stops would leave the pipe full and hang the CLI.
            std::thread::spawn(move || {
                let (mut buf, mut line) = ([0u8; 8192], Vec::new());
                while let Ok(n) = pipe.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    for &b in &buf[..n] {
                        if b == b'\n' || b == b'\r' {
                            if !line.is_empty() {
                                let _ = tx.send(String::from_utf8_lossy(&line).into_owned());
                                line.clear();
                            }
                        } else {
                            line.push(b);
                        }
                    }
                    ctx.request_repaint();
                }
                if !line.is_empty() {
                    let _ = tx.send(String::from_utf8_lossy(&line).into_owned());
                }
                ctx.request_repaint();
            });
        }
        self.run = Some(Run {
            child,
            rx,
            check_only: check,
            mode: self.mode,
            extracting: false,
            stopped: false,
            exited: None,
            pipes_closed: false,
        });
    }

    /// Takes in the CLI's output and notices when it has finished.
    fn poll(&mut self) {
        let Some(run) = self.run.as_mut() else { return };
        loop {
            match run.rx.try_recv() {
                Ok(line) => {
                    if let Some(p) = parse_progress(&line) {
                        run.extracting |= p.0 == "extracting";
                        self.progress = Some(p);
                        continue;
                    }
                    run.extracting |= line == "extracting" || line.starts_with("extracting ");
                    self.log.push(line);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    run.pipes_closed = true;
                    break;
                }
            }
        }
        if self.log.len() > MAX_LOG {
            self.log.drain(..self.log.len() - MAX_LOG);
        }
        if run.exited.is_none()
            && let Ok(Some(status)) = run.child.try_wait()
        {
            run.exited = Some(status.success());
        }
        let (Some(ok), true) = (run.exited, run.pipes_closed) else {
            return;
        };
        let last = |prefix: &str| {
            self.log
                .iter()
                .rev()
                .find(|l| l.starts_with(prefix))
                .cloned()
        };
        self.outcome = Some(if run.stopped {
            let mut msg = "Stopped.".to_string();
            if !run.check_only {
                msg += " The install folder holds part of the game; empty it before trying again.";
                if run.mode == Mode::FreeAsRead && run.extracting {
                    msg += " The repack was partly freed and can't be used again.";
                }
            }
            Outcome::Stopped(msg)
        } else if ok && run.check_only {
            Outcome::Ok(last("it ").map_or("Checked.".into(), |l| match l.as_str() {
                "it fits" => "It fits. Nothing was changed.".into(),
                "it does not fit" => "It does not fit on this drive. Nothing was changed.".into(),
                _ => l,
            }))
        } else if ok {
            self.unpacked = true;
            // The last progress line comes up to a second before the end.
            if let Some(p) = self.progress.as_mut() {
                p.1 = p.2;
            }
            Outcome::Ok(last("done: ").map_or("Done.".into(), |l| format!("Done: {}", &l[6..])))
        } else {
            let mut err = last("error: ")
                .map_or("unrepack failed; see the log below.".into(), |l| {
                    l[7..].to_string()
                });
            if !self.admin && self.log.iter().any(|l| l.contains("CLS error")) {
                err += "\nThe decoders failed without administrator rights: restart as administrator and try again.";
            }
            Outcome::Failed(err)
        });
        if !ok || run.stopped {
            self.progress = None;
        }
        self.run = None;
    }

    fn stop(&mut self) {
        if let Some(run) = self.run.as_mut() {
            run.stopped = true;
            // The CLI's decoders are in a kill-on-close job, so they go too.
            let _ = run.child.kill();
        }
    }

    /// Stopping now loses the repack.
    fn stop_destroys(&self) -> bool {
        self.run
            .as_ref()
            .is_some_and(|r| !r.check_only && r.mode == Mode::FreeAsRead && r.extracting)
    }

    fn folder_row(
        ui: &mut egui::Ui,
        label: &str,
        value: &mut String,
    ) -> Option<std::path::PathBuf> {
        ui.horizontal(|ui| {
            ui.add_sized([110.0, 20.0], egui::Label::new(label));
            ui.add(
                egui::TextEdit::singleline(value)
                    .desired_width((ui.available_width() - 80.0).max(120.0)),
            );
            ui.button("Browse…")
                .clicked()
                .then(|| rfd::FileDialog::new().pick_folder())
                .flatten()
        })
        .inner
    }

    fn body(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let running = self.run.is_some();
        ui.heading("unrepack");
        ui.label("Unpack a FreeArc game repack (DODI, FitGirl and similar) without its installer.");
        ui.add_space(6.0);

        if !self.admin {
            ui.horizontal(|ui| {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    "Not running as administrator. The repacks' decoders need it to unpack; Check space works without.",
                );
                if ui.add_enabled(!running, egui::Button::new("Restart as administrator")).clicked()
                    && restart_as_admin()
                {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.add_space(6.0);
        }

        ui.add_enabled_ui(!running, |ui| {
            if let Some(d) = Self::folder_row(ui, "Repack folder", &mut self.repack) {
                self.picked_repack(&d);
            }
            if let Some(d) = Self::folder_row(ui, "Install to", &mut self.out) {
                self.out = d.display().to_string();
            }
            if let Some(d) = Self::folder_row(ui, "Decoders folder", &mut self.decoders) {
                self.decoders = d.display().to_string();
            }
            ui.small(
                "Decoders are the repack's own cls-*.dll, cls.ini and arc.ini files. Most repacks keep them \
                 inside Setup.exe; unrepack can't take them out of it yet, so they must be in a folder.",
            );
            ui.add_space(8.0);

            ui.label(RichText::new("While unpacking").strong());
            ui.radio_value(&mut self.mode, Mode::Keep, "Keep the repack (safe; needs space for the repack and the game)");
            ui.radio_value(&mut self.mode, Mode::DeleteEach, "Delete each data file once it is fully unpacked (safe)");
            ui.radio_value(
                &mut self.mode,
                Mode::FreeAsRead,
                RichText::new("Free the repack while reading it (least space; NOT safe: a crash loses the repack)")
                    .color(ui.visuals().warn_fg_color),
            );
        });
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            if ui
                .add_enabled(!running, egui::Button::new("Check space"))
                .clicked()
            {
                self.start(&ctx, true);
            }
            if ui
                .add_enabled(
                    !running,
                    egui::Button::new(RichText::new("Unpack").strong()),
                )
                .clicked()
            {
                if self.mode == Mode::Keep {
                    self.start(&ctx, false);
                } else {
                    self.ask = Some(Ask::Extract);
                }
            }
            if ui.add_enabled(running, egui::Button::new("Stop")).clicked() {
                self.ask = Some(Ask::Stop);
            }
            if self.unpacked
                && Path::new(&self.out).is_dir()
                && ui.button("Open install folder").clicked()
            {
                let _ = Command::new("explorer").arg(&self.out).spawn();
            }
        });
        ui.small("unrepack runs the decoders that came with the download. Use repacks you trust.");
        ui.add_space(6.0);

        if let Some((phase, done, total)) = &self.progress {
            let frac = *done as f32 / (*total).max(1) as f32;
            // The mode can't change during a run, and still shows the last run's after it.
            let step = match (phase.as_str(), self.mode) {
                ("verifying", _) => "Step 1 of 2: checking every file before anything is changed",
                ("extracting", Mode::DeleteEach | Mode::FreeAsRead) => "Step 2 of 2: unpacking",
                _ => "Unpacking",
            };
            ui.label(step);
            ui.add(egui::ProgressBar::new(frac).text(format!(
                "{:.0}%   {} of {}",
                frac * 100.0,
                human(*done),
                human(*total)
            )));
        } else if running {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Reading the repack…");
            });
        }

        match &self.outcome {
            Some(Outcome::Ok(m)) => {
                ui.label(
                    RichText::new(m)
                        .color(Color32::from_rgb(60, 170, 90))
                        .strong(),
                );
            }
            Some(Outcome::Failed(m)) => {
                ui.label(RichText::new(m).color(ui.visuals().error_fg_color).strong());
            }
            Some(Outcome::Stopped(m)) => {
                ui.label(RichText::new(m).color(ui.visuals().warn_fg_color).strong());
            }
            None => {}
        }
        ui.add_space(6.0);

        ui.label(RichText::new("Log").strong());
        egui::Frame::group(ui.style()).show(ui, |ui| {
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for l in &self.log {
                        ui.label(RichText::new(l).monospace().size(12.0));
                    }
                });
        });
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(ask) = self.ask else { return };
        if ask != Ask::Extract && self.run.is_none() {
            // The run ended while the question was open.
            self.ask = None;
            return;
        }
        let destroys = self.stop_destroys();
        let mut answer = None;
        egui::Modal::new(egui::Id::unique("ask")).show(ctx, |ui| {
            ui.set_max_width(440.0);
            let (title, text, yes) = match ask {
                Ask::Extract if self.mode == Mode::FreeAsRead => (
                    "Free the repack while reading it?",
                    "This frees the repack's bytes as they are read. If the run stops for any reason after \
                     it starts (a crash, a power cut, a decoder failure), the repack is lost and must be \
                     downloaded again. Every file is checked first, before anything is freed.\n\n\
                     unrepack runs the decoders that came with the download.",
                    "Unpack and free the repack",
                ),
                Ask::Extract => (
                    "Delete each data file when done?",
                    "Each data file is deleted once all of it is unpacked and checked. Every file is checked \
                     first, before anything is deleted.\n\nunrepack runs the decoders that came with the download.",
                    "Unpack and delete",
                ),
                Ask::Stop | Ask::Close if destroys => (
                    "Stop now and lose the repack?",
                    "Part of the repack has already been freed. Stopping now leaves a half-unpacked game \
                     and a repack that can't be used again.",
                    "Stop and lose the repack",
                ),
                Ask::Stop | Ask::Close => (
                    "Stop unpacking?",
                    "The repack is unchanged, but the install folder keeps the files unpacked so far; empty it \
                     before trying again.",
                    "Stop",
                ),
            };
            ui.heading(title);
            ui.add_space(4.0);
            ui.label(text);
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let danger = destroys || (ask == Ask::Extract && self.mode == Mode::FreeAsRead);
                let mut b = egui::Button::new(yes);
                if danger {
                    b = b.fill(ui.visuals().error_fg_color.gamma_multiply(0.5));
                }
                if ui.add(b).clicked() {
                    answer = Some(true);
                }
                if ui.button(if ask == Ask::Extract { "Cancel" } else { "Keep going" }).clicked() {
                    answer = Some(false);
                }
            });
        });
        match answer {
            Some(true) => {
                self.ask = None;
                match ask {
                    Ask::Extract => self.start(ctx, false),
                    Ask::Stop => self.stop(),
                    Ask::Close => {
                        self.stop();
                        self.closing = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
            }
            Some(false) => self.ask = None,
            None => {}
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll();
        if self.run.is_some() {
            ctx.request_repaint_after(Duration::from_millis(250));
            // Closing the window would leave the CLI running unseen, or kill
            // it mid-run: ask instead.
            if ctx.input(|i| i.viewport().close_requested()) && !self.closing {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.ask = Some(Ask::Close);
            }
        }
        egui::CentralPanel::default().show(ui, |ui| self.body(ui));
        self.dialogs(&ctx);
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("unrepack")
            .with_inner_size([780.0, 640.0])
            .with_min_inner_size([560.0, 460.0]),
        ..Default::default()
    };
    eframe::run_native(
        "unrepack",
        options,
        Box::new(|_cc| Ok(Box::new(App::new()))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_and_progress_lines() {
        assert_eq!(
            cli_args("R", "O", "", Mode::Keep, false),
            ["extract", "R", "-o", "O", "--no-verify", "--yes"]
        );
        assert_eq!(
            cli_args("R", "O", "D", Mode::FreeAsRead, true),
            [
                "extract",
                "R",
                "-o",
                "O",
                "--decoders",
                "D",
                "--free-as-read",
                "--check"
            ]
        );
        assert_eq!(
            cli_args("R", "O", "", Mode::DeleteEach, false)[4],
            "--delete-each"
        );
        assert_eq!(
            parse_progress("@progress verifying 5 10"),
            Some(("verifying".into(), 5, 10))
        );
        assert_eq!(parse_progress("rzw: 42% done"), None);
        assert_eq!(parse_progress("@progress extracting x 10"), None);
    }
}
