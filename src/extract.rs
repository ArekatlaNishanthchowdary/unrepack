//! Decoding a FreeArc data file block by block into files, optionally
//! freeing the archive's bytes as the first decoder consumes them.
//!
//! v0.1 has no resume: once a byte is freed, a crash or a decoder failure
//! loses the data file. That is why a verify pass (decode everything, check
//! every CRC, write nothing) runs first by default, and why a marker file
//! next to the data file makes any later run refuse a half-consumed one.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

use crate::freearc::{Archive, Block};
use crate::win;

/// How far the reader runs ahead of the last punch: punching is a syscall
/// per call, and the decoder holds its own buffers anyway.
pub const PUNCH_STEP: u64 = 64 << 20;
const PART: &str = ".unrepack-part";

#[derive(Debug, Clone, PartialEq)]
pub enum Stage {
    Storing,
    /// A named method; [`Decoders::resolve`] says what runs it.
    Method {
        name: String,
        params: String,
    },
}

/// FreeArc methods compiled into unarc.dll that unrepack doesn't port yet.
const BUILT_IN: &[&str] = &[
    "exe", "exe2", "lzma", "lzma2", "ppmd", "tornado", "lzp",
    "grzip", "rep", "dict", "mm", "tta", "lz4", "bcj", "bcj2",
];

/// What runs one stage.
#[derive(Debug, Clone)]
enum Decode {
    /// A FreeArc CLS plug-in, `cls-<name>.dll`, run through the 32-bit host.
    Cls(PathBuf),
    /// An `[External compressor]` from the repack's arc.ini.
    Ext(External),
    /// FreeArc's `4x4`: a 4-byte stream header (0), then frames of unpacked
    /// size, packed size (u32 LE each) and the packed bytes, each frame
    /// decoded on its own by the inner method.
    FourByFour(External),
    /// FreeArc built-ins ported in `builtin.rs`.
    Delta,
    /// `dispack070`, with its block size.
    Dispack(usize),
}

/// FreeArc's memory sizes as methods write them: `8mb`, `512k`, `64m`, `1g`,
/// `100b`. A bare number is refused: FreeArc's default unit isn't certain.
fn mem_param(p: &str) -> Option<usize> {
    let p = p.to_ascii_lowercase();
    let digits = p.find(|c: char| !c.is_ascii_digit())?;
    let n: usize = p[..digits].parse().ok()?;
    let mult = match &p[digits..] {
        "b" => 1,
        "k" | "kb" => 1 << 10,
        "m" | "mb" => 1 << 20,
        "g" | "gb" => 1 << 30,
        _ => return None,
    };
    n.checked_mul(mult)
}

#[derive(Debug, Clone)]
struct External {
    name: String,
    exe: PathBuf,
    args: Vec<String>,
    /// The names the tool reads and writes in its folder (packed, data);
    /// `None` when it streams stdin to stdout.
    files: Option<(String, String)>,
}

/// The `[External compressor:a,b]` sections of an arc.ini, keys lowercased.
fn arc_ini(text: &str) -> Vec<(Vec<String>, Vec<(String, String)>)> {
    let mut out: Vec<(Vec<String>, Vec<(String, String)>)> = Vec::new();
    let mut in_ext = false;
    for line in text.lines().map(str::trim) {
        if let Some(h) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_ext = false;
            if let Some((kind, names)) = h.split_once(':')
                && kind.trim().eq_ignore_ascii_case("External compressor")
            {
                in_ext = true;
                out.push((names.split(',').map(|n| n.trim().to_string()).collect(), Vec::new()));
            }
        } else if in_ext && let Some((k, v)) = line.split_once('=') {
            out.last_mut()
                .unwrap()
                .1
                .push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    out
}

impl External {
    /// Builds the decoder for `name` from an arc.ini section, with programs
    /// taken only from `dir`.
    fn from_ini(text: &str, dir: &Path, name: &str, params: &str) -> Result<Option<External>> {
        let Some((_, keys)) = arc_ini(text).into_iter().find(|(n, _)| n.iter().any(|n| n == name))
        else {
            return Ok(None);
        };
        let get = |k: &str| keys.iter().find(|(x, _)| x == k).map(|(_, v)| v.as_str());
        let cmd = get("unpackcmd")
            .with_context(|| format!("arc.ini has no unpackcmd for '{name}'"))?;
        // header=1, FreeArc's default, adds a header unrepack doesn't parse.
        ensure!(
            get("header") == Some("0"),
            "arc.ini method '{name}' doesn't set header = 0; not supported"
        );
        let sub = |s: &str| {
            s.replace("$$arcpackedfile$$", "packed")
                .replace("$$arcdatafile$$", "data")
        };
        let options = params.replace(':', " ");
        let mut toks = Vec::new();
        for t in cmd.split_whitespace() {
            match t {
                "<stdin>" | "<stdout>" => {}
                "{options}" => toks.extend(options.split_whitespace().map(String::from)),
                t => toks.push(sub(t)),
            }
        }
        let stdio = cmd.contains("<stdin>");
        ensure!(
            stdio == cmd.contains("<stdout>"),
            "arc.ini method '{name}' streams only one way; not supported"
        );
        ensure!(!toks.is_empty(), "arc.ini method '{name}' has an empty unpackcmd");
        let prog = toks.remove(0);
        ensure!(
            !prog.contains(['/', '\\', ':']),
            "arc.ini method '{name}' names a program outside the decoders folder"
        );
        let exe = [dir.join(&prog), dir.join(format!("{prog}.exe"))]
            .into_iter()
            .find(|p| p.is_file())
            .with_context(|| format!("arc.ini method '{name}' needs {prog}, missing from {}", dir.display()))?;
        let files = (!stdio).then(|| {
            (
                sub(get("packedfile").unwrap_or("$$arcpackedfile$$.tmp")),
                sub(get("datafile").unwrap_or("$$arcdatafile$$.tmp")),
            )
        });
        if let Some((p, d)) = &files {
            ensure!(
                [p, d].iter().all(|f| !f.contains(['/', '\\', ':']) && !f.is_empty() && p != d),
                "arc.ini method '{name}' uses odd file names"
            );
        }
        Ok(Some(External {
            name: name.to_string(),
            exe,
            args: toks,
            files,
        }))
    }

    /// Decodes everything `input` gives into `out`, in folder `dir` (which
    /// must hold nothing else this tool cares about).
    fn run_once(&self, input: &mut (dyn Read + Send), dir: &Path, out: &mut dyn Write) -> io::Result<u64> {
        let fail = |what: String| io::Error::other(format!("decoder '{}' {what}", self.name));
        let Some((packed, data)) = &self.files else {
            let mut child = Command::new(&self.exe)
                .args(&self.args)
                .current_dir(dir)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            win::adopt(&child);
            let talk = forward(child.stderr.take().unwrap());
            let mut stdin = child.stdin.take().unwrap();
            let n = std::thread::scope(|s| {
                let pump = s.spawn(move || {
                    let r = io::copy(input, &mut stdin);
                    drop(stdin);
                    r
                });
                let n = io::copy(child.stdout.as_mut().unwrap(), out);
                let fed = pump.join().expect("pump thread panicked");
                let n = n?;
                fed?;
                io::Result::Ok(n)
            });
            let st = child.wait()?;
            settle(&[talk]);
            if !st.success() {
                return Err(fail(format!("failed ({st})")));
            }
            return n;
        };
        let (p, d) = (dir.join(packed), dir.join(data));
        let _ = fs::remove_file(&d);
        io::copy(input, &mut File::create(&p)?)?;
        let child = Command::new(&self.exe)
            .args(&self.args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        win::adopt(&child);
        let o = child.wait_with_output()?;
        let said = String::from_utf8_lossy(&[o.stdout, o.stderr].concat()).into_owned();
        let said: Vec<&str> = said.split(['\r', '\n']).filter(|l| !l.trim().is_empty()).collect();
        if !o.status.success() {
            for l in &said {
                crate::log::line(l, true);
            }
            return Err(fail(format!("failed ({})", o.status)));
        }
        for l in &said {
            crate::log::to_file(l);
        }
        let _ = fs::remove_file(&p);
        let n = io::copy(&mut File::open(&d).map_err(|e| fail(format!("wrote no {data}: {e}")))?, out)?;
        fs::remove_file(&d)?;
        Ok(n)
    }

    /// `4x4` around this method. ponytail: one frame at a time; FreeArc
    /// decodes frames on several threads, add that if this is the slow stage.
    fn four_by_four(&self, input: &mut (dyn Read + Send), dir: &Path, out: &mut dyn Write) -> io::Result<u64> {
        let bad = |m: String| io::Error::new(io::ErrorKind::InvalidData, format!("4x4: {m}"));
        let mut h = [0u8; 4];
        input.read_exact(&mut h)?;
        if h != [0; 4] {
            return Err(bad(format!("stream header {h:02x?} not understood")));
        }
        let mut total = 0;
        loop {
            let mut f = [0u8; 8];
            let got = read_full(input, &mut f)?;
            if got == 0 {
                return Ok(total);
            }
            if got < 8 {
                return Err(bad("stream ends inside a frame header".into()));
            }
            let size = u64::from(u32::from_le_bytes(f[..4].try_into().unwrap()));
            let packed = u64::from(u32::from_le_bytes(f[4..].try_into().unwrap()));
            let mut frame = (&mut *input).take(packed);
            let n = self.run_once(&mut frame, dir, out)?;
            if frame.limit() != 0 {
                return Err(bad("stream ends inside a frame".into()));
            }
            if n != size {
                return Err(bad(format!("frame decoded to {n} bytes, its header says {size}")));
            }
            total += n;
        }
    }
}

/// Reads until `buf` is full or the input ends; how much it got.
fn read_full(r: &mut dyn Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..])? {
            0 => break,
            n => got += n,
        }
    }
    Ok(got)
}

/// Decoder messages go through our logger, so they don't strand the status
/// line on screen and they land in --log too.
/// Lines end at `\r` and backspace too, so progress counters (precomp redraws
/// with backspaces) don't pile up into one line; lines without a letter
/// (`42.17% /`) are progress only and dropped, as our status line covers it.
fn forward(mut err: impl Read + Send + 'static) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let (mut buf, mut line) = ([0u8; 4096], Vec::new());
        let say = |l: &mut Vec<u8>| {
            let t = String::from_utf8_lossy(l);
            if t.chars().any(char::is_alphabetic) {
                crate::log::line(t.trim_end(), true);
            }
            l.clear();
        };
        while let Ok(n @ 1..) = err.read(&mut buf) {
            for &b in &buf[..n] {
                if b == b'\r' || b == b'\n' || b == 8 {
                    say(&mut line);
                } else {
                    line.push(b);
                }
            }
        }
        say(&mut line);
    })
}

/// Waits a little for threads to end, so a failing decoder's last words
/// reach the log. Bounded: a decoder's own child can hold a pipe open.
fn settle<T>(hs: &[JoinHandle<T>]) {
    let t = Instant::now();
    while hs.iter().any(|h| !h.is_finished()) && t.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The decode stages of a method, in decoding order (the reverse of the
/// order FreeArc compressed with).
pub fn stages(method: &str) -> Vec<Stage> {
    method
        .split('+')
        .rev()
        .map(|m| {
            let (name, params) = m.split_once(':').unwrap_or((m, ""));
            if name == "storing" {
                Stage::Storing
            } else {
                Stage::Method {
                    name: name.to_string(),
                    params: params.to_string(),
                }
            }
        })
        .collect()
}

pub struct Decoders {
    pub dir: Option<PathBuf>,
    pub host: PathBuf,
    /// Where the decoders run: srep and lolz put temp files in `.\`.
    pub work: PathBuf,
}

impl Decoders {
    /// A CLS plug-in wins over arc.ini, as in FreeArc.
    fn resolve(&self, name: &str, params: &str) -> Result<Decode> {
        let dir = self.dir.as_ref().with_context(|| {
            format!("method '{name}' needs the repack's decoders: pass --decoders <folder>")
        })?;
        let ini = fs::read_to_string(dir.join("arc.ini")).unwrap_or_default();
        if name == "4x4" {
            // 4x4:<its options>:<inner method>[:<inner options>]
            let mut parts = params.split(':');
            let inner = parts
                .by_ref()
                .find(|p| !(p.starts_with(['t', 'i', 'r', 'b']) && p[1..].starts_with(|c: char| c.is_ascii_digit())))
                .context("4x4 without an inner method")?;
            let rest: Vec<&str> = parts.collect();
            let e = External::from_ini(&ini, dir, inner, &rest.join(":"))?.with_context(|| {
                format!("4x4 around '{inner}': unrepack runs 4x4 only around arc.ini methods")
            })?;
            return Ok(Decode::FourByFour(e));
        }
        let p = dir.join(format!("cls-{name}.dll"));
        if p.is_file() {
            ensure!(
                self.host.is_file(),
                "the CLS host {} is missing",
                self.host.display()
            );
            return Ok(Decode::Cls(p));
        }
        if let Some(e) = External::from_ini(&ini, dir, name, params)? {
            return Ok(Decode::Ext(e));
        }
        match name {
            // Its block size and `x` only matter when compressing.
            "delta" => return Ok(Decode::Delta),
            "dispack" | "dispack070" => {
                let mut block = 8 << 20;
                for p in params.split(':').filter(|p| !p.is_empty() && *p != "x") {
                    block = mem_param(p.strip_prefix('b').filter(|r| !r.is_empty() && r.starts_with(|c: char| c.is_ascii_digit())).unwrap_or(p))
                        .filter(|&b| b > 0 && b <= 1 << 30)
                        .with_context(|| format!("method '{name}': parameter '{p}' not understood"))?;
                }
                return Ok(Decode::Dispack(block));
            }
            _ => {}
        }
        ensure!(
            !BUILT_IN.contains(&name),
            "method '{name}' is built into FreeArc itself; unrepack can't decode it yet"
        );
        bail!(
            "no decoder for method '{name}': neither cls-{name}.dll nor an arc.ini entry in {}",
            dir.display()
        )
    }

    /// Everything a block needs, checked before anything is touched.
    pub fn check(&self, block: &Block) -> Result<()> {
        for s in stages(&block.method) {
            if let Stage::Method { name, params } = s {
                self.resolve(&name, &params)?;
            }
        }
        Ok(())
    }
}

/// Reads one block's packed bytes in order, freeing them behind itself when
/// `punch` is on.
struct BlockReader {
    file: File,
    pos: u64,
    end: u64,
    punch: bool,
    punched: u64,
    /// Packed bytes handed to the decoders, over the whole run.
    consumed: Arc<AtomicU64>,
}

impl Read for BlockReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.end - self.pos;
        if left == 0 {
            return Ok(0);
        }
        let want = buf.len().min(usize::try_from(left).unwrap_or(usize::MAX));
        let n = self.file.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "data file ends inside a block",
            ));
        }
        self.pos += n as u64;
        self.consumed.fetch_add(n as u64, Ordering::Relaxed);
        // Bytes handed on are the decoder's now: it reads forward only.
        if self.punch && (self.pos - self.punched >= PUNCH_STEP || self.pos == self.end) {
            win::punch(&self.file, self.punched, self.pos)?;
            self.punched = self.pos / win::UNIT * win::UNIT;
        }
        Ok(n)
    }
}

/// With `UNREPACK_DUMP=<folder>`, a copy of each stage's output goes to
/// `<folder>/stage-<k>-<method>.bin`, for debugging a decode offline.
fn dump(out: Box<dyn Read + Send>, k: usize, name: &str) -> Result<Box<dyn Read + Send>> {
    struct Tee(Box<dyn Read + Send>, File);
    impl Read for Tee {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.0.read(buf)?;
            self.1.write_all(&buf[..n])?;
            Ok(n)
        }
    }
    let Some(dir) = std::env::var_os("UNREPACK_DUMP") else {
        return Ok(out);
    };
    let p = Path::new(&dir).join(format!("stage-{k}-{name}.bin"));
    fs::create_dir_all(&dir)?;
    let f = File::create(&p).with_context(|| format!("creating {}", p.display()))?;
    Ok(Box::new(Tee(out, f)))
}

/// A running decode chain: the reader of its final output, and what to wait
/// for once it is drained.
struct Chain {
    out: Box<dyn Read + Send>,
    /// Each CLS or stdio decoder's watcher: it waits for the process and,
    /// if it fails, stops every decoder so nothing waits on a dead stage.
    children: Vec<(String, JoinHandle<io::Result<ExitStatus>>)>,
    pumps: Vec<JoinHandle<io::Result<u64>>>,
    /// The threads passing decoders' messages on.
    talk: Vec<JoinHandle<()>>,
}

impl Chain {
    fn start(src: BlockReader, method: &str, dec: &Decoders) -> Result<Chain> {
        let mut out: Box<dyn Read + Send> = Box::new(src);
        let mut children = Vec::new();
        let mut pumps = Vec::new();
        let mut talk = Vec::new();
        for (k, s) in stages(method).into_iter().enumerate() {
            let Stage::Method { name, params } = s else {
                continue;
            };
            let mut cmd = match dec.resolve(&name, &params)? {
                Decode::Cls(dll) => {
                    let mut c = Command::new(&dec.host);
                    c.arg(dll).arg(&params);
                    c
                }
                Decode::Ext(e) if e.files.is_none() => {
                    let mut c = Command::new(&e.exe);
                    c.args(&e.args);
                    c
                }
                // Tools that work on files, and 4x4, run on a thread that
                // feeds a pipe; each gets its own folder for its fixed names.
                d => {
                    let dir = dec.work.join(format!("stage-{k}-{name}"));
                    fs::create_dir_all(&dir)
                        .with_context(|| format!("creating {}", dir.display()))?;
                    let (r, mut w) = io::pipe()?;
                    let mut input = out;
                    out = dump(Box::new(r), k, &name)?;
                    pumps.push(std::thread::spawn(move || {
                        let res = match &d {
                            Decode::FourByFour(e) => e.four_by_four(&mut input, &dir, &mut w),
                            Decode::Ext(e) => e.run_once(&mut input, &dir, &mut w),
                            Decode::Delta => crate::builtin::delta(&mut input, &mut w),
                            Decode::Dispack(block) => {
                                crate::builtin::dispack(&mut input, &mut w, *block)
                            }
                            Decode::Cls(_) => unreachable!(),
                        };
                        if let Err(e) = &res {
                            crate::log::line(&format!("decoder '{name}': {e}"), true);
                            win::stop_decoders();
                        }
                        let _ = fs::remove_dir_all(&dir);
                        res
                    }));
                    continue;
                }
            };
            let mut child = cmd
                .current_dir(&dec.work)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .with_context(|| format!("starting the decoder for '{name}'"))?;
            win::adopt(&child);
            talk.push(forward(child.stderr.take().unwrap()));
            let mut stdin = child.stdin.take().unwrap();
            let stdout = child.stdout.take().unwrap();
            let mut input = out;
            pumps.push(std::thread::spawn(move || {
                let n = io::copy(&mut input, &mut stdin);
                drop(stdin);
                n
            }));
            out = dump(Box::new(stdout), k, &name)?;
            let who = name.clone();
            children.push((
                name,
                std::thread::spawn(move || {
                    let st = child.wait()?;
                    if !st.success() {
                        crate::log::line(
                            &format!("decoder '{who}' failed ({st}); stopping the others"),
                            true,
                        );
                        win::stop_decoders();
                    }
                    Ok(st)
                }),
            ));
        }
        Ok(Chain {
            out,
            children,
            pumps,
            talk,
        })
    }

    /// After the last entry: nothing more may come out, and every stage must
    /// have succeeded.
    fn finish(mut self) -> Result<()> {
        let mut extra = [0u8; 1];
        let more = self
            .out
            .read(&mut extra)
            .context("reading the decoder's output")?;
        drop(self.out);
        for p in self.pumps {
            p.join()
                .expect("pump thread panicked")
                .context("feeding a decoder")?;
        }
        for (name, w) in self.children {
            let st = w.join().expect("watcher thread panicked")?;
            ensure!(st.success(), "decoder '{name}' failed ({st})");
        }
        settle(&self.talk);
        ensure!(
            more == 0,
            "the decoders produced more data than the index lists"
        );
        Ok(())
    }

    fn kill(self) {
        drop(self.out);
        win::stop_decoders();
        let watchers: Vec<_> = self.children.into_iter().map(|(_, w)| w).collect();
        settle(&watchers);
        settle(&self.pumps);
        settle(&self.talk);
    }
}

pub struct Progress {
    pub total: u64,
    pub done: u64,
    /// Packed bytes the decoders have taken, shared with the readers.
    pub consumed: Arc<AtomicU64>,
    /// The most the output ever ran ahead of the packed bytes consumed:
    /// with freeing on, that is the space the run needs beyond the start.
    pub max_lead: u64,
    last: Instant,
    logged: Instant,
    start: Instant,
}

impl Progress {
    pub fn new(total: u64) -> Self {
        Progress {
            total,
            done: 0,
            consumed: Arc::new(AtomicU64::new(0)),
            max_lead: 0,
            last: Instant::now(),
            logged: Instant::now(),
            start: Instant::now(),
        }
    }
    fn add(&mut self, n: u64, what: &str) {
        self.done += n;
        let lead = self
            .done
            .saturating_sub(self.consumed.load(Ordering::Relaxed));
        self.max_lead = self.max_lead.max(lead);
        if self.last.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last = Instant::now();
        let secs = self.start.elapsed().as_secs_f64().max(0.001);
        let speed = self.done as f64 / secs;
        let left = self.total.saturating_sub(self.done) as f64 / speed.max(1.0);
        let text = format!(
            "{what} {:>3.0}%  {} of {}  {}/s  {} left",
            100.0 * self.done as f64 / self.total.max(1) as f64,
            human(self.done),
            human(self.total),
            human(speed as u64),
            clock(left as u64)
        );
        let console = crate::log::console();
        if console {
            crate::log::status(&text);
        }
        if self.logged.elapsed() >= Duration::from_secs(5) {
            self.logged = Instant::now();
            if console {
                crate::log::to_file(&text);
            } else {
                note!("{text}");
            }
        }
    }
}

/// `1h 02m`, `4m 05s`, `12s`.
pub fn clock(secs: u64) -> String {
    match secs {
        s if s >= 3600 => format!("{}h {:02}m", s / 3600, s % 3600 / 60),
        s if s >= 60 => format!("{}m {:02}s", s / 60, s % 60),
        s => format!("{s}s"),
    }
}

pub fn human(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut u = 0;
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

/// Writes all of `buf`, waiting while the disk is full instead of failing:
/// once bytes are freed there is no going back, so running out of space must
/// pause the run, not end it.
fn write_patiently(f: &mut File, mut buf: &[u8], path: &Path) -> io::Result<()> {
    let mut warned = false;
    while !buf.is_empty() {
        match f.write(buf) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => buf = &buf[n..],
            Err(e) if win::is_disk_full(&e) => {
                if !warned {
                    note!(
                        "the disk is full while writing {}. Free some space; the run waits and \
                         continues by itself.",
                        path.display()
                    );
                    warned = true;
                }
                std::thread::sleep(Duration::from_secs(5));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Decodes one block. With `out` set, writes its entries there; without,
/// only checks every CRC. `punch` frees the packed bytes as they are read.
pub fn block(
    data: &Path,
    file: &File,
    block: &Block,
    dec: &Decoders,
    out: Option<&Path>,
    punch: bool,
    progress: &mut Progress,
) -> Result<()> {
    if let Some(out) = out {
        for e in block.entries.iter().filter(|e| e.is_dir) {
            fs::create_dir_all(out.join(&e.path))
                .with_context(|| format!("creating {}", out.join(&e.path).display()))?;
        }
    }
    if block.entries.iter().all(|e| e.is_dir || e.size == 0) && block.packed == 0 {
        if let Some(out) = out {
            for e in block.entries.iter().filter(|e| !e.is_dir) {
                create_file(&out.join(&e.path))?;
            }
        }
        return Ok(());
    }
    let mut f = file.try_clone()?;
    use std::io::Seek;
    f.seek(io::SeekFrom::Start(block.start))?;
    let reader = BlockReader {
        file: f,
        pos: block.start,
        end: block.start + block.packed,
        punch,
        punched: block.start,
        consumed: progress.consumed.clone(),
    };
    let mut chain = Chain::start(reader, &block.method, dec)?;
    let result = entries(&mut chain.out, block, out, progress, data);
    match result {
        Ok(()) => chain.finish(),
        Err(e) => {
            chain.kill();
            Err(e)
        }
    }
}

fn create_file(path: &Path) -> Result<File> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("creating {} (it must not exist yet)", path.display()))
}

fn entries(
    src: &mut dyn Read,
    block: &Block,
    out: Option<&Path>,
    progress: &mut Progress,
    data: &Path,
) -> Result<()> {
    let mut buf = vec![0u8; 1 << 20];
    let what = if out.is_some() {
        "extracting"
    } else {
        "verifying"
    };
    for e in block.entries.iter().filter(|e| !e.is_dir) {
        let target = out.map(|o| o.join(&e.path));
        let part = target.as_ref().map(|t| {
            let mut s = t.as_os_str().to_owned();
            s.push(PART);
            PathBuf::from(s)
        });
        let mut file = part.as_deref().map(create_file).transpose()?;
        let mut crc = block.crc.hasher();
        let mut left = e.size;
        while left > 0 {
            let want = buf.len().min(usize::try_from(left).unwrap_or(usize::MAX));
            let n = src
                .read(&mut buf[..want])
                .with_context(|| format!("decoding {}", data.display()))?;
            if n == 0 {
                bail!(
                    "the decoders stopped {} bytes short inside {}",
                    left,
                    e.path.display()
                );
            }
            crc.update(&buf[..n]);
            if let (Some(f), Some(p)) = (file.as_mut(), part.as_deref()) {
                write_patiently(f, &buf[..n], p)
                    .with_context(|| format!("writing {}", p.display()))?;
            }
            left -= n as u64;
            progress.add(n as u64, what);
        }
        let got = crc.finalize();
        ensure!(
            got == e.crc,
            "{} decoded with CRC {got:08x}, the index says {:08x}",
            e.path.display(),
            e.crc
        );
        if let (Some(f), Some(p), Some(t)) = (file, part, target) {
            f.sync_all()
                .with_context(|| format!("flushing {}", p.display()))?;
            drop(f);
            fs::rename(&p, &t).with_context(|| format!("renaming {}", p.display()))?;
        }
    }
    Ok(())
}

/// The marker that says a data file is partly consumed and can't be used
/// again by this version.
pub fn marker(data: &Path) -> PathBuf {
    let mut s = data.as_os_str().to_owned();
    s.push(".unrepack-consumed");
    PathBuf::from(s)
}

pub struct Job<'a> {
    pub data: &'a Path,
    pub archive: &'a Archive,
}

/// Extracts one data file. With `consume`, its bytes are freed as they are
/// read and the file is deleted at the end.
pub fn run(
    job: &Job,
    dec: &Decoders,
    out: &Path,
    consume: bool,
    progress: &mut Progress,
) -> Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .write(consume)
        .open(job.data)
        .with_context(|| format!("opening {}", job.data.display()))?;
    if consume {
        let m = marker(job.data);
        fs::write(
            &m,
            "unrepack v0.1 started freeing this data file; it can't be extracted again.\n",
        )
        .with_context(|| format!("writing {}", m.display()))?;
        win::set_sparse(&file).with_context(|| format!("making {} sparse", job.data.display()))?;
    }
    for b in &job.archive.blocks {
        block(job.data, &file, b, dec, Some(out), consume, progress)
            .with_context(|| format!("block '{}' of {}", b.method, job.data.display()))?;
    }
    if consume {
        drop(file);
        fs::remove_file(job.data).with_context(|| format!("deleting {}", job.data.display()))?;
        let _ = fs::remove_file(marker(job.data));
    }
    Ok(())
}

/// Decodes everything and checks every CRC, writing and freeing nothing.
pub fn verify(job: &Job, dec: &Decoders, progress: &mut Progress) -> Result<()> {
    let file = File::open(job.data).with_context(|| format!("opening {}", job.data.display()))?;
    for b in &job.archive.blocks {
        block(job.data, &file, b, dec, None, false, progress)
            .with_context(|| format!("verifying block '{}' of {}", b.method, job.data.display()))?;
    }
    Ok(())
}

/// Samples the size of a folder in the background, keeping the largest seen:
/// the decoders' temp files, which no index lists.
pub struct FolderPeak {
    stop: Arc<std::sync::atomic::AtomicBool>,
    peak: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}

impl FolderPeak {
    pub fn start(dir: PathBuf) -> Self {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let peak = Arc::new(AtomicU64::new(0));
        let (s, p) = (stop.clone(), peak.clone());
        let thread = std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                p.fetch_max(folder_size(&dir), Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(500));
            }
        });
        FolderPeak {
            stop,
            peak,
            thread: Some(thread),
        }
    }

    pub fn finish(mut self) -> u64 {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.peak.load(Ordering::Relaxed)
    }
}

fn folder_size(dir: &Path) -> u64 {
    let Ok(rd) = fs::read_dir(dir) else { return 0 };
    rd.filter_map(|e| e.ok())
        .map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => folder_size(&e.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stages_decode_in_reverse() {
        assert_eq!(
            stages("srep+lolz"),
            vec![
                Stage::Method {
                    name: "lolz".into(),
                    params: "".into()
                },
                Stage::Method {
                    name: "srep".into(),
                    params: "".into()
                }
            ]
        );
        assert_eq!(
            stages("xtool:c64mb+srep:m3f"),
            vec![
                Stage::Method {
                    name: "srep".into(),
                    params: "m3f".into()
                },
                Stage::Method {
                    name: "xtool".into(),
                    params: "c64mb".into()
                }
            ]
        );
        assert_eq!(stages("storing"), vec![Stage::Storing]);
    }

    /// FitGirl's arc.ini entries, and the methods that pick them.
    #[test]
    fn arc_ini_methods() {
        let dir = tempfile::tempdir().unwrap();
        for f in ["rzw.exe", "xtool.exe", "cls-magic2.dll"] {
            fs::write(dir.path().join(f), b"").unwrap();
        }
        let ini = "[External compressor:rzwb]\nheader = 0\nsolid = 0\nunpackcmd  = rzw d f2 f1 1023 1023\ndatafile   = f1\npackedfile = f2\n\n\
                   [External compressor:xt2p,xt2png,xt2]\nheader    = 0\nunpackcmd = xtool.exe decode -t100p - - <stdin> <stdout>\n\n\
                   [External compressor:bcm]\nunpackcmd = bcm -d $$arcpackedfile$$.tmp $$arcdatafile$$.tmp\n";
        fs::write(dir.path().join("arc.ini"), ini).unwrap();
        let e = External::from_ini(ini, dir.path(), "rzwb", "").unwrap().unwrap();
        assert_eq!(e.exe, dir.path().join("rzw.exe"));
        assert_eq!(e.args, ["d", "f2", "f1", "1023", "1023"]);
        assert_eq!(e.files, Some(("f2".into(), "f1".into())));
        let e = External::from_ini(ini, dir.path(), "xt2png", "").unwrap().unwrap();
        assert_eq!(e.args, ["decode", "-t100p", "-", "-"]);
        assert_eq!(e.files, None);
        assert!(External::from_ini(ini, dir.path(), "bcm", "").is_err(), "header=1");
        assert!(External::from_ini(ini, dir.path(), "lolz", "").unwrap().is_none());

        let dec = Decoders {
            dir: Some(dir.path().into()),
            host: dir.path().join("rzw.exe"),
            work: dir.path().into(),
        };
        assert!(matches!(dec.resolve("magic2", ""), Ok(Decode::Cls(_))));
        assert!(matches!(dec.resolve("4x4", "b128mb:rzwb"), Ok(Decode::FourByFour(_))));
        assert!(matches!(dec.resolve("delta", ""), Ok(Decode::Delta)));
        assert!(matches!(dec.resolve("dispack070", ""), Ok(Decode::Dispack(0x80_0000))));
        assert!(matches!(dec.resolve("dispack", "b4mb:x"), Ok(Decode::Dispack(0x40_0000))));
        assert!(dec.resolve("dispack070", "7").is_err(), "bare number");
        let err = dec.resolve("exe2", "").unwrap_err().to_string();
        assert!(err.contains("built into FreeArc"), "{err}");
    }

    /// 4x4 framing through a file-mode tool that copies f2 to f1.
    #[test]
    fn four_by_four_frames() {
        let dir = tempfile::tempdir().unwrap();
        let copy = External {
            name: "copy".into(),
            exe: "C:\\Windows\\System32\\cmd.exe".into(),
            args: ["/c", "copy", "/b", "f2", "f1"].map(String::from).to_vec(),
            files: Some(("f2".into(), "f1".into())),
        };
        let frame = |b: &[u8], size: u32| {
            let mut v = size.to_le_bytes().to_vec();
            v.extend((b.len() as u32).to_le_bytes());
            v.extend(b);
            v
        };
        let mut s = vec![0u8; 4];
        s.extend(frame(b"hello ", 6));
        s.extend(frame(b"world", 5));
        let mut out = Vec::new();
        let n = copy.four_by_four(&mut &s[..], dir.path(), &mut out).unwrap();
        assert_eq!((n, &out[..]), (11, &b"hello world"[..]));

        let mut bad = vec![0u8; 4];
        bad.extend(frame(b"hello", 9));
        assert!(copy.four_by_four(&mut &bad[..], dir.path(), &mut Vec::new()).is_err());
        let cut = &s[..s.len() - 2];
        assert!(copy.four_by_four(&mut &cut[..], dir.path(), &mut Vec::new()).is_err());
    }
}
