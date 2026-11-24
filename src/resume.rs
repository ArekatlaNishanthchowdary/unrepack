//! Picking an extraction up where a crash, a power cut or an error left it.
//!
//! `<output>\.unrepack-resume` says what the run is: its mode and every data
//! file with its size, written once, whole (a temporary file renamed into
//! place). After that, lines are appended and forced to disk one by one:
//!
//! - `done <f> <b>`: blocks 0 to `b` of data file `f` are on disk. Written at
//!   checkpoints, only after the files of those blocks are flushed.
//! - `begin <i>`: Setup's install step `i`, a command, has started.
//! - `step <i>`: Setup's install step `i` is finished.
//!
//! A line cut short by a crash is ignored. The rule: the journal never says
//! a block is done before its files are on disk, and no packed byte of a
//! block is freed by `--free-per-block` before the journal says it is done.
//!
//! `--free-as-read` frees blocks the journal doesn't have yet, so a rerun
//! looks at the blocks after the last checkpoint itself: one whose files
//! are all there with the right CRCs is done (each was flushed before it got
//! its name), one whose start reads as freed is lost, and the first that is
//! neither is decoded again.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::win;

pub const NAME: &str = ".unrepack-resume";
/// Output between checkpoints, at most: what a crash can make a rerun redo,
/// and with `--free-per-block`, how much packed input waits to be freed.
pub const CHECKPOINT_BYTES: u64 = 1 << 30;
const CHECKPOINT_TIME: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Keep,
    DeleteEach,
    /// Frees packed bytes as the decoders read them.
    FreeAsRead,
    /// Frees a block's packed bytes once its files are on disk.
    FreePerBlock,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Keep => "keep",
            Mode::DeleteEach => "delete-each",
            Mode::FreeAsRead => "free-as-read",
            Mode::FreePerBlock => "free-per-block",
        }
    }
    pub fn frees(self) -> bool {
        matches!(self, Mode::FreeAsRead | Mode::FreePerBlock)
    }
    /// Whether data files are gone once extracted.
    pub fn removes(self) -> bool {
        self != Mode::Keep
    }
}

/// What a journal was started for.
pub struct Header {
    pub mode: Mode,
    /// Data files in extraction order.
    pub data: Vec<DataFile>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct DataFile {
    pub path: PathBuf,
    /// Its size, which freeing leaves as it is.
    pub len: u64,
    pub blocks: usize,
}

impl Header {
    fn text(&self) -> String {
        let mut s = format!("unrepack-resume 1\nmode {}\n", self.mode.name());
        for d in &self.data {
            s += &format!("data {} {} {}\n", d.len, d.blocks, d.path.display());
        }
        s + "end\n"
    }
}

pub struct Journal {
    path: PathBuf,
    file: File,
    pub header: Header,
    /// Per data file, how many of its blocks are on disk.
    done: Vec<usize>,
    steps: usize,
    /// The last command step that started.
    begun: Option<usize>,
    pending: Vec<PathBuf>,
    pending_block: Option<(usize, usize)>,
    pending_bytes: u64,
    since: Instant,
    /// Set when the repack is kept and nothing follows the last data file:
    /// its last checkpoint isn't flushed, since the journal is deleted next.
    /// Flushing 5,000 files there doubled a 15 s extraction.
    pub lazy_end: bool,
}

/// Reads the journal in `out`, if there is one.
pub fn peek(out: &Path) -> Result<Option<Journal>> {
    let path = out.join(NAME);
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let bad = || {
        format!(
            "{} is damaged; extract into an empty folder",
            path.display()
        )
    };
    let mut lines = text.lines();
    if lines.next() != Some("unrepack-resume 1") {
        bail!(
            "{} was written by a newer unrepack, or is damaged",
            path.display()
        );
    }
    let mode = match lines.next().and_then(|l| l.strip_prefix("mode ")) {
        Some("keep") => Mode::Keep,
        Some("delete-each") => Mode::DeleteEach,
        Some("free-as-read") => Mode::FreeAsRead,
        Some("free-per-block") => Mode::FreePerBlock,
        _ => bail!(bad()),
    };
    let mut data = Vec::new();
    loop {
        let l = lines.next().with_context(bad)?;
        if l == "end" {
            break;
        }
        let mut w = l.strip_prefix("data ").with_context(bad)?.splitn(3, ' ');
        let mut num = || {
            w.next()
                .and_then(|s| s.parse::<u64>().ok())
                .with_context(bad)
        };
        let (len, blocks) = (num()?, num()? as usize);
        data.push(DataFile {
            path: PathBuf::from(w.next().with_context(bad)?),
            len,
            blocks,
        });
    }
    let n = data.len();
    let mut j = Journal {
        file: OpenOptions::new()
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?,
        path,
        header: Header { mode, data },
        done: vec![0; n],
        steps: 0,
        begun: None,
        pending: Vec::new(),
        pending_block: None,
        pending_bytes: 0,
        since: Instant::now(),
        lazy_end: false,
    };
    // Only the last line can be cut short; anything unreadable is skipped.
    for l in lines {
        let w: Vec<&str> = l.split(' ').collect();
        let num = |i: usize| w.get(i).and_then(|s| s.parse::<u64>().ok());
        match (w[0], num(1), num(2), num(3)) {
            ("done", Some(f), Some(b), None) if (f as usize) < n => {
                j.done[f as usize] = j.done[f as usize].max(b as usize + 1);
            }
            ("begin", Some(i), None, None) => j.begun = Some(i as usize),
            ("step", Some(i), None, None) => j.steps = j.steps.max(i as usize + 1),
            _ => {}
        }
    }
    Ok(Some(j))
}

/// Starts a journal in `out`.
pub fn create(out: &Path, header: Header) -> Result<Journal> {
    let path = out.join(NAME);
    let tmp = out.join(format!("{NAME}.tmp"));
    let mut f = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    f.write_all(header.text().as_bytes())?;
    f.sync_all()?;
    drop(f);
    fs::rename(&tmp, &path).with_context(|| format!("creating {}", path.display()))?;
    Ok(peek(out)?.expect("the journal was just written"))
}

impl Journal {
    pub fn blocks_done(&self, f: usize) -> usize {
        self.done[f]
    }
    /// Whether every block of data file `f` is on disk.
    pub fn complete(&self, f: usize) -> bool {
        self.done[f] >= self.header.data[f].blocks
    }
    pub fn steps_done(&self) -> usize {
        self.steps
    }
    /// The command step that was running when the last run stopped.
    pub fn interrupted_step(&self) -> Option<usize> {
        self.begun.filter(|&i| i >= self.steps)
    }

    fn append(&mut self, line: &str) -> Result<()> {
        self.file
            .write_all(format!("{line}\n").as_bytes())
            .and_then(|()| self.file.sync_data())
            .with_context(|| format!("writing {}", self.path.display()))
    }

    /// Setup's command step `i` is starting.
    pub fn begin_step(&mut self, i: usize) -> Result<()> {
        self.append(&format!("begin {i}"))?;
        self.begun = Some(i);
        Ok(())
    }

    /// Block `b` of data file `f` is decoded and its files written. Every
    /// so often, and at a data file's last block, those files are flushed
    /// and the journal says so: then this returns the last block it covers.
    pub fn finished(
        &mut self,
        f: usize,
        b: usize,
        files: impl IntoIterator<Item = PathBuf>,
        bytes: u64,
        last: bool,
    ) -> Result<Option<usize>> {
        if last && self.lazy_end && f + 1 == self.header.data.len() {
            // The run ends here and the journal goes with it; a stop before
            // that only redoes this batch, from the repack that is kept.
            self.pending.clear();
            self.pending_block = None;
            return Ok(None);
        }
        self.pending.extend(files);
        self.pending_block = Some((f, b));
        self.pending_bytes += bytes;
        // The resume tests checkpoint after every block.
        let every = std::env::var("UNREPACK_TEST_CHECKPOINT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(CHECKPOINT_BYTES);
        if last || self.pending_bytes >= every || self.since.elapsed() >= CHECKPOINT_TIME {
            return self.checkpoint();
        }
        Ok(None)
    }

    fn checkpoint(&mut self) -> Result<Option<usize>> {
        let Some((f, b)) = self.pending_block.take() else {
            return Ok(None);
        };
        let files = std::mem::take(&mut self.pending);
        // Freed as read, each file was flushed as it was finished.
        if self.header.mode != Mode::FreeAsRead {
            crate::extract::flush(&files)?;
        }
        crash_point("flushed", 1);
        self.append(&format!("done {f} {b}"))?;
        self.done[f] = b + 1;
        self.pending_bytes = 0;
        self.since = Instant::now();
        crash_point("journaled", 1);
        Ok(Some(b))
    }

    /// Block `b` of data file `f` turned out to be on disk already.
    pub fn mark_done(&mut self, f: usize, b: usize) -> Result<()> {
        self.append(&format!("done {f} {b}"))?;
        self.done[f] = self.done[f].max(b + 1);
        Ok(())
    }

    pub fn step_done(&mut self, i: usize) -> Result<()> {
        self.append(&format!("step {i}"))?;
        self.steps = i + 1;
        Ok(())
    }

    /// Everything is done: the journal goes.
    pub fn finish(self) -> Result<()> {
        drop(self.file);
        fs::remove_file(&self.path).with_context(|| format!("deleting {}", self.path.display()))
    }
}

/// Whether any of the packed bytes `start..end` of `data` are freed. Freeing
/// goes front to back in whole units (`win::punch`), so if any of a block's
/// bytes went, the unit holding its start or the one after it did, and a
/// freed unit reads as all zeros. Packed data never holds that many zeros
/// in a row, so this also tells a partly freed file from a fresh copy.
// ponytail: a `storing` block of zeros would read as freed; compressed
// repack data doesn't, and a false "freed" only asks for a fresh download.
pub fn freed(data: &Path, start: u64, end: u64) -> Result<bool> {
    let mut f = File::open(data).with_context(|| format!("opening {}", data.display()))?;
    let len = f.metadata()?.len();
    let first = start / win::UNIT * win::UNIT;
    let mut buf = vec![0u8; win::UNIT as usize];
    for unit in [first, first + win::UNIT] {
        if unit >= end || unit + win::UNIT > len {
            break;
        }
        f.seek(SeekFrom::Start(unit))?;
        f.read_exact(&mut buf)
            .with_context(|| format!("reading {}", data.display()))?;
        if buf.iter().all(|&b| b == 0) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// For the resume tests: `UNREPACK_TEST_CRASH=<point>:<n>` aborts the run,
/// as a crash would, once `point` is passed `n` times (or, for `bytes`,
/// once that much output is written).
// ponytail: one counter for all points; a test sets one point per run.
pub fn crash_point(point: &str, amount: u64) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEEN: AtomicU64 = AtomicU64::new(0);
    let Some(v) = std::env::var_os("UNREPACK_TEST_CRASH") else {
        return;
    };
    let v = v.to_string_lossy();
    let Some((p, n)) = v.split_once(':') else {
        return;
    };
    if p == point && SEEN.fetch_add(amount, Ordering::Relaxed) + amount >= n.parse().unwrap_or(1) {
        std::process::abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_journal_reads_back_and_ignores_a_torn_line() {
        let dir = tempfile::tempdir().unwrap();
        let mut j = create(
            dir.path(),
            Header {
                mode: Mode::FreeAsRead,
                data: vec![
                    DataFile {
                        path: "C:\\r\\data1.doi".into(),
                        len: 100,
                        blocks: 2,
                    },
                    DataFile {
                        path: "C:\\r\\data 2.doi".into(),
                        len: 7,
                        blocks: 1,
                    },
                ],
            },
        )
        .unwrap();
        j.finished(0, 0, [], 5, false).unwrap();
        assert_eq!(j.finished(0, 1, [], 5, true).unwrap(), Some(1));
        j.begin_step(3).unwrap();
        j.step_done(3).unwrap();
        j.begin_step(5).unwrap();
        drop(j);
        let mut f = OpenOptions::new()
            .append(true)
            .open(dir.path().join(NAME))
            .unwrap();
        // A whole line, then one a crash cut short.
        f.write_all(b"done 1 0\ndo").unwrap();
        drop(f);
        let j = peek(dir.path()).unwrap().unwrap();
        assert_eq!(j.header.mode, Mode::FreeAsRead);
        assert_eq!(
            j.header.data[1],
            DataFile {
                path: "C:\\r\\data 2.doi".into(),
                len: 7,
                blocks: 1
            }
        );
        assert!(j.complete(0) && j.complete(1));
        assert_eq!(j.blocks_done(0), 2);
        assert_eq!(j.blocks_done(1), 1);
        assert_eq!(j.steps_done(), 4);
        assert_eq!(j.interrupted_step(), Some(5));
        j.finish().unwrap();
        assert!(peek(dir.path()).unwrap().is_none());
    }
}
