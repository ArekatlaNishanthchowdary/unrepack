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

/// One data file, as `unrepack parts` describes it.
#[derive(Clone, PartialEq, Debug)]
struct Part {
    kind: String,
    on: bool,
    unpacked: u64,
    name: String,
}

/// `unrepack parts` output: kind, on/off, packed, unpacked, name per line.
fn parse_parts(text: &str) -> Vec<Part> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.trim_end_matches('\r').splitn(5, '\t').collect();
            let [kind, on, _, unpacked, name] = f.as_slice() else {
                return None;
            };
            Some(Part {
                kind: kind.to_string(),
                on: *on == "on",
                unpacked: unpacked.parse().ok()?,
                name: name.to_string(),
            })
        })
        .collect()
}

/// What to extract: the folder when every part is ticked (or none are
/// known), else the ticked data files by name.
fn inputs(repack: &str, parts: &[Part]) -> Vec<String> {
    if parts.iter().all(|p| p.on) {
        return vec![repack.into()];
    }
    parts
        .iter()
        .filter(|p| p.on)
        .map(|p| Path::new(repack).join(&p.name).display().to_string())
        .collect()
}

/// The CLI's arguments for one run. `--yes` because the window asks instead.
fn cli_args(inputs: &[String], out: &str, decoders: &str, mode: Mode, check: bool) -> Vec<String> {
    let mut a: Vec<String> = vec!["extract".into()];
    a.extend(inputs.iter().cloned());
    a.extend(["-o".into(), out.into()]);
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

const OK_GREEN: Color32 = Color32::from_rgb(60, 170, 90);

/// Roomier spacing, rounder corners and slightly larger text than egui's defaults.
fn theme(ctx: &egui::Context) {
    use egui::{FontId, TextStyle};
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 8.0);
        s.spacing.button_padding = egui::vec2(12.0, 5.0);
        s.spacing.interact_size.y = 26.0;
        s.spacing.indent = 26.0;
        for (t, size) in [
            (TextStyle::Heading, 22.0),
            (TextStyle::Body, 14.0),
            (TextStyle::Button, 14.0),
            (TextStyle::Small, 11.5),
        ] {
            let family = s.text_styles[&t].family.clone();
            s.text_styles.insert(t, FontId::new(size, family));
        }
        let w = &mut s.visuals.widgets;
        for v in [
            &mut w.noninteractive,
            &mut w.inactive,
            &mut w.hovered,
            &mut w.active,
            &mut w.open,
        ] {
            v.corner_radius = 6.into();
        }
    });
}

/// A titled section on a slightly raised background.
fn card(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style())
        .fill(ui.visuals().faint_bg_color)
        .inner_margin(12)
        .corner_radius(8)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(title).strong().size(15.0));
            ui.add_space(2.0);
            add(ui);
        });
}

/// A tinted box for a warning or a result.
fn banner(ui: &mut egui::Ui, color: Color32, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.12))
        .stroke((1.0, color.gamma_multiply(0.6)))
        .inner_margin(10)
        .corner_radius(8)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
}

fn is_admin() -> bool {
    unsafe { windows_sys::Win32::UI::Shell::IsUserAnAdmin() != 0 }
}

fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().chain([0]).collect()
}

/// The PowerShell that has Defender skip unrepack's programs (`dir` holds
/// them) and the repack folder.
fn defender_command(dir: &Path, repack: &Path) -> String {
    let q = |p: &Path| format!("'{}'", p.display().to_string().replace('\'', "''"));
    let exes = ["unrepack-gui.exe", "unrepack.exe", "unrepack-clshost.exe"]
        .map(|e| q(&dir.join(e)))
        .join(",");
    format!(
        "$ErrorActionPreference='Stop'; Add-MpPreference -ExclusionProcess {exes} -ExclusionPath {}",
        q(repack)
    )
}

/// Adds the Defender exclusions through an administrator prompt. Ok(false):
/// the user said no to the prompt. ponytail: waits on the UI thread; the
/// prompt is modal anyway and PowerShell takes a second or two after it.
fn defender_exclude(repack: &Path) -> Result<bool, String> {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_CANCELLED};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, INFINITE, WaitForSingleObject,
    };
    use windows_sys::Win32::UI::Shell::{
        SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
    };
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let cmd = defender_command(exe.parent().unwrap_or(Path::new(".")), repack);
    let params = format!("-NoProfile -NonInteractive -WindowStyle Hidden -Command \"{cmd}\"");
    let (verb, file, params) = (
        wide("runas".as_ref()),
        wide("powershell.exe".as_ref()),
        wide(params.as_ref()),
    );
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = 0; // SW_HIDE
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        let e = std::io::Error::last_os_error();
        return match e.raw_os_error() {
            Some(c) if c as u32 == ERROR_CANCELLED => Ok(false),
            _ => Err(e.to_string()),
        };
    }
    let mut code = 1u32;
    unsafe {
        WaitForSingleObject(info.hProcess, INFINITE);
        GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
    }
    if code == 0 {
        Ok(true)
    } else {
        Err(format!(
            "Windows Defender didn't take the exclusion (PowerShell exited with {code}). \
             Another antivirus may be in charge, or your organisation manages Defender."
        ))
    }
}

fn restart_as_admin() -> bool {
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
    Defender,
}

struct App {
    repack: String,
    /// The repack's data files, read for the folder in `parts_for`.
    parts: Vec<Part>,
    parts_for: String,
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
            parts: Vec::new(),
            parts_for: String::new(),
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

    /// Reads the repack's parts when the folder changes. ponytail: runs the
    /// CLI on the UI thread; it reads only the indexes, which takes moments.
    fn refresh_parts(&mut self) {
        let dir = self.repack.trim().to_string();
        if dir == self.parts_for {
            return;
        }
        self.parts_for = dir.clone();
        self.parts.clear();
        if !Path::new(&dir).is_dir() {
            return;
        }
        let Ok(exe) = std::env::current_exe().map(|e| e.with_file_name("unrepack.exe")) else {
            return;
        };
        if let Ok(o) = Command::new(exe)
            .args(["parts", &dir])
            .stdin(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            self.parts = parse_parts(&String::from_utf8_lossy(&o.stdout));
        }
    }

    fn parts_card(&mut self, ui: &mut egui::Ui) {
        if self.parts.iter().all(|p| p.kind == "required") {
            return;
        }
        card(ui, "What to install", |ui| {
            let base: u64 = self
                .parts
                .iter()
                .filter(|p| p.kind == "required")
                .map(|p| p.unpacked)
                .sum();
            ui.label(
                RichText::new(format!(
                    "The game itself always installs ({}).",
                    human(base)
                ))
                .weak(),
            );
            for (kind, title) in [("language", "Languages"), ("optional", "Extras")] {
                if !self.parts.iter().any(|p| p.kind == kind) {
                    continue;
                }
                ui.label(RichText::new(title).strong());
                ui.horizontal_wrapped(|ui| {
                    for p in self.parts.iter_mut().filter(|p| p.kind == kind) {
                        let stem = p.name.rsplit_once('.').map_or(p.name.as_str(), |(s, _)| s);
                        let label = format!("{} ({})", stem.replace('_', " "), human(p.unpacked));
                        ui.checkbox(&mut p.on, label);
                    }
                });
            }
        });
    }

    fn start(&mut self, ctx: &egui::Context, check: bool) {
        if self.repack.trim().is_empty() || self.out.trim().is_empty() {
            self.outcome = Some(Outcome::Failed(
                "Choose the repack folder and where to install first.".into(),
            ));
            return;
        }
        self.refresh_parts();
        let args = cli_args(
            &inputs(self.repack.trim(), &self.parts),
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
            ui.add_sized([132.0, 24.0], egui::Label::new(label));
            ui.add(
                egui::TextEdit::singleline(value)
                    // Room for the Browse button and the gap before it.
                    .desired_width((ui.available_width() - 96.0).max(120.0)),
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
        ui.heading(RichText::new("unrepack").strong());
        ui.label(
            RichText::new(
                "Unpack a FreeArc game repack (DODI, FitGirl and similar) without its installer.",
            )
            .weak(),
        );
        ui.add_space(4.0);

        if !self.admin {
            let warn = ui.visuals().warn_fg_color;
            banner(ui, warn, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new("⚠ Not running as administrator.")
                            .color(warn)
                            .strong(),
                    );
                    ui.label("The repacks' decoders need it to unpack; Check space works without.");
                    if ui
                        .add_enabled(!running, egui::Button::new("Restart as administrator"))
                        .clicked()
                        && restart_as_admin()
                    {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            });
        }

        ui.add_enabled_ui(!running, |ui| {
            card(ui, "Folders", |ui| {
                if let Some(d) = Self::folder_row(ui, "Repack folder", &mut self.repack) {
                    self.picked_repack(&d);
                }
                if let Some(d) = Self::folder_row(ui, "Install to", &mut self.out) {
                    self.out = d.display().to_string();
                }
                if let Some(d) = Self::folder_row(ui, "Decoders (optional)", &mut self.decoders) {
                    self.decoders = d.display().to_string();
                }
                ui.label(
                    RichText::new(
                        "Leave Decoders empty: unrepack reads the repack's decoders (cls-*.dll, cls.ini, arc.ini) out of \
                         its Setup.exe without running it. Fill it in only to use a folder of decoders instead.",
                    )
                    .small()
                    .weak(),
                );
            });

            self.refresh_parts();
            self.parts_card(ui);

            card(ui, "While unpacking", |ui| {
                let warn = ui.visuals().warn_fg_color;
                for (mode, title, note, risky) in [
                    (Mode::Keep, "Keep the repack", "Safe. Needs room for both the repack and the game.", false),
                    (
                        Mode::DeleteEach,
                        "Delete each data file once it is fully unpacked",
                        "Safe. Frees space as it goes; every file is checked first.",
                        false,
                    ),
                    (
                        Mode::FreeAsRead,
                        "Free the repack while reading it",
                        "Least space. NOT safe: a crash or power cut loses the repack.",
                        true,
                    ),
                ] {
                    ui.radio_value(&mut self.mode, mode, title);
                    ui.add_space(-6.0);
                    ui.indent(mode as u8, |ui| {
                        let note = RichText::new(note).small();
                        ui.label(if risky { note.color(warn) } else { note.weak() });
                    });
                }
            });
        });

        ui.horizontal(|ui| {
            let unpack = egui::Button::new(RichText::new("Unpack").strong().color(Color32::WHITE))
                .fill(ui.visuals().selection.bg_fill)
                .min_size(egui::vec2(120.0, 32.0));
            if ui.add_enabled(!running, unpack).clicked() {
                if self.mode == Mode::Keep {
                    self.start(&ctx, false);
                } else {
                    self.ask = Some(Ask::Extract);
                }
            }
            let side = egui::vec2(0.0, 32.0);
            if ui
                .add_enabled(!running, egui::Button::new("Check space").min_size(side))
                .clicked()
            {
                self.start(&ctx, true);
            }
            if ui
                .add_enabled(running, egui::Button::new("Stop").min_size(side))
                .clicked()
            {
                self.ask = Some(Ask::Stop);
            }
            if self.unpacked
                && Path::new(&self.out).is_dir()
                && ui.button("📂 Open install folder").clicked()
            {
                let _ = Command::new("explorer").arg(&self.out).spawn();
            }
        });
        if ui
            .add_enabled(
                !running && Path::new(self.repack.trim()).is_dir(),
                egui::Button::new("🛡 Let Windows Defender skip this repack…").small(),
            )
            .on_hover_text("Defender often removes repack decoders and Setup.exe mid-unpack.")
            .clicked()
        {
            self.ask = Some(Ask::Defender);
        }
        ui.label(
            RichText::new(
                "unrepack runs the decoders that came with the download. Use repacks you trust.",
            )
            .small()
            .weak(),
        );

        if let Some((phase, done, total)) = &self.progress {
            let frac = *done as f32 / (*total).max(1) as f32;
            // The mode can't change during a run, and still shows the last run's after it.
            let step = match (phase.as_str(), self.mode) {
                ("verifying", _) => "Step 1 of 2: checking every file before anything is changed",
                ("extracting", Mode::DeleteEach | Mode::FreeAsRead) => "Step 2 of 2: unpacking",
                _ => "Unpacking",
            };
            ui.label(RichText::new(step).strong());
            ui.add(
                egui::ProgressBar::new(frac)
                    .desired_height(22.0)
                    .animate(running)
                    .text(format!(
                        "{:.0}%   {} of {}",
                        frac * 100.0,
                        human(*done),
                        human(*total)
                    )),
            );
        } else if running {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Reading the repack…");
            });
        }

        let shown = match &self.outcome {
            Some(Outcome::Ok(m)) => Some((OK_GREEN, m)),
            Some(Outcome::Failed(m)) => Some((ui.visuals().error_fg_color, m)),
            Some(Outcome::Stopped(m)) => Some((ui.visuals().warn_fg_color, m)),
            None => None,
        };
        if let Some((color, m)) = shown {
            banner(ui, color, |ui| {
                ui.label(RichText::new(m).color(color).strong());
            });
        }

        ui.add_space(2.0);
        ui.label(RichText::new("Log").strong());
        egui::Frame::group(ui.style())
            .fill(ui.visuals().extreme_bg_color)
            .inner_margin(8)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if self.log.is_empty() {
                            ui.label(RichText::new("The CLI's output appears here.").weak());
                        }
                        for l in &self.log {
                            ui.label(RichText::new(l).monospace().size(12.0));
                        }
                    });
            });
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(ask) = self.ask else { return };
        if matches!(ask, Ask::Stop | Ask::Close) && self.run.is_none() {
            // The run ended while the question was open.
            self.ask = None;
            return;
        }
        let destroys = self.stop_destroys();
        let mut answer = None;
        let defender = format!(
            "Windows Defender often flags a repack's decoders and Setup.exe and removes them while \
             unpacking. This adds Defender exclusions for unrepack's own programs and for this folder:\n\n\
             {}\n\nDefender then won't scan anything in that folder, so do this only for a repack you \
             trust. Windows asks for administrator permission next. To undo it: Windows Security > \
             Virus & threat protection > Manage settings > Exclusions.",
            self.repack.trim()
        );
        egui::Modal::new(egui::Id::unique("ask")).show(ctx, |ui| {
            ui.set_max_width(440.0);
            let (title, text, yes) = match ask {
                Ask::Defender => (
                    "Let Windows Defender skip this repack?",
                    defender.as_str(),
                    "Add exclusion",
                ),
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
                let no = if matches!(ask, Ask::Stop | Ask::Close) { "Keep going" } else { "Cancel" };
                if ui.button(no).clicked() {
                    answer = Some(false);
                }
            });
        });
        match answer {
            Some(true) => {
                self.ask = None;
                match ask {
                    Ask::Extract => self.start(ctx, false),
                    Ask::Defender => {
                        self.outcome = Some(match defender_exclude(Path::new(self.repack.trim())) {
                            Ok(true) => Outcome::Ok(
                                "Windows Defender now skips unrepack and this repack folder."
                                    .into(),
                            ),
                            Ok(false) => Outcome::Stopped(
                                "No exclusion added: the administrator prompt was declined.".into(),
                            ),
                            Err(e) => Outcome::Failed(e),
                        })
                    }
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
        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(ui.style()).inner_margin(16))
            .show(ui, |ui| self.body(ui));
        self.dialogs(&ctx);
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("unrepack")
            .with_inner_size([820.0, 760.0])
            .with_min_inner_size([560.0, 460.0]),
        ..Default::default()
    };
    eframe::run_native(
        "unrepack",
        options,
        Box::new(|cc| {
            theme(&cc.egui_ctx);
            Ok(Box::new(App::new()))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_pick_the_inputs() {
        let mut parts = parse_parts(
            "required\ton\t9\t90\tdata1.doi\r\nlanguage\ton\t1\t10\tEnglish.doi\n\
             language\toff\t1\t11\tFrench.doi\noptional\ton\t2\t20\tArtwork_Mini.doi\nbad line\n",
        );
        assert_eq!(parts.len(), 4);
        assert_eq!(
            parts[2],
            Part {
                kind: "language".into(),
                on: false,
                unpacked: 11,
                name: "French.doi".into()
            }
        );
        let r = Path::new("R");
        assert_eq!(
            inputs("R", &parts),
            ["data1.doi", "English.doi", "Artwork_Mini.doi"]
                .map(|n| r.join(n).display().to_string())
        );
        parts[2].on = true;
        assert_eq!(inputs("R", &parts), ["R"]);
        assert_eq!(inputs("R", &[]), ["R"]);
    }

    #[test]
    fn args_and_progress_lines() {
        let r = ["R".to_string()];
        assert_eq!(
            cli_args(&r, "O", "", Mode::Keep, false),
            ["extract", "R", "-o", "O", "--no-verify", "--yes"]
        );
        assert_eq!(
            cli_args(&["A".into(), "B".into()], "O", "", Mode::Keep, true),
            ["extract", "A", "B", "-o", "O", "--no-verify", "--check"]
        );
        assert_eq!(
            cli_args(&r, "O", "D", Mode::FreeAsRead, true),
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
            cli_args(&r, "O", "", Mode::DeleteEach, false)[4],
            "--delete-each"
        );
        assert_eq!(
            parse_progress("@progress verifying 5 10"),
            Some(("verifying".into(), 5, 10))
        );
        assert_eq!(parse_progress("rzw: 42% done"), None);
        assert_eq!(parse_progress("@progress extracting x 10"), None);
    }

    #[test]
    fn defender_command_quotes_paths() {
        let c = defender_command(
            Path::new(r"C:\u"),
            Path::new(r"D:\Bob's Game [DODI Repack]"),
        );
        assert!(c.starts_with("$ErrorActionPreference='Stop'; Add-MpPreference -ExclusionProcess 'C:\\u\\unrepack-gui.exe','C:\\u\\unrepack.exe','C:\\u\\unrepack-clshost.exe' "), "{c}");
        assert!(
            c.ends_with(r" -ExclusionPath 'D:\Bob''s Game [DODI Repack]'"),
            "{c}"
        );
    }
}
