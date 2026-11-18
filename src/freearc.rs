//! Reads the index of a FreeArc archive: the footer at the very end, the
//! directory blocks it points to, and from those every solid block and the
//! entries it decodes to. Data blocks are not touched here.
//!
//! Layout, as measured on real repack data files (see docs in the README):
//! - Each control block's bytes are followed by a descriptor: `ArC\x01`, type
//!   byte, method (NUL ended), unpacked size, packed size (varints), CRC-32 of
//!   the unpacked bytes, CRC-32 of the descriptor itself.
//! - The file ends with the footer's descriptor (type 8). The footer lists the
//!   control blocks (type, method, distance back from the footer, sizes, CRC).
//! - A directory (type 6) lists its solid blocks (entry count, method,
//!   distance back from the directory, packed size), then folder names and,
//!   per entry, name, folder, size, time, folder flag and CRC-32.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

pub const SIGNATURE: &[u8; 4] = b"ArC\x01";
const FOOTER: u8 = 8;
const DIRECTORY: u8 = 6;
/// Control blocks are small; a larger claim is damage or hostile.
const CONTROL_MAX: u64 = 256 << 20;

#[derive(Debug, Clone)]
pub struct Entry {
    /// Relative, checked to stay inside the output folder.
    pub path: PathBuf,
    pub size: u64,
    pub crc: u32,
    pub is_dir: bool,
}

#[derive(Debug, Clone)]
pub struct Block {
    /// Methods as compressed, left to right, for example `srep+lolz`.
    pub method: String,
    pub start: u64,
    pub packed: u64,
    pub entries: Vec<Entry>,
    /// The checksum the entries' CRCs use.
    pub crc: Crc,
}

impl Block {
    pub fn unpacked(&self) -> u64 {
        self.entries.iter().map(|e| e.size).sum()
    }
}

#[derive(Debug)]
pub struct Archive {
    pub len: u64,
    pub blocks: Vec<Block>,
}

impl Archive {
    pub fn unpacked(&self) -> u64 {
        self.blocks.iter().map(Block::unpacked).sum()
    }
    pub fn entries(&self) -> usize {
        self.blocks.iter().map(|b| b.entries.len()).sum()
    }
}

/// The CRC-32 an archive's checksums use. FitGirl's FreeArc build swaps the
/// polynomial for 0x0895171B (reflected, init and final xor all ones); found
/// by searching every polynomial against her header block, then confirmed on
/// the footer, directory and both descriptors of fg-05.bin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Crc {
    Ieee,
    FitGirl,
}

const FITGIRL: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0x0895_171B
            } else {
                c >> 1
            };
            k += 1;
        }
        t[n] = c;
        n += 1;
    }
    t
};

pub enum Hasher {
    Ieee(crc32fast::Hasher),
    /// ponytail: one table lookup per byte, a few hundred MB/s; slicing-by-8
    /// if it ever becomes slower than the decoders feeding it.
    FitGirl(u32),
}

impl Crc {
    pub fn hasher(self) -> Hasher {
        match self {
            Crc::Ieee => Hasher::Ieee(crc32fast::Hasher::new()),
            Crc::FitGirl => Hasher::FitGirl(!0),
        }
    }
    pub fn hash(self, b: &[u8]) -> u32 {
        let mut h = self.hasher();
        h.update(b);
        h.finalize()
    }
}

impl Hasher {
    pub fn update(&mut self, b: &[u8]) {
        match self {
            Hasher::Ieee(h) => h.update(b),
            Hasher::FitGirl(c) => {
                for &x in b {
                    *c = (*c >> 8) ^ FITGIRL[((*c ^ u32::from(x)) & 0xff) as usize];
                }
            }
        }
    }
    pub fn finalize(self) -> u32 {
        match self {
            Hasher::Ieee(h) => h.finalize(),
            Hasher::FitGirl(c) => !c,
        }
    }
}

/// FreeArc's variable-length integer: the count of trailing one bits in the
/// first byte says how many bytes follow, little-endian, value above them.
pub fn varint(b: &[u8], i: &mut usize) -> Result<u64> {
    let first = *b.get(*i).context("index ends inside a number")?;
    let ones = first.trailing_ones() as usize;
    let (len, shift) = if ones >= 8 {
        (9, 8)
    } else {
        (ones + 1, ones + 1)
    };
    let bytes = b.get(*i..*i + len).context("index ends inside a number")?;
    let mut v = 0u128;
    for (k, &x) in bytes.iter().enumerate() {
        v |= u128::from(x) << (8 * k);
    }
    *i += len;
    u64::try_from(v >> shift).context("number too large")
}

fn cstr<'a>(b: &'a [u8], i: &mut usize) -> Result<&'a [u8]> {
    let rest = b.get(*i..).context("index ends inside a name")?;
    let end = rest
        .iter()
        .position(|&c| c == 0)
        .context("name without end")?;
    *i += end + 1;
    Ok(&rest[..end])
}

fn u32le(b: &[u8], i: &mut usize) -> Result<u32> {
    let x = b.get(*i..*i + 4).context("index ends inside a checksum")?;
    *i += 4;
    Ok(u32::from_le_bytes(x.try_into().unwrap()))
}

/// Decodes a control block. FreeArc writes them with `storing` or with
/// `lzma:<dict>:...`, raw LZMA1 with lc 3, lp 0, pb 2.
fn decode_control(packed: &[u8], method: &str, unpacked: u64) -> Result<Vec<u8>> {
    ensure!(
        unpacked <= CONTROL_MAX,
        "control block of {unpacked} bytes is implausible"
    );
    if method == "storing" {
        ensure!(
            packed.len() as u64 == unpacked,
            "stored control block has the wrong size"
        );
        return Ok(packed.to_vec());
    }
    let mut parts = method.split(':');
    ensure!(
        parts.next() == Some("lzma"),
        "control block method '{method}' is not supported"
    );
    let dict = parts.find_map(size_param).unwrap_or(1 << 20);
    let mut r = lzma_rust2::LzmaReader::new(packed, unpacked, 3, 0, 2, dict, None)
        .context("starting the LZMA decoder")?;
    let mut out = Vec::with_capacity(unpacked as usize);
    r.read_to_end(&mut out)
        .context("control block is damaged")?;
    ensure!(
        out.len() as u64 == unpacked,
        "control block decoded to the wrong size"
    );
    Ok(out)
}

/// `1mb`, `64m`, `512k`, `d64m`: a FreeArc size parameter.
fn size_param(p: &str) -> Option<u32> {
    let p = p.strip_prefix('d').unwrap_or(p).to_ascii_lowercase();
    let p = p.strip_suffix('b').unwrap_or(&p);
    let (num, mult) = match p.as_bytes().last()? {
        b'k' => (&p[..p.len() - 1], 1u64 << 10),
        b'm' => (&p[..p.len() - 1], 1 << 20),
        b'g' => (&p[..p.len() - 1], 1 << 30),
        _ => return None,
    };
    u32::try_from(num.parse::<u64>().ok()? * mult).ok()
}

struct Descriptor {
    method: String,
    unpacked: u64,
    packed: u64,
    crc: u32,
}

/// Parses a descriptor at `b[0..]` that must end exactly at `b.len()`.
fn descriptor(b: &[u8], kind: Crc) -> Result<Descriptor> {
    ensure!(b.starts_with(SIGNATURE), "no block descriptor");
    let mut i = 4;
    b.get(i).context("short descriptor")?; // block type, checked by the caller
    i += 1;
    let method = String::from_utf8(cstr(b, &mut i)?.to_vec()).context("method is not text")?;
    let unpacked = varint(b, &mut i)?;
    let packed = varint(b, &mut i)?;
    let crc = u32le(b, &mut i)?;
    let own = i;
    let self_crc = u32le(b, &mut i)?;
    ensure!(i == b.len(), "descriptor has trailing bytes");
    ensure!(
        kind.hash(&b[..own]) == self_crc,
        "descriptor checksum mismatch"
    );
    Ok(Descriptor {
        method,
        unpacked,
        packed,
        crc,
    })
}

fn read_at(f: &mut File, at: u64, len: u64) -> Result<Vec<u8>> {
    ensure!(
        len <= CONTROL_MAX,
        "control block of {len} bytes is implausible"
    );
    f.seek(SeekFrom::Start(at))?;
    let mut v = vec![0; len as usize];
    f.read_exact(&mut v).context("archive ends early")?;
    Ok(v)
}

fn control(f: &mut File, at: u64, d: &Descriptor, kind: Crc) -> Result<Vec<u8>> {
    let packed = read_at(f, at, d.packed)?;
    let out = decode_control(&packed, &d.method, d.unpacked)?;
    ensure!(kind.hash(&out) == d.crc, "control block checksum mismatch");
    Ok(out)
}

/// Whether `path` starts like a FreeArc archive.
pub fn sniff(path: &Path) -> bool {
    let mut head = [0u8; 4];
    File::open(path)
        .and_then(|mut f| f.read_exact(&mut head))
        .is_ok()
        && &head == SIGNATURE
}

pub fn open(path: &Path) -> Result<Archive> {
    let mut f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let len = f.metadata()?.len();
    let tail_len = len.min(4096);
    let tail = read_at(&mut f, len - tail_len, tail_len)?;

    // The footer descriptor is the last thing in the file; try every
    // signature from the end until one parses and ends exactly at EOF. Its
    // checksum also tells which CRC the whole archive uses.
    let (desc_at, footer, kind) = (0..tail.len().saturating_sub(4))
        .rev()
        .filter(|&k| tail[k..].starts_with(SIGNATURE) && tail.get(k + 4) == Some(&FOOTER))
        .find_map(|k| {
            [Crc::Ieee, Crc::FitGirl].into_iter().find_map(|kind| {
                descriptor(&tail[k..], kind)
                    .ok()
                    .map(|d| (len - tail_len + k as u64, d, kind))
            })
        })
        .context("no FreeArc footer at the end of the file (damaged or not FreeArc)")?;
    let footer_at = desc_at
        .checked_sub(footer.packed)
        .context("footer lies before the file")?;
    let fb = control(&mut f, footer_at, &footer, kind).context("reading the footer")?;

    let mut i = 0;
    let count = varint(&fb, &mut i)?;
    let mut blocks = Vec::new();
    for _ in 0..count {
        let ty = *fb.get(i).context("short footer")?;
        i += 1;
        let method = String::from_utf8(cstr(&fb, &mut i)?.to_vec())?;
        let back = varint(&fb, &mut i)?;
        let unpacked = varint(&fb, &mut i)?;
        let packed = varint(&fb, &mut i)?;
        let crc = u32le(&fb, &mut i)?;
        if ty != DIRECTORY {
            continue;
        }
        let at = footer_at
            .checked_sub(back)
            .context("directory lies before the file")?;
        let d = Descriptor {
            method,
            unpacked,
            packed,
            crc,
        };
        let db = control(&mut f, at, &d, kind).context("reading the directory")?;
        blocks.extend(directory(&db, at, kind).context("parsing the directory")?);
    }
    ensure!(!blocks.is_empty(), "the footer lists no directory");

    blocks.sort_by_key(|b| b.start);
    let mut end = 0;
    for b in &blocks {
        let b_end = b
            .start
            .checked_add(b.packed)
            .context("block size overflows")?;
        ensure!(b.packed == 0 || b.start >= end, "solid blocks overlap");
        ensure!(b_end <= footer_at, "a solid block reaches into the index");
        end = end.max(b_end);
    }
    Ok(Archive { len, blocks })
}

fn directory(d: &[u8], at: u64, kind: Crc) -> Result<Vec<Block>> {
    let mut i = 0;
    let n_blocks = varint(d, &mut i)? as usize;
    ensure!(n_blocks <= d.len(), "implausible block count");
    let mut counts = Vec::with_capacity(n_blocks);
    for _ in 0..n_blocks {
        counts.push(varint(d, &mut i)? as usize);
    }
    let mut methods = Vec::with_capacity(n_blocks);
    for _ in 0..n_blocks {
        methods.push(String::from_utf8(cstr(d, &mut i)?.to_vec())?);
    }
    let mut starts = Vec::with_capacity(n_blocks);
    for _ in 0..n_blocks {
        let back = varint(d, &mut i)?;
        starts.push(at.checked_sub(back).context("block lies before the file")?);
    }
    let mut packed = Vec::with_capacity(n_blocks);
    for _ in 0..n_blocks {
        packed.push(varint(d, &mut i)?);
    }
    let n: usize = counts.iter().sum();
    ensure!(n <= d.len(), "implausible entry count");
    let n_dirs = varint(d, &mut i)? as usize;
    ensure!(n_dirs <= d.len(), "implausible folder count");
    let mut dirs = Vec::with_capacity(n_dirs);
    for _ in 0..n_dirs {
        dirs.push(cstr(d, &mut i)?);
    }
    let mut names = Vec::with_capacity(n);
    for _ in 0..n {
        names.push(cstr(d, &mut i)?);
    }
    let mut folder = Vec::with_capacity(n);
    for _ in 0..n {
        folder.push(varint(d, &mut i)? as usize);
    }
    let mut sizes = Vec::with_capacity(n);
    for _ in 0..n {
        sizes.push(varint(d, &mut i)?);
    }
    i += 4 * n; // modification times, not restored in v0.1
    let flags = d
        .get(i..i + n)
        .context("index ends in the folder flags")?
        .to_vec();
    i += n;
    let mut crcs = Vec::with_capacity(n);
    for _ in 0..n {
        crcs.push(u32le(d, &mut i)?);
    }
    // ponytail: FreeArc can append optional fields; seen archives end with a
    // single 0 here. Anything else is refused rather than guessed at.
    ensure!(
        d.get(i..) == Some(&[0][..]),
        "directory has fields this version doesn't know"
    );

    let mut k = 0;
    let mut blocks = Vec::with_capacity(n_blocks);
    for b in 0..n_blocks {
        let mut entries = Vec::with_capacity(counts[b]);
        for _ in 0..counts[b] {
            let dir = *dirs
                .get(folder[k])
                .context("entry names a folder that doesn't exist")?;
            let path = safe_path(dir, names[k])?;
            entries.push(Entry {
                path,
                size: sizes[k],
                crc: crcs[k],
                is_dir: flags[k] != 0,
            });
            k += 1;
        }
        blocks.push(Block {
            method: methods[b].clone(),
            start: starts[b],
            packed: packed[b],
            entries,
            crc: kind,
        });
    }
    Ok(blocks)
}

/// `CON`, `NUL`, `COM1` and the rest open a device on Windows, with any
/// extension, so `nul.txt` can't be a file either.
fn is_device_name(part: &str) -> bool {
    let stem = part
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ((stem.starts_with("COM") || stem.starts_with("LPT"))
        && stem.len() == 4
        && stem.as_bytes()[3].is_ascii_digit())
}

/// Joins a folder and a name from the archive into a relative path that can't
/// leave the output folder.
pub fn safe_path(dir: &[u8], name: &[u8]) -> Result<PathBuf> {
    let text = |b: &[u8]| String::from_utf8(b.to_vec()).context("entry name is not UTF-8");
    let joined = if dir.is_empty() {
        text(name)?
    } else {
        format!("{}/{}", text(dir)?, text(name)?)
    };
    let mut out = PathBuf::new();
    for part in joined
        .split(['/', '\\'])
        .filter(|p| !p.is_empty() && *p != ".")
    {
        let bad = part == ".."
            || part.contains([':', '\0', '<', '>', '"', '|', '?', '*'])
            || part.ends_with(['.', ' '])
            || part.chars().any(|c| (c as u32) < 32)
            || is_device_name(part);
        if bad {
            bail!("unsafe entry name '{joined}'");
        }
        out.push(part);
    }
    ensure!(
        out.components().all(|c| matches!(c, Component::Normal(_))) && !out.as_os_str().is_empty(),
        "unsafe entry name '{joined}'"
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_from_real_archives() {
        let mut i = 0;
        assert_eq!(varint(&[0x7e], &mut i).unwrap(), 63);
        let b = [0x4f, 0x65, 0xad, 0x49, 0x02];
        let mut i = 0;
        // data2.doi: the footer's distance back to the header block.
        assert_eq!(varint(&b, &mut i).unwrap(), 307_063_594);
        assert_eq!(i, 5);
    }

    #[test]
    fn fitgirl_crc() {
        // fg-05.bin's stored header block and its CRC from the footer.
        assert_eq!(Crc::FitGirl.hash(b"ArC  "), 0xf664dce3);
        let mut h = Crc::FitGirl.hasher();
        h.update(b"ArC");
        h.update(b"  ");
        assert_eq!(h.finalize(), 0xf664dce3);
        assert_eq!(Crc::Ieee.hash(b"123456789"), 0xcbf43926);
    }

    #[test]
    fn unsafe_names_are_refused() {
        for (d, n) in [
            ("", "../x"),
            ("a", "..\\..\\x"),
            ("C:", "x"),
            ("a", "b:stream"),
            ("", "con."),
            ("a", "NUL.txt"),
            ("com1", "x"),
            ("", "lpt9.dat"),
            ("", "a\u{1}b"),
        ] {
            assert!(safe_path(d.as_bytes(), n.as_bytes()).is_err(), "{d}/{n}");
        }
        assert!(safe_path(b"", b"console.txt").is_ok() && safe_path(b"", b"com10").is_ok());
        let p = safe_path(b"clausewitz/gfx", b"busy.ani").unwrap();
        assert_eq!(p, Path::new("clausewitz").join("gfx").join("busy.ani"));
    }

    /// Reads a real repack data file when UNREPACK_SAMPLE names one.
    #[test]
    fn real_sample() {
        let Some(p) = std::env::var_os("UNREPACK_SAMPLE") else {
            return;
        };
        let a = open(Path::new(&p)).unwrap();
        assert!(a.entries() > 0);
        for b in &a.blocks {
            eprintln!(
                "{} at {} packed {} unpacked {} entries {}",
                b.method,
                b.start,
                b.packed,
                b.unpacked(),
                b.entries.len()
            );
        }
    }
}
