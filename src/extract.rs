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
    "exe", "exe2", "lzma2", "ppmd", "lzp", "grzip", "rep", "dict", "mm", "tta", "lz4", "bcj",
    "bcj2",
];

/// What runs one stage.
#[derive(Debug, Clone)]
enum Decode {
    /// A FreeArc CLS plug-in, `cls-<name>.dll`, run through the 32-bit host.
    Cls(PathBuf),
    /// An `[External compressor]` from the repack's arc.ini.
    Ext(External),
    /// FreeArc's `4x4` (C_4x4.cpp): a 4-byte stream header (0), then frames
    /// of unpacked size, packed size (u32 LE each) and the packed bytes, each
    /// frame decoded on its own by the inner method; an unpacked size of
    /// 0xFFFFFFFF marks a frame stored as is.
    FourByFour(Box<Decode>),
    /// FreeArc built-ins ported in `builtin.rs`.
    Delta,
    /// `dispack070`, with its block size.
    Dispack(usize),
    /// `lzma`: raw LZMA1 up to its end marker, with dictionary, lc, lp, pb
    /// taken from the method string.
    Lzma(Lzma),
    /// `tor` (Tornado), ported in `tornado.rs`; the stream has its own header.
    Tornado,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Lzma {
    dict: u32,
    lc: u32,
    lp: u32,
    pb: u32,
}

/// FreeArc's `parseMem`: digits, then b, k, m, g or ^ (2 to the power), of
/// which only the first letter counts; no letter means `spec`.
fn freearc_mem(p: &str, spec: u8) -> Option<u64> {
    let p = p.strip_prefix('=').unwrap_or(p);
    let d = p.bytes().take_while(u8::is_ascii_digit).count();
    let n: u64 = p.get(..d).filter(|s| !s.is_empty())?.parse().ok()?;
    match p.as_bytes().get(d).copied().unwrap_or(spec) {
        b'b' => Some(n),
        b'k' => n.checked_mul(1 << 10),
        b'm' => n.checked_mul(1 << 20),
        b'g' => n.checked_mul(1 << 30),
        b'^' => (n < 64).then(|| 1 << n),
        _ => None,
    }
}

/// `parse_LZMA` (C_LZMA.cpp), keeping only what decoding needs. Its order
/// matters: named options, then a match finder, a bare number (fast bytes),
/// and last a memory size (the dictionary). `*` marks an option FreeArc
/// skips when it can't read it.
fn lzma_params(params: &str) -> Result<Lzma> {
    let mut m = Lzma {
        dict: 64 << 20,
        lc: 3,
        lp: 0,
        pb: 2,
    };
    let int = |s: &str| s.parse::<u32>().ok();
    let mut dict = None;
    for p in params.split(':').filter(|p| !p.is_empty()) {
        let (optional, p) = p.strip_prefix('*').map_or((false, p), |r| (true, r));
        let ok = if let Some(r) = p.strip_prefix('d') {
            freearc_mem(r, b'^').map(|n| dict = Some(n)).is_some()
        } else if p.starts_with('h') && freearc_mem(&p[1..], b'^').is_some() {
            true
        } else if let Some(r) = p.strip_prefix('a') {
            int(r).is_some()
        } else if let Some(r) = p.strip_prefix("fb").or_else(|| p.strip_prefix("mc")) {
            int(r).is_some()
        } else if let Some(r) = p.strip_prefix("lc") {
            int(r).map(|n| m.lc = n).is_some()
        } else if let Some(r) = p.strip_prefix("lp") {
            int(r).map(|n| m.lp = n).is_some()
        } else if let Some(r) = p.strip_prefix("pb") {
            int(r).map(|n| m.pb = n).is_some()
        } else {
            p.starts_with("mf")
                || ["fastest", "fast", "normal", "max", "ultra"].contains(&p)
                || ["bt2", "bt3", "bt4", "hc4", "ht4"]
                    .iter()
                    .any(|f| p.eq_ignore_ascii_case(f))
                || int(p).is_some()
                || freearc_mem(p, b'^').map(|n| dict = Some(n)).is_some()
        };
        ensure!(
            ok || optional,
            "method 'lzma': parameter '{p}' not understood"
        );
    }
    if let Some(d) = dict {
        m.dict = u32::try_from(d).context("method 'lzma': dictionary over 4 GB")?;
    }
    ensure!(
        m.lc <= 8 && m.lp <= 4 && m.pb <= 4,
        "method 'lzma': lc{} lp{} pb{} out of range",
        m.lc,
        m.lp,
        m.pb
    );
    Ok(m)
}

/// Decodes FreeArc's `lzma` stream, then reads what's left of `input` so the
/// stage feeding it isn't cut off.
fn lzma(input: &mut dyn Read, out: &mut dyn Write, m: Lzma) -> io::Result<u64> {
    let n = {
        let mut r = lzma_rust2::LzmaReader::new(
            io::BufReader::with_capacity(1 << 16, &mut *input),
            u64::MAX,
            m.lc,
            m.lp,
            m.pb,
            m.dict,
            None,
        )?;
        io::copy(&mut r, out)?
    };
    io::copy(input, &mut io::sink())?;
    Ok(n)
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

/// An arc.ini section: its method names and its `key=value` lines.
type IniSection = (Vec<String>, Vec<(String, String)>);

/// The `[External compressor:a,b]` sections of an arc.ini, keys lowercased.
fn arc_ini(text: &str) -> Vec<IniSection> {
    let mut out: Vec<IniSection> = Vec::new();
    let mut in_ext = false;
    for line in text.lines().map(str::trim) {
        if let Some(h) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_ext = false;
            if let Some((kind, names)) = h.split_once(':')
                && kind.trim().eq_ignore_ascii_case("External compressor")
            {
                in_ext = true;
                out.push((
                    names.split(',').map(|n| n.trim().to_string()).collect(),
                    Vec::new(),
                ));
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
    /// taken only from `dir`. `pending`: programs not there yet that an
    /// earlier step puts in `dir` (a FitGirl repack unpacks its xtool).
    fn from_ini(
        text: &str,
        dir: &Path,
        name: &str,
        params: &str,
        pending: &[PathBuf],
    ) -> Result<Option<External>> {
        let Some((_, keys)) = arc_ini(text)
            .into_iter()
            .find(|(n, _)| n.iter().any(|n| n == name))
        else {
            return Ok(None);
        };
        let get = |k: &str| keys.iter().find(|(x, _)| x == k).map(|(_, v)| v.as_str());
        let cmd =
            get("unpackcmd").with_context(|| format!("arc.ini has no unpackcmd for '{name}'"))?;
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
        ensure!(
            !toks.is_empty(),
            "arc.ini method '{name}' has an empty unpackcmd"
        );
        let prog = toks.remove(0);
        // `xt4\xtool.exe` is fine; anything that could leave the folder isn't.
        ensure!(
            !prog.contains(['/', ':'])
                && Path::new(&prog)
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_))),
            "arc.ini method '{name}' names a program outside the decoders folder"
        );
        let exe = [dir.join(&prog), dir.join(format!("{prog}.exe"))]
            .into_iter()
            .find(|p| {
                p.is_file()
                    || pending
                        .iter()
                        .any(|q| q.as_os_str().eq_ignore_ascii_case(p.as_os_str()))
            })
            .with_context(|| {
                format!(
                    "arc.ini method '{name}' needs {prog}, missing from {}",
                    dir.display()
                )
            })?;
        let files = (!stdio).then(|| {
            (
                sub(get("packedfile").unwrap_or("$$arcpackedfile$$.tmp")),
                sub(get("datafile").unwrap_or("$$arcdatafile$$.tmp")),
            )
        });
        if let Some((p, d)) = &files {
            ensure!(
                [p, d]
                    .iter()
                    .all(|f| !f.contains(['/', '\\', ':']) && !f.is_empty() && p != d),
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
    fn run_once(
        &self,
        input: &mut (dyn Read + Send),
        dir: &Path,
        out: &mut dyn Write,
    ) -> io::Result<u64> {
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
        let said: Vec<&str> = said
            .split(['\r', '\n'])
            .filter(|l| !l.trim().is_empty())
            .collect();
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
        let n = io::copy(
            &mut File::open(&d).map_err(|e| fail(format!("wrote no {data}: {e}")))?,
            out,
        )?;
        fs::remove_file(&d)?;
        Ok(n)
    }
}

impl Decode {
    /// Decodes everything `input` gives into `out` on this thread, in folder
    /// `dir`. CLS plug-ins run as processes in [`Chain::start`] instead.
    fn run(
        &self,
        input: &mut (dyn Read + Send),
        dir: &Path,
        out: &mut dyn Write,
    ) -> io::Result<u64> {
        match self {
            Decode::FourByFour(inner) => four_by_four(inner, input, dir, out),
            Decode::Ext(e) => e.run_once(input, dir, out),
            Decode::Delta => crate::builtin::delta(input, out),
            Decode::Dispack(block) => crate::builtin::dispack(input, out, *block),
            Decode::Lzma(m) => lzma(input, out, *m),
            Decode::Tornado => crate::tornado::decode(input, out),
            Decode::Cls(_) => unreachable!("resolve keeps CLS out of 4x4"),
        }
    }
}

/// `4x4` around `inner`. ponytail: one frame at a time; FreeArc decodes
/// frames on several threads, add that if this is the slow stage.
fn four_by_four(
    inner: &Decode,
    input: &mut (dyn Read + Send),
    dir: &Path,
    out: &mut dyn Write,
) -> io::Result<u64> {
    let bad = |m: String| io::Error::new(io::ErrorKind::InvalidData, format!("4x4: {m}"));
    let mut h = [0u8; 4];
    match read_full(input, &mut h)? {
        0 => return Ok(0),
        4 if h == [0; 4] => {}
        _ => return Err(bad(format!("stream header {h:02x?} not understood"))),
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
        let size = u32::from_le_bytes(f[..4].try_into().unwrap());
        let packed = u64::from(u32::from_le_bytes(f[4..].try_into().unwrap()));
        let mut frame = (&mut *input).take(packed);
        let (n, size) = if size == u32::MAX {
            (io::copy(&mut frame, out)?, packed)
        } else {
            (inner.run(&mut frame, dir, out)?, u64::from(size))
        };
        if frame.limit() != 0 {
            return Err(bad("stream ends inside a frame".into()));
        }
        if n != size {
            return Err(bad(format!(
                "frame decoded to {n} bytes, its header says {size}"
            )));
        }
        total += n;
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
    /// The decoders' folder, or what to do to get one.
    pub dir: std::result::Result<PathBuf, String>,
    pub host: PathBuf,
    /// Where the decoders run: srep and lolz put temp files in `.\`.
    pub work: PathBuf,
    /// Files a later step puts in `dir`; see `External::from_ini`.
    pub pending: Vec<PathBuf>,
}

impl Decoders {
    /// As FreeArc's `ParseCompressionMethod` does: the repack's own decoders
    /// first, then the built-ins, which need no decoders folder.
    fn resolve(&self, name: &str, params: &str) -> Result<Decode> {
        if name == "4x4" {
            // 4x4[:b<size>:t<n>:i<n>:r<pct>]:<inner method>[:<its options>];
            // as parse_4x4, an option has a digit first or second.
            let mut parts = params.split(':');
            let inner = parts
                .by_ref()
                .find(|p| !p.bytes().take(2).any(|c| c.is_ascii_digit()))
                .context("4x4 without an inner method")?;
            let rest: Vec<&str> = parts.collect();
            let d = self.resolve(inner, &rest.join(":"))?;
            ensure!(
                !matches!(d, Decode::Cls(_) | Decode::FourByFour(_)),
                "4x4 around '{inner}': not supported"
            );
            return Ok(Decode::FourByFour(Box::new(d)));
        }
        if let Ok(dir) = &self.dir
            && let Some(d) = self.repack_decoder(dir, name, params)?
        {
            return Ok(d);
        }
        match name {
            "lzma" => return Ok(Decode::Lzma(lzma_params(params)?)),
            "tor" => return Ok(Decode::Tornado),
            // Its block size and `x` only matter when compressing.
            "delta" => return Ok(Decode::Delta),
            "dispack" | "dispack070" => {
                let mut block = 8 << 20;
                for p in params.split(':').filter(|p| !p.is_empty() && *p != "x") {
                    block = mem_param(
                        p.strip_prefix('b')
                            .filter(|r| {
                                !r.is_empty() && r.starts_with(|c: char| c.is_ascii_digit())
                            })
                            .unwrap_or(p),
                    )
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
        let dir = self
            .dir
            .as_ref()
            .map_err(|why| anyhow::anyhow!("method '{name}' needs the repack's decoders: {why}"))?;
        bail!(
            "no decoder for method '{name}': neither cls-{name}.dll nor an arc.ini entry in {}",
            dir.display()
        )
    }

    /// A CLS plug-in, which wins over arc.ini as in FreeArc, or an arc.ini
    /// entry.
    fn repack_decoder(&self, dir: &Path, name: &str, params: &str) -> Result<Option<Decode>> {
        let ini = fs::read_to_string(dir.join("arc.ini")).unwrap_or_default();
        let p = dir.join(format!("cls-{name}.dll"));
        if p.is_file() {
            ensure!(
                self.host.is_file(),
                "the CLS host {} is missing",
                self.host.display()
            );
            return Ok(Some(Decode::Cls(p)));
        }
        Ok(External::from_ini(&ini, dir, name, params, &self.pending)?.map(Decode::Ext))
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
                        let res = d.run(&mut input, &dir, &mut w);
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
        // unrepack-gui sets this and reads `@progress <phase> <done> <total>`.
        if std::env::var_os("UNREPACK_GUI").is_some() {
            eprintln!("@progress {what} {} {}", self.done, self.total);
        }
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

    fn builtin(method: &str) -> Decode {
        let dec = Decoders {
            dir: Err("no decoders in this test".into()),
            host: PathBuf::new(),
            work: PathBuf::new(),
            pending: Vec::new(),
        };
        let (name, params) = method.split_once(':').unwrap_or((method, ""));
        dec.resolve(name, params).unwrap()
    }

    fn lzma_of(method: &str) -> Lzma {
        match builtin(method) {
            Decode::Lzma(m) => m,
            d => panic!("{method} resolved to {d:?}"),
        }
    }

    #[test]
    fn lzma_method_strings() {
        // As FreeArc's ShowCompressionMethod prints them.
        let m = |dict, lc, lp, pb| Lzma { dict, lc, lp, pb };
        assert_eq!(
            lzma_of("lzma:96mb:normal:bt4:128:mc256:lc8"),
            m(96 << 20, 8, 0, 2)
        );
        assert_eq!(
            lzma_of("lzma:1gb:max:bt4:273:pb0:lc1:lp2"),
            m(1 << 30, 1, 2, 0)
        );
        assert_eq!(lzma_of("lzma:64mb:normal:32"), m(64 << 20, 3, 0, 2));
        assert_eq!(lzma_of("lzma:1536kb:h4mb:a2:32"), m(1536 << 10, 3, 0, 2));
        // Hand-written ones: a bare number is fast bytes, d is 2^n by default.
        assert_eq!(lzma_of("lzma"), m(64 << 20, 3, 0, 2));
        assert_eq!(lzma_of("lzma:d24:hc4:64"), m(16 << 20, 3, 0, 2));
        assert_eq!(lzma_of("lzma:ultra:*zz"), m(64 << 20, 3, 0, 2));
        assert!(matches!(builtin("tor:7:c3"), Decode::Tornado));
        let dec = Decoders {
            dir: Err("none".into()),
            host: PathBuf::new(),
            work: PathBuf::new(),
            pending: Vec::new(),
        };
        for bad in ["zz", "a", "lc9", "pb5", "d8gb"] {
            assert!(dec.resolve("lzma", bad).is_err(), "{bad}");
        }
    }

    /// liblzma's raw LZMA1 (testdata/README.md), decoded through `resolve`,
    /// with junk after the end marker that must be read and dropped.
    #[test]
    fn lzma_decodes() {
        for (file, method) in [
            (
                &include_bytes!("../testdata/lzma-default.bin")[..],
                "lzma:1mb:normal:bt4:32",
            ),
            (
                include_bytes!("../testdata/lzma-lc1lp2pb0.bin"),
                "lzma:1mb:normal:32:pb0:lc1:lp2",
            ),
        ] {
            let mut src = [file, &[9u8; 5000][..]].concat();
            let mut input = &src[..];
            let mut out = Vec::new();
            let n = lzma(&mut input, &mut out, lzma_of(method)).unwrap();
            assert!(input.is_empty());
            assert_eq!((n, out.len()), (123616, 123616));
            assert_eq!(crc32fast::hash(&out), 0x65d817f5);
            // Cut short: an error, not a short success.
            src.truncate(file.len() - 10);
            assert!(lzma(&mut &src[..], &mut Vec::new(), lzma_of(method)).is_err());
        }
    }

    /// A repack's own decoder for a method FreeArc also has built in wins,
    /// as in FreeArc; without one, the built-in runs.
    #[test]
    fn repack_decoders_win_over_builtins() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("mylzma.exe"), b"").unwrap();
        let ini = "[External compressor:lzma]\nheader = 0\nunpackcmd = mylzma d <stdin> <stdout>\n";
        fs::write(dir.path().join("arc.ini"), ini).unwrap();
        let dec = Decoders {
            dir: Ok(dir.path().into()),
            host: PathBuf::new(),
            work: PathBuf::new(),
            pending: Vec::new(),
        };
        assert!(matches!(
            dec.resolve("lzma", "96mb").unwrap(),
            Decode::Ext(_)
        ));
        assert!(matches!(dec.resolve("tor", "5").unwrap(), Decode::Tornado));
        assert!(matches!(dec.resolve("delta", "").unwrap(), Decode::Delta));
        let err = dec.resolve("ppmd", "").unwrap_err().to_string();
        assert!(err.contains("built into FreeArc"), "{err}");
        assert!(dec.resolve("nosuch", "").is_err());
    }

    /// Both built-ins through the real stage chain: a data file with an
    /// `lzma` block (two files) and a `tor` block (one), each cut by CRC.
    #[test]
    fn lzma_and_tor_blocks_extract() {
        let lz = include_bytes!("../testdata/lzma-default.bin");
        let tor = include_bytes!("../testdata/tor-c4opt.bin");
        let mut want = Vec::new();
        lzma(&mut &lz[..], &mut want, lzma_of("lzma:1mb")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data1.bin");
        fs::write(&data, [&[0u8; 100][..], lz, tor].concat()).unwrap();
        let entry = |name: &str, part: &[u8]| crate::freearc::Entry {
            path: name.into(),
            size: part.len() as u64,
            crc: crc32fast::hash(part),
            is_dir: false,
        };
        let blocks = [
            Block {
                method: "lzma:1mb:normal:bt4:32".into(),
                start: 100,
                packed: lz.len() as u64,
                entries: vec![entry("a", &want[..50000]), entry("b", &want[50000..])],
                crc: crate::freearc::Crc::Ieee,
            },
            Block {
                method: "tor:5:c4".into(),
                start: 100 + lz.len() as u64,
                packed: tor.len() as u64,
                entries: vec![entry("c", &want)],
                crc: crate::freearc::Crc::Ieee,
            },
        ];
        let dec = Decoders {
            dir: Err("none".into()),
            host: PathBuf::new(),
            work: dir.path().join("work"),
            pending: Vec::new(),
        };
        let out = dir.path().join("out");
        let file = File::open(&data).unwrap();
        let mut p = Progress::new(3 * want.len() as u64);
        for b in &blocks {
            block(&data, &file, b, &dec, Some(&out), false, &mut p).unwrap();
        }
        assert!(fs::read(out.join("a")).unwrap() == want[..50000]);
        assert!(fs::read(out.join("b")).unwrap() == want[50000..]);
        assert!(fs::read(out.join("c")).unwrap() == want);
    }

    /// `4x4` around the built-ins, on streams cut from archives FreeArc 0.67's
    /// Arc.exe made: 6 frames each, 2 of them stored.
    #[test]
    fn four_by_four_builtins() {
        let dec = Decoders {
            dir: Err("none".into()),
            host: PathBuf::new(),
            work: PathBuf::new(),
            pending: Vec::new(),
        };
        let dir = tempfile::tempdir().unwrap();
        for (params, s) in [
            (
                "b32kb:tor:3:188kb",
                &include_bytes!("../testdata/4x4-tor.bin")[..],
            ),
            (
                "b32kb:lzma:188kb:fast:32",
                include_bytes!("../testdata/4x4-lzma.bin"),
            ),
        ] {
            let d = dec.resolve("4x4", params).unwrap();
            let mut out = Vec::new();
            d.run(&mut &s[..], dir.path(), &mut out).unwrap();
            assert_eq!(
                (out.len(), crc32fast::hash(&out)),
                (189152, 0x9815b9f8),
                "{params}"
            );
        }
        let err = dec.resolve("4x4", "b7mb:ppmd:8").unwrap_err().to_string();
        assert!(err.contains("built into FreeArc"), "{err}");
    }

    /// FreeArc often writes lc8, which liblzma can't make.
    #[test]
    fn lzma_lc8() {
        let data = include_bytes!("../testdata/gen.py").repeat(200);
        let opts = lzma_rust2::LzmaOptions {
            lc: 8,
            lp: 0,
            pb: 2,
            dict_size: 4 << 20,
            ..lzma_rust2::LzmaOptions::with_preset(6)
        };
        let mut w = lzma_rust2::LzmaWriter::new_no_header(Vec::new(), &opts, true).unwrap();
        w.write_all(&data).unwrap();
        let packed = w.finish().unwrap();
        let mut out = Vec::new();
        lzma(
            &mut &packed[..],
            &mut out,
            lzma_of("lzma:4mb:normal:bt4:32:lc8"),
        )
        .unwrap();
        assert!(out == data);
    }

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
        let e = External::from_ini(ini, dir.path(), "rzwb", "", &[])
            .unwrap()
            .unwrap();
        assert_eq!(e.exe, dir.path().join("rzw.exe"));
        assert_eq!(e.args, ["d", "f2", "f1", "1023", "1023"]);
        assert_eq!(e.files, Some(("f2".into(), "f1".into())));
        let e = External::from_ini(ini, dir.path(), "xt2png", "", &[])
            .unwrap()
            .unwrap();
        assert_eq!(e.args, ["decode", "-t100p", "-", "-"]);
        assert_eq!(e.files, None);
        assert!(
            External::from_ini(ini, dir.path(), "bcm", "", &[]).is_err(),
            "header=1"
        );
        assert!(
            External::from_ini(ini, dir.path(), "lolz", "", &[])
                .unwrap()
                .is_none()
        );

        let dec = Decoders {
            dir: Ok(dir.path().into()),
            host: dir.path().join("rzw.exe"),
            work: dir.path().into(),
            pending: Vec::new(),
        };
        assert!(matches!(dec.resolve("magic2", ""), Ok(Decode::Cls(_))));
        assert!(matches!(
            dec.resolve("4x4", "b128mb:rzwb"),
            Ok(Decode::FourByFour(_))
        ));
        assert!(matches!(dec.resolve("delta", ""), Ok(Decode::Delta)));
        assert!(matches!(
            dec.resolve("dispack070", ""),
            Ok(Decode::Dispack(0x80_0000))
        ));
        assert!(matches!(
            dec.resolve("dispack", "b4mb:x"),
            Ok(Decode::Dispack(0x40_0000))
        ));
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
        let copy = Decode::Ext(copy);
        let run = |s: &[u8]| {
            let mut out = Vec::new();
            four_by_four(&copy, &mut &s[..], dir.path(), &mut out).map(|n| (n, out))
        };
        let mut s = vec![0u8; 4];
        s.extend(frame(b"hello ", 6));
        s.extend(frame(b"big ", u32::MAX)); // stored, not run through `copy`
        s.extend(frame(b"world", 5));
        assert_eq!(run(&s).unwrap(), (15, b"hello big world".to_vec()));

        let mut bad = vec![0u8; 4];
        bad.extend(frame(b"hello", 9));
        assert!(run(&bad).is_err());
        assert!(run(&s[..s.len() - 2]).is_err());
        assert!(run(&[1, 0, 0, 0]).is_err(), "version 1");
        assert_eq!(run(&[]).unwrap().0, 0);
    }
}
