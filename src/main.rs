//! unrepack: extract game repacks built on FreeArc (DODI, FitGirl and the
//! like) without the installer, optionally giving the repack's space back
//! while it is read.

#[macro_use]
mod log;
mod builtin;
mod extract;
mod freearc;
mod inno;
mod tornado;
mod win;

use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail, ensure};

use extract::{Decoders, Job, Progress, human};

const USAGE: &str = "\
unrepack 0.1: extract FreeArc repack data files without running the installer

Usage:
  unrepack list <data file>
  unrepack extract <repack folder | data files...> -o <output folder> [options]
  unrepack parts <repack folder>              which data files always install, and which
                                              are languages or extras to choose from
  unrepack decoders <Setup.exe> -o <folder>   copy the decoders out of the installer

A folder extracts every data file in it. To choose languages and extras as the
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
                    v0.1: if the run stops after the first freed byte, the repack is lost
  --no-verify       skip the decode-only pass that checks every CRC before anything is
                    freed or deleted (halves the time, removes the safety net)
  --yes             don't ask before freeing or deleting

The repack's decoders are programs that came with the download; unrepack runs them.
";

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
        verify: true,
        yes: false,
    };
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
            "--no-verify" => a.verify = false,
            "--yes" | "-y" => a.yes = true,
            s if s.starts_with('-') => bail!("unknown option {s}\n\n{USAGE}"),
            _ => a.inputs.push(PathBuf::from(arg)),
        }
    }
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

/// A folder of decoders read out of Setup.exe, removed when the run ends.
/// ponytail: a killed run leaves it in %TEMP%; sweep old ones if that piles up.
struct Carved(PathBuf);

impl Drop for Carved {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
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
    let setup = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| {
            p.file_name()
                .is_some_and(|n| n.eq_ignore_ascii_case("setup.exe"))
        });
    let Some(setup) = setup else {
        return Ok((Err(HOW.into()), None));
    };
    let tmp = std::env::temp_dir().join(format!("unrepack-decoders-{}", std::process::id()));
    // A run that was killed leaves its folder; a later run with the same id
    // must not mix that repack's decoders with this one's.
    let _ = fs::remove_dir_all(&tmp);
    let carved = Carved(tmp.clone());
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
                .filter(|p| p.is_file() && freearc::sniff(p))
                .collect();
            let name = |p: &Path| {
                p.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_lowercase()
            };
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
                freearc::sniff(p),
                "{} is not a FreeArc archive",
                p.display()
            );
            out.push(p.clone());
        }
    }
    ensure!(
        !out.is_empty(),
        "name a repack folder or its data files\n\n{USAGE}"
    );
    Ok(out
        .into_iter()
        .map(|p| std::path::absolute(&p).unwrap_or(p))
        .collect())
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
        let a =
            freearc::open(&f).with_context(|| format!("reading the index of {}", f.display()))?;
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
    let a = freearc::open(path)?;
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

fn extract_cmd(a: Args) -> Result<()> {
    let out = a
        .out
        .clone()
        .context("say where to extract with -o <folder>")?;
    let files = data_files(&a.inputs)?;
    let mut jobs = Vec::new();
    for f in &files {
        ensure!(
            !extract::marker(f).exists(),
            "{} was partly freed by an earlier run, which v0.1 can't continue. Its data is gone; \
             the repack has to be downloaded again.",
            f.display()
        );
        let arc =
            freearc::open(f).with_context(|| format!("reading the index of {}", f.display()))?;
        jobs.push((f.clone(), arc));
    }

    fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
    let out = std::path::absolute(&out)?;
    let host = match &a.host {
        Some(h) => h.clone(),
        None => std::env::current_exe()?.with_file_name("unrepack-clshost.exe"),
    };
    let work = a.work.clone().unwrap_or_else(|| out.join(".unrepack-work"));
    let (dir, _carved) = find_decoders(a.decoders.as_deref(), &files[0])?;
    let dec = Decoders { dir, host, work };

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
    if !consume {
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
            fs::remove_file(f).with_context(|| format!("deleting {}", f.display()))?;
        }
    }
    let _ = fs::remove_dir(&dec.work);
    say!("done: {} in {}", human(total_out), out.display());
    Ok(())
}

fn main() -> ExitCode {
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
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
