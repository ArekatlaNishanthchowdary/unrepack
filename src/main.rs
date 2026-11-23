//! unrepack: extract game repacks built on FreeArc (DODI, FitGirl and the
//! like) without the installer, optionally giving the repack's space back
//! while it is read.

#[macro_use]
mod log;
mod builtin;
mod extract;
mod freearc;
mod ifps;
mod inno;
mod recipe;
mod tornado;
mod win;
mod zip;

use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail, ensure};

use extract::{Decoders, Job, Progress, human};

const USAGE: &str = concat!(
    "unrepack ",
    env!("CARGO_PKG_VERSION"),
    ": extract FreeArc repack data files without running the installer,
and plain ZIP archives the same way

Usage:
  unrepack list <data file | zip>
  unrepack extract <repack folder | data files... | zip files...> -o <output folder> [options]
  unrepack parts <repack folder>              which data files always install, and which
                                              are languages or extras to choose from
  unrepack decoders <Setup.exe> -o <folder>   copy the decoders out of the installer

A folder extracts every data file in it, or every ZIP if it has no data files
(stored and deflate entries; no passwords). To choose languages and extras as the
installer would, name the data files you want. When two of them hold the same
file, the one named later wins, as with the installer.

Options:
  --decoders <dir>  folder with the repack's cls-*.dll, cls-*_x64.exe and cls.ini. Default:
                    those files next to the data files, else read out of the Setup.exe there
  --host <exe>      the 32-bit CLS host (default: unrepack-clshost.exe next to unrepack)
  --work <dir>      where decoders run and keep temp files (default: <output>\\.unrepack-work)
  --log <file>      also write every message and a progress line every 5 s to <file>
  --check           plan only: say whether it fits, change nothing
  --delete-each     delete each data file once all of it is extracted (crash safe)
  --free-as-read    free each data file's bytes as they are decoded. NOT crash safe in
                    this version: if the run stops after the first freed byte, the repack is lost
  --verify          decode everything once, checking every CRC, before writing anything.
                    On by default with --free-as-read and --delete-each, where it stops a
                    bad repack before any of it is gone; it doubles the time
  --no-verify       skip that pass even when freeing or deleting (removes the safety net)
  --yes             don't ask before freeing or deleting

The repack's decoders are programs that came with the download; unrepack runs them.
"
);

struct Args {
    cmd: String,
    inputs: Vec<PathBuf>,
    out: Option<PathBuf>,
    decoders: Option<PathBuf>,
    host: Option<PathBuf>,
    work: Option<PathBuf>,
    log: Option<PathBuf>,
    check: bool,
    delete_each: bool,
    free_as_read: bool,
    verify: bool,
    yes: bool,
}

fn parse() -> Result<Args> {
    let mut it = std::env::args_os().skip(1);
    let cmd = it
        .next()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut a = Args {
        cmd,
        inputs: vec![],
        out: None,
        decoders: None,
        host: None,
        work: None,
        log: None,
        check: false,
        delete_each: false,
        free_as_read: false,
        verify: false,
        yes: false,
    };
    let mut verify = None;
    while let Some(arg) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .map(PathBuf::from)
                .with_context(|| format!("{name} needs a value"))
        };
        match arg.to_string_lossy().as_ref() {
            "-o" | "--output" => a.out = Some(value("-o")?),
            "--decoders" => a.decoders = Some(value("--decoders")?),
            "--host" => a.host = Some(value("--host")?),
            "--work" => a.work = Some(value("--work")?),
            "--log" => a.log = Some(value("--log")?),
            "--check" => a.check = true,
            "--delete-each" => a.delete_each = true,
            "--free-as-read" => a.free_as_read = true,
            "--verify" => verify = Some(true),
            "--no-verify" => verify = Some(false),
            "--yes" | "-y" => a.yes = true,
            s if s.starts_with('-') => bail!("unknown option {s}\n\n{USAGE}"),
            _ => a.inputs.push(PathBuf::from(arg)),
        }
    }
    // Keeping the repack, a bad block only means rerunning into an empty
    // folder, and the MD5 check already caught damaged downloads; decoding
    // everything twice isn't worth it there.
    a.verify = verify.unwrap_or(a.delete_each || a.free_as_read);
    Ok(a)
}

/// The repack's decoders sit next to its data files only if someone put them
/// there; most repacks keep them inside Setup.exe.
fn has_decoders(dir: &Path) -> bool {
    fs::read_dir(dir).is_ok_and(|rd| {
        rd.filter_map(|e| e.ok()).any(|e| {
            let n = e.file_name().to_string_lossy().to_lowercase();
            n == "arc.ini" || n == "cls.ini" || (n.starts_with("cls-") && n.ends_with(".dll"))
        })
    })
}

fn setup_in(dir: &Path) -> Option<PathBuf> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| {
            p.file_name()
                .is_some_and(|n| n.eq_ignore_ascii_case("setup.exe"))
        })
}

/// The steps Setup.exe's script takes, when following them matters (see
/// [`recipe::needed`]). A script unrepack can't read is passed over: the
/// data files still extract by name, as before. A Setup.exe that can't be
/// opened at all (an antivirus blocking it) is refused: its steps may be
/// needed, and name order would quietly leave them out.
fn recipe_for(dir: &Path) -> Result<Option<Vec<recipe::Step>>> {
    let Some(setup) = setup_in(dir) else {
        return Ok(None);
    };
    let code = match inno::script(&setup) {
        Ok(Some(c)) => c,
        Err(e) if e.downcast_ref::<std::io::Error>().is_some() => {
            return Err(e.context(
                "Setup.exe holds the repack's install steps; if an antivirus blocked it, \
                 restore it (or exclude this folder) and run again",
            ));
        }
        _ => return Ok(None),
    };
    let calls = match ifps::Script::parse(&code).and_then(|s| s.calls("CurStepChanged")) {
        Ok(c) => c,
        Err(e) => {
            note!("can't follow Setup.exe's script ({e:#}); extracting by file name");
            return Ok(None);
        }
    };
    if !calls
        .iter()
        .any(|c| c.name.eq_ignore_ascii_case("ISArcExtract"))
    {
        return Ok(None);
    }
    let steps = recipe::steps(&calls).context("following Setup.exe's install steps")?;
    Ok(recipe::needed(&steps).then_some(steps))
}

/// Sets `key` in `[section]` of an ini file, adding either if missing.
/// `{app}` and `{tmp}` become `.`, as in the ini files read out of Setup.
fn set_ini(path: &Path, section: &str, key: &str, value: &str) -> Result<()> {
    let text = fs::read_to_string(path).unwrap_or_default();
    let value = value.replace("{app}", ".").replace("{tmp}", ".");
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let head = |l: &str| {
        l.trim()
            .strip_prefix('[')
            .and_then(|r| r.strip_suffix(']'))
            .map(str::trim)
            .map(str::to_owned)
    };
    let start = lines
        .iter()
        .position(|l| head(l).is_some_and(|h| h.eq_ignore_ascii_case(section)));
    let line = format!("{key}={value}");
    match start {
        None => {
            lines.push(format!("[{section}]"));
            lines.push(line);
        }
        Some(s) => {
            let end = (s + 1..lines.len())
                .find(|&i| head(&lines[i]).is_some())
                .unwrap_or(lines.len());
            let at = (s + 1..end).find(|&i| {
                lines[i]
                    .split_once('=')
                    .is_some_and(|(k, _)| k.trim().eq_ignore_ascii_case(key))
            });
            match at {
                Some(i) => lines[i] = line,
                None => lines.insert(end, line),
            }
        }
    }
    fs::write(path, lines.join("\r\n") + "\r\n")
        .with_context(|| format!("writing {}", path.display()))
}

/// Runs one of Setup's commands and waits for it. A missing program or
/// folder stops the run; a non-zero exit is only logged, since some tools
/// (fart counts its replacements) use it for other things than failure, as
/// Setup ignores it too. FitGirl's hash list is the real check afterwards.
/// ponytail: output shows when it ends, not as it goes.
fn run_setup_cmd(exe: &str, params: &str, dir: &str, places: &recipe::Places) -> Result<()> {
    use std::os::windows::process::CommandExt;
    let (exe, params, dir) = (
        places.expand(exe)?,
        places.expand(params)?,
        places.expand(dir)?,
    );
    let dir = if dir.is_empty() {
        places.tmp.clone()
    } else {
        PathBuf::from(&dir)
    };
    ensure!(
        dir.is_dir(),
        "Setup runs {exe} in {}, which doesn't exist",
        dir.display()
    );
    let lower = exe.to_ascii_lowercase();
    let script = lower.ends_with(".bat") || lower.ends_with(".cmd");
    ensure!(
        Path::new(&exe).is_file(),
        "Setup runs {exe}, which isn't there"
    );
    if script {
        // Kept in the log: the decoders' folder, which holds it, goes at the end.
        say!("{exe}:");
        for l in fs::read_to_string(&exe).unwrap_or_default().lines() {
            say!("  | {l}");
        }
    }
    let mut cmd = if script {
        let mut c = std::process::Command::new(places.expand("{cmd}")?);
        c.raw_arg(format!("/C \"\"{exe}\" {params}\""));
        c
    } else {
        let mut c = std::process::Command::new(&exe);
        c.raw_arg(&params);
        c
    };
    say!("running {exe} {params}");
    let o = cmd
        .current_dir(&dir)
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("starting {exe}"))?;
    for l in String::from_utf8_lossy(&o.stdout)
        .lines()
        .chain(String::from_utf8_lossy(&o.stderr).lines())
        .filter(|l| !l.trim().is_empty())
    {
        say!("  {l}");
    }
    if !o.status.success() {
        note!("  {exe} ended with {}", o.status);
    }
    Ok(())
}

/// Whether a component of Setup's (`local\eng`) belongs to a data file being
/// unpacked.
fn chosen_component(steps: &[recipe::Step], jobs: &[(PathBuf, freearc::Archive)], c: &str) -> bool {
    steps.iter().any(|s| match s {
        recipe::Step::Extract {
            data,
            component: Some(x),
            ..
        } => {
            x.eq_ignore_ascii_case(c)
                && jobs.iter().any(|(f, _)| {
                    f.file_name()
                        .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(data))
                })
        }
        _ => false,
    })
}

/// Does one of Setup's steps. Settings changes were made up front; a data
/// file not chosen for this run is passed over, as Setup does an unticked one.
fn setup_step(
    s: &recipe::Step,
    pl: &recipe::Places,
    tools: &[(PathBuf, String, freearc::Archive)],
    jobs: &[(PathBuf, freearc::Archive)],
    dec: &Decoders,
    p: &mut Progress,
) -> Result<()> {
    let named = |f: &Path, data: &str| {
        f.file_name()
            .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(data))
    };
    match s {
        recipe::Step::Ini { .. } => {}
        recipe::Step::Extract {
            data,
            tools: true,
            sub,
            ..
        } => {
            let (f, _, arc) = tools
                .iter()
                .find(|(f, ..)| named(f, data))
                .context("tool archive missing")?;
            note!("unpacking Setup's tools from {}", f.display());
            extract_tools(
                &Job {
                    data: f,
                    archive: arc,
                },
                sub,
                dec,
                &pl.tmp,
            )?;
        }
        recipe::Step::Extract { data, .. } => {
            if let Some((f, arc)) = jobs.iter().find(|(f, _)| named(f, data)) {
                note!("extracting {}", f.display());
                extract::run(
                    &Job {
                        data: f,
                        archive: arc,
                    },
                    dec,
                    &pl.app,
                    false,
                    p,
                )?;
            }
        }
        recipe::Step::Run {
            exe, params, dir, ..
        } => run_setup_cmd(exe, params, dir, pl)?,
        recipe::Step::DelTree(d) => {
            let d = pl.inside(d)?;
            if d.is_dir() {
                fs::remove_dir_all(&d).with_context(|| format!("deleting {}", d.display()))?;
            }
        }
        recipe::Step::DeleteFile(f) => {
            let f = pl.inside(f)?;
            if f.is_file() {
                fs::remove_file(&f).with_context(|| format!("deleting {}", f.display()))?;
            }
        }
    }
    Ok(())
}

/// A tool archive Setup unpacks into its temp folder: only what is under
/// `sub` in it, with `sub` taken off (`work\work\x.bat` -> `work\x.bat`).
fn extract_tools(job: &Job, sub: &str, dec: &Decoders, tmp: &Path) -> Result<()> {
    let mut p = Progress::new(job.archive.unpacked());
    if sub.is_empty() {
        return extract::run(job, dec, tmp, false, &mut p);
    }
    let stage = tmp.join(".unrepack-stage");
    let _ = fs::remove_dir_all(&stage);
    extract::run(job, dec, &stage, false, &mut p)?;
    let from = stage.join(sub);
    for e in fs::read_dir(&from)
        .with_context(|| format!("{} has no {sub} folder", job.data.display()))?
    {
        let e = e?;
        let to = tmp.join(e.file_name());
        ensure!(!to.exists(), "{} already exists", to.display());
        fs::rename(e.path(), &to).with_context(|| format!("moving {}", e.path().display()))?;
    }
    fs::remove_dir_all(&stage).with_context(|| format!("removing {}", stage.display()))
}

/// A folder of decoders read out of Setup.exe, removed when the run ends.
/// ponytail: a killed run leaves it in %TEMP%; sweep old ones if that piles up.
struct Carved(PathBuf, std::cell::Cell<bool>);

impl Carved {
    /// Leaves the folder in place after a failed step, to look into.
    fn keep(&self) {
        if !self.1.replace(true) {
            note!("Setup's tools and scripts are kept in {}", self.0.display());
        }
    }
}

impl Drop for Carved {
    fn drop(&mut self) {
        if !self.1.get() {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

type Found = (std::result::Result<PathBuf, String>, Option<Carved>);

/// `--decoders`, else decoders next to the data files, else the ones inside
/// the Setup.exe there. Without any, the reason goes along: methods that need
/// them are refused later, with it.
fn find_decoders(given: Option<&Path>, first: &Path) -> Result<Found> {
    const HOW: &str = "keep its Setup.exe next to the data files, or pass --decoders <folder>";
    if let Some(d) = given {
        return Ok((Ok(std::path::absolute(d)?), None));
    }
    let first = std::path::absolute(first)?;
    let Some(dir) = first.parent() else {
        return Ok((Err(HOW.into()), None));
    };
    if has_decoders(dir) {
        return Ok((Ok(dir.to_path_buf()), None));
    }
    let Some(setup) = setup_in(dir) else {
        return Ok((Err(HOW.into()), None));
    };
    let tmp = std::env::temp_dir().join(format!("unrepack-decoders-{}", std::process::id()));
    // A run that was killed leaves its folder; a later run with the same id
    // must not mix that repack's decoders with this one's.
    let _ = fs::remove_dir_all(&tmp);
    let carved = Carved(tmp.clone(), Default::default());
    match inno::carve(&setup, &tmp) {
        Ok(n) => {
            say!(
                "decoders: read {n} files out of {} (nothing in it was run)",
                setup.display()
            );
            Ok((Ok(tmp), Some(carved)))
        }
        Err(e) => Ok((
            Err(format!(
                "unrepack can't read them out of {}: {e:#}. Pass --decoders <folder>",
                setup.display()
            )),
            None,
        )),
    }
}

/// Data files named directly, or every FreeArc file in a folder: the
/// required ones first, as DODI's installer extracts them, then by name.
/// Order matters where a file is in two of them: GoW's data2.doi holds
/// English's copy of the language file each language pack replaces.
fn data_files(inputs: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for p in inputs {
        if p.is_dir() {
            let mut found: Vec<PathBuf> = fs::read_dir(p)
                .with_context(|| format!("listing {}", p.display()))?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.is_file())
                .collect();
            // A folder of ZIPs (a game shipped zipped). A ZIP next to FreeArc
            // data files is something else, such as a crack, and stays put.
            if !found.iter().any(|f| freearc::sniff(f)) {
                let mut zips: Vec<PathBuf> = found.into_iter().filter(|f| zip::sniff(f)).collect();
                ensure!(
                    !zips.is_empty(),
                    "no FreeArc data files or ZIP archives in {}",
                    p.display()
                );
                zips.sort();
                out.extend(zips);
                continue;
            }
            let name = |p: &Path| {
                p.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_lowercase()
            };
            // A language or extra left out of a download is often there in
            // part (the torrent's shared pieces): skip it, as the installer
            // does a missing one. A broken game file still stops the run.
            let broken: Vec<String> = found
                .iter()
                .filter(|f| {
                    number(&name(f)).is_some()
                        && f.metadata().is_ok_and(|m| m.len() > 0)
                        && !freearc::sniff(f)
                })
                .map(|f| name(f))
                .collect();
            ensure!(
                broken.is_empty(),
                "{} is damaged (its start is missing); download it again",
                broken.join(", ")
            );
            found.retain(|f| {
                if !freearc::sniff(f) {
                    // ponytail: a language pack is the only name we know is
                    // a data file; Setup.exe and the rest stay quiet.
                    if language(&name(f)).is_some() && f.metadata().is_ok_and(|m| m.len() > 0) {
                        note!(
                            "skipping {}: incomplete or damaged (its start is missing)",
                            name(f)
                        );
                    }
                    return false;
                }
                if kind(&name(f)) == Kind::Required {
                    return true;
                }
                let e = freearc::open(f).err();
                if let Some(e) = &e {
                    note!("skipping {}: incomplete or damaged ({e})", name(f));
                }
                e.is_none()
            });
            found.sort_by_cached_key(|p| order(&name(p)));
            ensure!(
                !found.is_empty(),
                "no FreeArc data files in {}",
                p.display()
            );
            // The required files come first, by prefix then number: each must
            // follow the one before it, or the download is missing one. CRCs
            // can't tell, as they only cover the files that are there.
            // ponytail: a missing *last* file passes; the installer script's
            // `{src}\dataN` list would catch it, add if one goes missing.
            let mut last: Option<(&str, u64, &Path)> = None;
            for f in &found {
                let Some((pre, n)) = number(&name(f)) else {
                    break;
                };
                let want = match last {
                    Some((lp, ln, _)) if lp == pre => ln + 1,
                    _ => 1,
                };
                // n < want: a second copy (data1.doi and data1.bin), or a 0.
                ensure!(
                    n <= want,
                    "{} has no {pre}{want}{}: the download looks incomplete. Get the missing file(s) before extracting",
                    p.display(),
                    match last {
                        Some((lp, _, lf)) if lp == pre => format!(
                            " between {} and {}",
                            lf.file_name().unwrap_or_default().to_string_lossy(),
                            f.file_name().unwrap_or_default().to_string_lossy()
                        ),
                        _ => format!(
                            " before {}",
                            f.file_name().unwrap_or_default().to_string_lossy()
                        ),
                    }
                );
                last = Some((pre, n, f));
            }
            out.extend(found);
        } else {
            ensure!(
                freearc::sniff(p) || zip::sniff(p),
                "{} is not a FreeArc or ZIP archive",
                p.display()
            );
            out.push(p.clone());
        }
    }
    ensure!(
        !out.is_empty(),
        "name a repack folder or its data files\n\n{USAGE}"
    );
    ensure!(
        out.iter().all(|f| zip::sniff(f)) || !out.iter().any(|f| zip::sniff(f)),
        "extract ZIPs and repack data files in separate runs"
    );
    Ok(out
        .into_iter()
        .map(|p| std::path::absolute(&p).unwrap_or(p))
        .collect())
}

/// A data file's index: FreeArc's, or a ZIP's read into the same shape.
fn open_data(p: &Path) -> Result<freearc::Archive> {
    if zip::sniff(p) {
        zip::open(p)
    } else {
        freearc::open(p)
    }
}

/// What a data file is to the installer, judged by its name.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    /// DODI's `data<N>` and FitGirl's `fg-<N>`: always installed.
    Required,
    /// A language pack: `English.doi`, `fg-selective-french.bin`.
    Language,
    /// Anything else the installer offers: movies, artwork, soundtracks.
    Optional,
}

/// Names repacks give their language packs, lowercase. ponytail: a fixed
/// list; a pack it misses shows up as an extra, ticked, so nothing is lost.
const LANGUAGES: &[&str] = &[
    "english",
    "french",
    "german",
    "italian",
    "spanish",
    "japanese",
    "russian",
    "polish",
    "portuguese",
    "brazilian",
    "chinese",
    "korean",
    "czech",
    "turkish",
    "arabic",
    "dutch",
    "hungarian",
    "thai",
    "ukrainian",
    "swedish",
    "norwegian",
    "danish",
    "finnish",
    "greek",
    "romanian",
    "vietnamese",
    "indonesian",
    "hebrew",
    "latam",
    "mexican",
];

/// The language a data file holds, judged by its name, if any.
fn language(name: &str) -> Option<&'static str> {
    let stem = Path::new(name)
        .file_stem()?
        .to_string_lossy()
        .to_lowercase();
    let rest = ["fg-selective-", "fg-optional-", "selective-", "optional-"]
        .iter()
        .find_map(|p| stem.strip_prefix(p))
        .unwrap_or(&stem);
    let first = rest.split(['-', '_', ' ', '.']).next()?;
    LANGUAGES.iter().copied().find(|l| *l == first)
}

/// `data<N>` or `fg-<N>`: the prefix and N, which DODI leaves unpadded.
fn number(name: &str) -> Option<(&'static str, u64)> {
    let stem = Path::new(name)
        .file_stem()?
        .to_string_lossy()
        .to_lowercase();
    ["data", "fg-"].into_iter().find_map(|pre| {
        let r = stem.strip_prefix(pre)?;
        r.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| r.parse().ok())
            .flatten()
            .map(|n| (pre, n))
    })
}

/// The installer's order: required files by number (data2 before data10),
/// then the rest by name.
fn order(name: &str) -> (bool, Option<(&'static str, u64)>, String) {
    let n = number(name);
    (n.is_none(), n, name.to_lowercase())
}

fn kind(name: &str) -> Kind {
    if number(name).is_some() {
        Kind::Required
    } else if language(name).is_some() {
        Kind::Language
    } else {
        Kind::Optional
    }
}

/// Every data file in `dir`, what it is, and whether it starts ticked: all
/// but the languages other than English. Errs toward extracting: with no
/// required file recognised, all are required, and without an English
/// pack every language starts ticked.
fn parts(dir: &Path) -> Result<Vec<(PathBuf, Kind, bool)>> {
    let files = data_files(&[dir.to_path_buf()])?;
    let name = |p: &Path| {
        p.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    };
    let mut kinds: Vec<Kind> = files.iter().map(|f| kind(&name(f))).collect();
    if !kinds.contains(&Kind::Required) {
        kinds.fill(Kind::Required);
    }
    let english = |f: &Path| language(&name(f)) == Some("english");
    let has_english = files
        .iter()
        .zip(&kinds)
        .any(|(f, k)| *k == Kind::Language && english(f));
    Ok(files
        .into_iter()
        .zip(kinds)
        .map(|(f, k)| {
            let on = k != Kind::Language || !has_english || english(&f);
            (f, k, on)
        })
        .collect())
}

/// One line per data file for the app: kind, on or off, packed and unpacked
/// bytes, name; tab-separated.
fn parts_cmd(dir: &Path) -> Result<()> {
    for (f, k, on) in parts(dir)? {
        let a = open_data(&f).with_context(|| format!("reading the index of {}", f.display()))?;
        println!(
            "{}\t{}\t{}\t{}\t{}",
            format!("{k:?}").to_lowercase(),
            if on { "on" } else { "off" },
            a.len,
            a.unpacked(),
            f.file_name().unwrap_or_default().to_string_lossy()
        );
    }
    Ok(())
}

fn list(path: &Path) -> Result<()> {
    let a = open_data(path)?;
    say!(
        "{}: {} entries, {} packed, {} unpacked",
        path.display(),
        a.entries(),
        human(a.len),
        human(a.unpacked())
    );
    for b in &a.blocks {
        say!(
            "  block '{}' at {}: {} packed, {} unpacked, {} entries",
            b.method,
            b.start,
            human(b.packed),
            human(b.unpacked()),
            b.entries.len()
        );
        for e in &b.entries {
            if e.is_dir {
                say!("    {}\\", e.path.display());
            } else {
                say!(
                    "    {}  {}  crc {:08x}",
                    e.path.display(),
                    human(e.size),
                    e.crc
                );
            }
        }
    }
    Ok(())
}

/// What the disk must have free, from a replay of the run: each data file
/// adds its output, rounded up to 4 KiB per file; with --free-as-read it gives
/// back its packed bytes as it goes, with --delete-each once it is done.
/// ponytail: --free-as-read assumes output and freed input keep pace; a
/// decoder that runs far ahead needs more, and the run pauses on a full disk.
fn space_needed(jobs: &[(PathBuf, freearc::Archive)], consume: bool, delete_each: bool) -> u64 {
    let mut written = 0u64;
    let mut freed = 0u64;
    let mut peak = 0u64;
    for (_, a) in jobs {
        let out: u64 = a
            .blocks
            .iter()
            .flat_map(|b| &b.entries)
            .map(|e| e.size.next_multiple_of(4096))
            .sum();
        if consume {
            peak = peak.max((written + out).saturating_sub(freed + a.len));
        } else {
            peak = peak.max((written + out).saturating_sub(freed));
        }
        written += out;
        if consume || delete_each {
            freed += a.len;
        }
    }
    peak
}

fn confirm(question: &str) -> Result<bool> {
    eprint!("{question} Type yes to go on: ");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().eq_ignore_ascii_case("yes"))
}

/// `<hash> *..\fg-01.bin` lines of FitGirl's MD5 lists: (lowercase file
/// name, lowercase hash).
fn md5_list(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let (h, f) = l.trim().split_once(' ')?;
            let f = f.trim_start_matches('*').rsplit(['\\', '/']).next()?;
            (h.len() == 32 && h.bytes().all(|b| b.is_ascii_hexdigit()))
                .then(|| (f.to_lowercase(), h.to_lowercase()))
        })
        .collect()
}

/// Checks the data files against the repack's `MD5\*.md5`, when it has one. A
/// torrent can leave a file damaged in the middle, where nothing else looks,
/// and some decoders then hang instead of failing (CONTROL's rzw did). A
/// damaged language or extra is left out, as an incomplete one is; a damaged
/// game file stops the run.
fn md5_check(files: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    let dir = files[0].parent().unwrap_or(Path::new(".")).join("MD5");
    let want: std::collections::HashMap<String, String> = fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("md5")))
        .flat_map(|p| md5_list(&fs::read_to_string(p).unwrap_or_default()))
        .collect();
    if want.is_empty() {
        return Ok(files);
    }
    note!("checking the data files against the repack's MD5 list");
    let mut kept = Vec::new();
    for f in files {
        let n = f
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase();
        if let Some(h) = want.get(&n) {
            let got: String = win::md5(&f)
                .with_context(|| format!("reading {}", f.display()))?
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            if &got != h {
                ensure!(
                    kind(&n) != Kind::Required,
                    "{n} doesn't match the repack's MD5 list, so the download is damaged; \
                     recheck it in your torrent client. Nothing was changed"
                );
                note!("skipping {n}: damaged (it doesn't match the repack's MD5 list)");
                continue;
            }
        }
        kept.push(f);
    }
    ensure!(!kept.is_empty(), "every data file is damaged");
    Ok(kept)
}

fn extract_cmd(a: Args) -> Result<()> {
    let out = a
        .out
        .clone()
        .context("say where to extract with -o <folder>")?;
    win::watch_memory();
    let files = data_files(&a.inputs)?;
    let files = md5_check(files)?;
    let src_dir = files[0].parent().map(Path::to_path_buf).unwrap_or_default();
    let lname = |p: &Path| {
        p.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase()
    };
    // A Setup.exe next to a ZIP is some other download's: neither its steps
    // nor its decoders apply.
    let zips = zip::sniff(&files[0]);
    let steps = if zips { None } else { recipe_for(&src_dir)? };
    // Archives Setup unpacks into its temp folder for later steps: (file,
    // the folder in it that's wanted, index).
    let mut tools = Vec::new();
    let files = match &steps {
        None => files,
        Some(steps) => {
            // Setup's order, which isn't the number order: Cyberpunk's goes
            // fg-07, 05, 04, 03, 08, 02, 01, the languages, then fg-06.
            let mut app = Vec::new();
            for s in steps {
                let recipe::Step::Extract {
                    data,
                    tools: t,
                    sub,
                    ..
                } = s
                else {
                    continue;
                };
                if *t {
                    let path = src_dir.join(data);
                    let arc = freearc::open(&path).with_context(|| {
                        format!("Setup unpacks its tools from {}", path.display())
                    })?;
                    tools.push((path, sub.clone(), arc));
                } else if let Some(f) = files.iter().find(|f| lname(f) == data.to_lowercase()) {
                    app.push(f.clone());
                } else {
                    // A language or extra left out is a choice; the game isn't.
                    ensure!(
                        number(data).is_none(),
                        "Setup.exe unpacks {data}, which is missing, empty or not chosen; \
                         the game can't be installed without it"
                    );
                }
            }
            for f in &files {
                let n = lname(f);
                if !app.contains(f) && !tools.iter().any(|(t, ..)| lname(t) == n) {
                    note!("skipping {n}: Setup.exe doesn't unpack it");
                }
            }
            say!(
                "following Setup.exe's install steps: {} tool archive(s), {} data file(s), {} command(s) it runs",
                tools.len(),
                app.len(),
                steps
                    .iter()
                    .filter(|s| matches!(s, recipe::Step::Run { .. }))
                    .count()
            );
            ensure!(
                !app.is_empty(),
                "none of these data files is one Setup.exe unpacks"
            );
            app
        }
    };
    let mut jobs = Vec::new();
    for f in &files {
        ensure!(
            !extract::marker(f).exists(),
            "{} was partly freed by an earlier run, which v0.1 can't continue. Its data is gone; \
             the repack has to be downloaded again.",
            f.display()
        );
        let arc = open_data(f).with_context(|| format!("reading the index of {}", f.display()))?;
        jobs.push((f.clone(), arc));
    }

    fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
    let out = std::path::absolute(&out)?;
    let host = match &a.host {
        Some(h) => h.clone(),
        None => std::env::current_exe()?.with_file_name("unrepack-clshost.exe"),
    };
    let work = a.work.clone().unwrap_or_else(|| out.join(".unrepack-work"));
    let (dir, carved) = if zips {
        (Err("ZIPs need no decoders".into()), None)
    } else {
        find_decoders(a.decoders.as_deref(), &files[0])?
    };
    let mut dec = Decoders {
        dir,
        host,
        work,
        pending: Vec::new(),
    };
    // With Setup's steps, the decoders' folder stands in for its {tmp}: its
    // tools are unpacked into it and its decoder settings changed there.
    let mut places = None;
    if let Some(steps) = &steps {
        let tmp = carved.as_ref().map(|c| c.0.clone()).context(
            "this repack's Setup.exe unpacks its own tools, so unrepack reads the decoders out of it; \
             drop --decoders and keep Setup.exe next to the data files",
        )?;
        for (f, sub, arc) in &tools {
            for b in &arc.blocks {
                dec.check(b).with_context(|| format!("{}", f.display()))?;
                for e in b.entries.iter().filter(|e| !e.is_dir) {
                    let rel = e.path.to_string_lossy().into_owned();
                    let rel = if sub.is_empty() {
                        Some(rel.as_str())
                    } else {
                        rel.get(..sub.len() + 1)
                            .filter(|h| h.eq_ignore_ascii_case(&format!("{sub}\\")))
                            .map(|_| &rel[sub.len() + 1..])
                    };
                    if let Some(r) = rel {
                        dec.pending.push(tmp.join(r));
                    }
                }
            }
        }
        for s in steps {
            if let recipe::Step::Ini {
                file,
                section,
                key,
                value,
            } = s
            {
                set_ini(&tmp.join(file), section, key, value)?;
            }
        }
        places = Some(recipe::Places {
            tmp,
            app: out.clone(),
            src: src_dir.clone(),
        });
    }

    // Everything refusable is refused here, before anything changes.
    let mut seen = std::collections::HashMap::new();
    for (f, arc) in &jobs {
        for b in &arc.blocks {
            dec.check(b).with_context(|| format!("{}", f.display()))?;
            for e in &b.entries {
                let key = e.path.to_string_lossy().to_lowercase();
                if let Some(first) = seen.insert(key, f).filter(|_| !e.is_dir) {
                    let (a, b) = (
                        first.file_name().unwrap_or_default().display(),
                        f.file_name().unwrap_or_default().display(),
                    );
                    ensure!(first != f, "{} is twice in {b}", e.path.display());
                    // Data files extract in order, and a file's last copy is
                    // renamed over the earlier one.
                    note!(
                        "{} is in both {a} and {b}; {b}'s copy is kept, as the installer would",
                        e.path.display()
                    );
                }
                let t = out.join(&e.path);
                ensure!(
                    e.is_dir || !t.exists(),
                    "{} already exists; extract into an empty folder",
                    t.display()
                );
            }
        }
    }
    if let (Some(steps), Some(setup)) = (&steps, setup_in(&src_dir)) {
        let own = inno::carve_app(&setup, None, &|c| chosen_component(steps, &jobs, c))?;
        for n in &own {
            ensure!(
                !seen.contains_key(&n.to_lowercase()),
                "Setup.exe puts {n} in the game folder, and a data file has it too; \
                 unrepack can't tell which is meant"
            );
        }
        say!(
            "Setup.exe puts these in the game folder itself: {}",
            own.join(", ")
        );
    }
    let out_volume = win::volume(&out)?;
    let same_volume = jobs
        .iter()
        .all(|(f, _)| win::volume(f).ok() == Some(out_volume));
    let mut consume = a.free_as_read;
    if consume {
        let all_sparse = jobs.iter().all(|(f, _)| {
            fs::File::open(f)
                .map(|h| win::sparse_supported(&h))
                .unwrap_or(false)
        });
        ensure!(
            all_sparse,
            "the repack's drive can't free parts of a file (FAT32 or exFAT?); drop --free-as-read"
        );
        if !same_volume {
            note!(
                "note: the output is on another drive, so freeing the repack doesn't help it; not freeing."
            );
            consume = false;
        }
    }
    let delete_each = a.delete_each && same_volume;
    // ponytail: FitGirl's rebuild scripts get {src} patched in and may read
    // the repack; allow these once it's known they don't.
    ensure!(
        steps.is_none() || !(consume || delete_each),
        "this repack's Setup.exe runs commands after unpacking that may read the repack, so it \
         has to be kept: drop --free-as-read and --delete-each"
    );
    let total_out: u64 = jobs.iter().map(|(_, a)| a.unpacked()).sum();
    let total_in: u64 = jobs.iter().map(|(_, a)| a.len).sum();
    let need = space_needed(&jobs, consume, delete_each);
    let free = win::free_space(&out)?;
    say!(
        "{} data file(s), {} packed, {} to write; free {}, needed {}{}",
        jobs.len(),
        human(total_in),
        human(total_out),
        human(free),
        human(need),
        if consume {
            " (freeing the repack as it is read)"
        } else if delete_each {
            " (deleting each data file when done)"
        } else {
            ""
        }
    );
    if !consume && steps.is_none() {
        let with_free = space_needed(&jobs, same_volume, false);
        if with_free < need {
            say!("with --free-as-read it would need {}", human(with_free));
        }
    }
    say!(
        "decoders keep temp files in {}; that space is not counted.",
        dec.work.display()
    );
    let fits = free >= need;
    if a.check {
        say!("{}", if fits { "it fits" } else { "it does not fit" });
        return Ok(());
    }
    ensure!(
        fits,
        "not enough free space: {} more is needed",
        human(need - free)
    );

    if (consume || delete_each) && !a.yes {
        let q = if consume {
            "This frees the repack's bytes as they are read. If the run stops for any reason after \
             it starts, the repack is lost and must be downloaded again."
        } else {
            "Each data file is deleted once all of it is extracted and checked."
        };
        if !confirm(q)? {
            bail!("stopped; nothing was changed");
        }
    }
    fs::create_dir_all(&dec.work).with_context(|| format!("creating {}", dec.work.display()))?;

    // Setup's steps up to its first unpack into the game folder, and every
    // tool archive: the verify pass and later steps need them. All of this
    // touches only the decoders' folder.
    let mut first = 0;
    if let (Some(steps), Some(pl)) = (&steps, &places) {
        first = steps
            .iter()
            .position(|s| matches!(s, recipe::Step::Extract { tools: false, .. }))
            .unwrap_or(steps.len());
        let early = steps[..first].iter().chain(
            steps[first..]
                .iter()
                .filter(|s| matches!(s, recipe::Step::Extract { tools: true, .. })),
        );
        for s in early {
            setup_step(s, pl, &tools, &[], &dec, &mut Progress::new(0))
                .inspect_err(|_| carved.iter().for_each(Carved::keep))?;
        }
    }

    if a.verify {
        let mut p = Progress::new(total_out);
        let temp = extract::FolderPeak::start(dec.work.clone());
        for (f, arc) in &jobs {
            note!("verifying {}", f.display());
            let r = extract::verify(
                &Job {
                    data: f,
                    archive: arc,
                },
                &dec,
                &mut p,
            );
            if r.is_err() {
                temp.finish();
                return r;
            }
        }
        let temp = temp.finish();
        note!("every file's CRC matches");
        // The plan above was an estimate; the verify pass measured the two
        // things it couldn't know, so check again before anything changes.
        let files: u64 = jobs
            .iter()
            .flat_map(|(_, a)| &a.blocks)
            .flat_map(|b| &b.entries)
            .filter(|e| !e.is_dir)
            .count() as u64;
        let base = if consume {
            p.max_lead + files * 4096 + 2 * extract::PUNCH_STEP
        } else {
            need
        };
        let measured = base + temp;
        let free_now = win::free_space(&out)?;
        say!(
            "measured: decoder temp files peaked at {}, output ran at most {} ahead of the input; needs {}, free {}",
            human(temp),
            human(p.max_lead),
            human(measured),
            human(free_now)
        );
        ensure!(
            free_now >= measured,
            "not enough free space by the measured plan: {} more is needed. Nothing was changed",
            human(measured - free_now)
        );
        note!("extracting");
    }
    let mut p = Progress::new(total_out);
    if let (Some(steps), Some(pl)) = (&steps, &places) {
        // What Setup installs into the game folder itself: FitGirl's hash
        // lists, one per component, which its last step joins up.
        if let Some(setup) = setup_in(&src_dir) {
            let n = inno::carve_app(&setup, Some(&out), &|c| chosen_component(steps, &jobs, c))?;
            say!(
                "copied {} file(s) Setup.exe puts in the game folder itself",
                n.len()
            );
        }
        for s in &steps[first..] {
            if !matches!(s, recipe::Step::Extract { tools: true, .. }) {
                setup_step(s, pl, &tools, &jobs, &dec, &mut p)
                    .inspect_err(|_| carved.iter().for_each(Carved::keep))?;
            }
        }
        // A rebuild step that exits 0 without making its file would leave
        // the game folder short; Setup names the file it makes.
        for s in &steps[first..] {
            if let recipe::Step::Run { makes, .. } = s
                && let Some(f) = recipe::product(makes)
            {
                let path = pl.app.join(f);
                ensure!(
                    path.is_file(),
                    "Setup's step should have made {}, but it isn't there; \
                     the game folder is incomplete",
                    path.display()
                );
            }
        }
        let _ = fs::remove_dir(&dec.work);
        say!(
            "done: {} in {}, and Setup's steps after it",
            human(total_out),
            out.display()
        );
        return Ok(());
    }
    for (f, arc) in &jobs {
        note!("extracting {}", f.display());
        extract::run(
            &Job {
                data: f,
                archive: arc,
            },
            &dec,
            &out,
            consume,
            &mut p,
        )?;
        if delete_each && !consume {
            extract::flush(&out, arc)?;
            fs::remove_file(f).with_context(|| format!("deleting {}", f.display()))?;
        }
    }
    let _ = fs::remove_dir(&dec.work);
    say!("done: {} in {}", human(total_out), out.display());
    Ok(())
}

fn main() -> ExitCode {
    // ponytail: Ctrl+C skips the restore; that window keeps QuickEdit off.
    let _quick_edit = win::quick_edit_off();
    let result = parse()
        .and_then(|a| {
            if let Some(l) = &a.log {
                log::open(l)?;
            }
            Ok(a)
        })
        .and_then(|a| match a.cmd.as_str() {
            "list" => a.inputs.iter().try_for_each(|p| list(p)),
            "extract" => extract_cmd(a),
            "parts" => {
                let [dir] = a.inputs.as_slice() else {
                    bail!("usage: unrepack parts <repack folder>");
                };
                parts_cmd(dir)
            }
            "decoders" => {
                let (Some(setup), Some(out), 1) = (a.inputs.first(), &a.out, a.inputs.len()) else {
                    bail!("usage: unrepack decoders <Setup.exe> -o <folder>");
                };
                let n = inno::carve(setup, out)?;
                say!("{n} files, every SHA-1 matching, in {}", out.display());
                Ok(())
            }
            _ => {
                print!("{USAGE}");
                Ok(())
            }
        });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            note!("error: {e:#}");
            if let Some(m) = win::low_memory_note() {
                note!("{m}");
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_lists_and_hashes() {
        let l = md5_list(
            "; If you miss selective download files - that's OK\r\n\
             C04729DB96D67BD7FFAF723CB1678A83 *..\\fg-optional-spanish-vo.bin\r\n\
             d6d466f7db2de23339320c7fff3f3f70 *fg-01.bin\r\nnot a line\r\n",
        );
        assert_eq!(
            l,
            [
                (
                    "fg-optional-spanish-vo.bin".into(),
                    "c04729db96d67bd7ffaf723cb1678a83".into()
                ),
                (
                    "fg-01.bin".into(),
                    "d6d466f7db2de23339320c7fff3f3f70".into()
                )
            ]
        );
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("abc");
        fs::write(&f, b"abc").unwrap();
        let h: String = win::md5(&f)
            .unwrap()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(h, "900150983cd24fb0d6963f7d28e17f72");
    }

    /// Needs the Cyberpunk 2077 FitGirl repack next to this folder.
    #[test]
    #[ignore]
    fn cyberpunk_setup_steps() {
        let steps = recipe_for(Path::new("../Cyberpunk 2077 [FitGirl Repack]"))
            .unwrap()
            .unwrap();
        let order: Vec<String> = steps
            .iter()
            .filter_map(|s| match s {
                recipe::Step::Extract { data, tools, .. } => {
                    Some(format!("{}{data}", if *tools { "tmp:" } else { "" }))
                }
                _ => None,
            })
            .take(8)
            .collect();
        assert_eq!(
            order,
            [
                "tmp:fg-07.bin",
                "fg-05.bin",
                "fg-04.bin",
                "fg-03.bin",
                "tmp:fg-08.bin",
                "fg-02.bin",
                "fg-optional-credits.bin",
                "fg-01.bin"
            ]
        );
        assert!(
            steps
                .iter()
                .any(|s| matches!(s, recipe::Step::Run { exe, params, dir, .. }
            if exe == r"{tmp}\work\build01.bat"
                && params.is_empty()
                && dir == r"{app}\archive\pc\content\audio_2_soundbanks\"))
        );
    }

    #[test]
    fn unreadable_setup_is_refused_other_setups_fall_back() {
        let d = std::env::temp_dir().join(format!("unrepack-setup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("setup.exe"), b"MZ not inno").unwrap();
        assert!(recipe_for(&d).unwrap().is_none());
        // A folder can't be read as a file: stands in for one an antivirus blocks.
        fs::remove_file(d.join("setup.exe")).unwrap();
        fs::create_dir(d.join("setup.exe")).unwrap();
        let e = recipe_for(&d).unwrap_err();
        assert!(format!("{e:#}").contains("antivirus"), "{e:#}");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn set_ini_replaces_adds_and_maps_folders() {
        let d = std::env::temp_dir().join(format!("unrepack-ini-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        let f = d.join("arc.ini");
        fs::write(&f, "[External compressor:bz3s,bz3]\nheader = 0\nunpackcmd = bzip3 -j fgrplc\n[Other]\nx=1\n").unwrap();
        set_ini(
            &f,
            "external compressor:bz3s,bz3",
            "unpackcmd",
            "bzip3 -j 4",
        )
        .unwrap();
        set_ini(&f, "Other", "TempPath", "{app}").unwrap();
        set_ini(&f, "New", "k", "v").unwrap();
        assert_eq!(
            fs::read_to_string(&f).unwrap(),
            "[External compressor:bz3s,bz3]\r\nheader = 0\r\nunpackcmd=bzip3 -j 4\r\n[Other]\r\nx=1\r\nTempPath=.\r\n[New]\r\nk=v\r\n"
        );
        fs::remove_dir_all(&d).unwrap();
    }

    fn arc(len: u64, out: u64) -> freearc::Archive {
        freearc::Archive {
            len,
            blocks: vec![freearc::Block {
                method: "storing".into(),
                start: 0,
                packed: len,
                entries: vec![freearc::Entry {
                    path: "a".into(),
                    size: out,
                    crc: 0,
                    is_dir: false,
                }],
                crc: freearc::Crc::Ieee,
            }],
        }
    }

    #[test]
    fn parts_by_name() {
        for (n, k) in [
            ("data1.doi", Kind::Required),
            ("DATA12.dodi", Kind::Required),
            ("fg-01.bin", Kind::Required),
            ("English.doi", Kind::Language),
            ("Spanish_Latam.doi", Kind::Language),
            ("fg-selective-french.bin", Kind::Language),
            ("fg-optional-bonus.bin", Kind::Optional),
            ("Artwork_MiniSoundtrack.doi", Kind::Optional),
            ("opening_movie_hi.doi", Kind::Optional),
            ("data.doi", Kind::Optional),
        ] {
            assert_eq!(kind(n), k, "{n}");
        }
        assert_eq!(language("fg-selective-english.bin"), Some("english"));
    }

    #[test]
    fn required_files_sort_by_number() {
        let mut v = [
            "English.doi",
            "data10.doi",
            "data2.doi",
            "Data1.doi",
            "artwork.doi",
        ];
        v.sort_by_key(|n| order(n));
        assert_eq!(
            v,
            [
                "Data1.doi",
                "data2.doi",
                "data10.doi",
                "artwork.doi",
                "English.doi"
            ]
        );
        assert_eq!(number("fg-07.bin"), Some(("fg-", 7)));
    }

    #[test]
    fn space_plan_matches_the_ck3_numbers() {
        // Crusader Kings III [DODI Repack]: data1 then data2.
        let jobs = vec![
            ("1".into(), arc(5_740_077_565, 19_371_573_248)),
            ("2".into(), arc(307_063_704, 496_164_864)),
        ];
        // Nothing freed: all output on top of the repack.
        assert_eq!(space_needed(&jobs, false, false), 19_867_738_112);
        // Whole files deleted when done: the peak is data1's own output, before
        // anything can go; little help with one big data file.
        assert_eq!(space_needed(&jobs, false, true), 19_371_573_248);
        // Freed as read: output minus the whole repack.
        assert_eq!(
            space_needed(&jobs, true, false),
            19_867_738_112 - 6_047_141_269
        );
    }
}
