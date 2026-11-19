//! Reads the decoders out of a repack's Inno Setup 5.5 (unicode) installer,
//! the way the installer would put them in `{tmp}`, without running it.
//!
//! Layout (checked on DODI's and FitGirl's setups): the second "Inno Setup
//! Setup Data" ID is followed by two LZMA1 header blocks. Block 0 holds the
//! file entries (destination name, then the index of a location record);
//! block 1 holds 74-byte location records (chunk offset, offset inside the
//! decoded chunk, size, SHA-1, flags). Chunk offsets count from the start of
//! the data area, a run of "zlb\x1a" LZMA chunks that sits before the header.

use std::fs;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};

const ID: &[u8] = b"Inno Setup Setup Data (5.5.0) (u)";
const REC: usize = 74;
const CHUNK: &[u8] = b"zlb\x1a";
const CALL_FILTER: u16 = 0x10;
const ENCRYPTED: u16 = 0x40;
const COMPRESSED: u16 = 0x80;
/// Decoders are a few MB; anything near this is not a decoder set.
const MAX_OUT: u64 = 1 << 31;

fn u32le(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}

fn u64le(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().unwrap())
}

/// Reads up to `max` bytes, stopping where the stream ends. Inno's LZMA1
/// header blocks have no end marker, so the decoder runs out of input.
fn read_up_to(mut r: impl Read, max: u64) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 65536];
    while (out.len() as u64) < max {
        match r.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
        }
    }
    out.truncate(max.min(out.len() as u64) as usize);
    out
}

/// One header block: CRC, stored size, compressed flag, then 4 KiB pieces
/// each led by its CRC-32, together one raw LZMA1 stream with 5 props bytes.
fn header_block(data: &[u8], p: &mut usize) -> Result<Vec<u8>> {
    let h = data
        .get(*p..*p + 9)
        .context("the setup header is cut short")?;
    ensure!(
        crc32fast::hash(&h[4..]) == u32le(h, 0),
        "the setup header is damaged"
    );
    ensure!(h[8] == 1, "the setup header isn't compressed the usual way");
    let stored = u32le(h, 4) as usize;
    let body = data
        .get(*p + 9..*p + 9 + stored)
        .context("the setup header is cut short")?;
    *p += 9 + stored;
    let mut raw = Vec::with_capacity(stored);
    for piece in body.chunks(4096 + 4) {
        ensure!(
            piece.len() > 4 && crc32fast::hash(&piece[4..]) == u32le(piece, 0),
            "the setup header is damaged"
        );
        raw.extend_from_slice(&piece[4..]);
    }
    ensure!(raw.len() > 5, "the setup header is empty");
    let r =
        lzma_rust2::LzmaReader::new_with_props(&raw[5..], u64::MAX, raw[0], u32le(&raw, 1), None)?;
    Ok(read_up_to(r, MAX_OUT))
}

/// LZMA2's one props byte to its dictionary size.
fn lzma2_dict(p: u8) -> Result<u32> {
    ensure!(p <= 40, "bad LZMA2 dictionary size");
    Ok(if p == 40 {
        u32::MAX
    } else {
        (2 | (p as u32 & 1)) << (p / 2 + 11)
    })
}

/// The first `need` bytes of the chunk at `at` (just past "zlb\x1a").
fn chunk(data: &[u8], at: usize, compressed: bool, need: u64) -> Result<Vec<u8>> {
    let body = &data[at..];
    let out = if !compressed {
        body.get(..need as usize).map(<[u8]>::to_vec)
    } else if body.first().is_some_and(|&p| p <= 40) {
        // ponytail: one props byte <= 40 means LZMA2, five means LZMA1; the
        // setup header names the method, but SHA-1 catches a wrong guess.
        let r = lzma_rust2::Lzma2Reader::new(&body[1..], lzma2_dict(body[0])?, None);
        Some(read_up_to(r, need))
    } else if body.len() > 5 {
        let r = lzma_rust2::LzmaReader::new_with_props(
            &body[5..],
            u64::MAX,
            body[0],
            u32le(body, 1),
            None,
        )?;
        Some(read_up_to(r, need))
    } else {
        None
    };
    out.filter(|o| o.len() as u64 == need)
        .context("a chunk in Setup.exe is cut short")
}

/// Undoes Inno's CALL/JMP filter: the target of every E8/E9 whose high byte
/// is 00 or FF was stored as an offset from the file start (64 KiB buffers,
/// the last 4 bytes of each skipped).
fn unfilter(b: &mut [u8]) {
    for start in (0..b.len()).step_by(0x10000) {
        let end = (start + 0x10000).min(b.len());
        let mut i = start;
        while i + 4 < end {
            if b[i] == 0xE8 || b[i] == 0xE9 {
                i += 1;
                if b[i + 3] == 0 || b[i + 3] == 0xFF {
                    let addr = (i + 4) as u32 & 0xFF_FFFF;
                    let rel = (b[i] as u32 | (b[i + 1] as u32) << 8 | (b[i + 2] as u32) << 16)
                        .wrapping_sub(addr)
                        & 0xFF_FFFF;
                    if rel & 0x80_0000 != 0 {
                        b[i + 3] ^= 0xFF;
                    }
                    b[i..i + 3].copy_from_slice(&rel.to_le_bytes()[..3]);
                }
                i += 4;
            } else {
                i += 1;
            }
        }
    }
}

fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&(data.len() as u64 * 8).to_be_bytes());
    for block in msg.chunks(64) {
        let mut w = [0u32; 80];
        for t in 0..16 {
            w[t] = u32::from_be_bytes(block[t * 4..t * 4 + 4].try_into().unwrap());
        }
        for t in 16..80 {
            w[t] = (w[t - 3] ^ w[t - 8] ^ w[t - 14] ^ w[t - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (t, &wt) in w.iter().enumerate() {
            let (f, k) = match t {
                0..=19 => ((b & c) | (!b & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wt);
            (e, d, c, b, a) = (d, c, b.rotate_left(30), a, t);
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 20];
    for (i, x) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&x.to_be_bytes());
    }
    out
}

/// A UTF-16 string with its byte length in front.
fn wstr(b: &[u8], i: usize) -> Option<(String, usize)> {
    let n = u32le(b.get(i..i + 4)?, 0) as usize;
    let raw = b.get(i + 4..i + 4 + n).filter(|_| n.is_multiple_of(2))?;
    let units: Vec<u16> = raw
        .chunks(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    Some((String::from_utf16(&units).ok()?, i + 4 + n))
}

/// A plain file name, nothing that could leave the folder.
fn safe_name(n: &str) -> bool {
    !n.is_empty()
        && n != "."
        && n != ".."
        && !n.contains(['\\', '/', ':', '\0'])
        && !n.ends_with(['.', ' '])
}

/// `(name, location index)` of every file the installer puts straight into
/// `{tmp}`. ponytail: found by searching block 0 for "{tmp}\" names instead
/// of parsing every header section before the file entries; the entry is the
/// destination string, 8 more strings, MinVersion and OnlyBelowVersion (10
/// bytes each), then the location index.
fn tmp_files(h0: &[u8], count: usize) -> Vec<(String, usize)> {
    let pat: Vec<u8> = "{tmp}\\"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    let mut out: Vec<(String, usize)> = Vec::new();
    for at in (4..h0.len().saturating_sub(pat.len())).filter(|&i| h0[i..].starts_with(&pat)) {
        let entry = (|| {
            let (dest, mut j) = wstr(h0, at - 4)?;
            for _ in 0..8 {
                j = wstr(h0, j)?.1;
            }
            let loc = i32::from_le_bytes(h0.get(j + 20..j + 24)?.try_into().ok()?);
            let name = dest.strip_prefix("{tmp}\\")?.to_string();
            (safe_name(&name) && (0..count as i32).contains(&loc)).then_some((name, loc as usize))
        })();
        if let Some((name, loc)) = entry
            && !out.iter().any(|(n, _)| n.eq_ignore_ascii_case(&name))
        {
            out.push((name, loc));
        }
    }
    out
}

/// `{app}` and `{tmp}` in an ini become `.`, the decoders' working folder.
fn patch_ini(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i..].starts_with(b"{app}") || b[i..].starts_with(b"{tmp}") {
            out.push(b'.');
            i += 5;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

/// Writes the files the installer would put in `{tmp}` (its decoders among
/// them) into `out`, each checked against its SHA-1. Returns how many.
pub fn carve(setup: &Path, out: &Path) -> Result<usize> {
    let data = fs::read(setup).with_context(|| format!("reading {}", setup.display()))?;
    let Some(id) = data.windows(ID.len()).rposition(|w| w == ID) else {
        bail!("not an Inno Setup 5.5 installer, the only kind unrepack can read the decoders from");
    };
    let mut p = id + 64;
    let h0 = header_block(&data, &mut p)?;
    let h1 = header_block(&data, &mut p)?;
    ensure!(
        !h1.is_empty() && h1.len().is_multiple_of(REC),
        "Setup.exe's file table isn't the Inno Setup 5.5 kind"
    );
    let recs: Vec<&[u8]> = h1.chunks(REC).collect();
    let files = tmp_files(&h0, recs.len());
    ensure!(!files.is_empty(), "Setup.exe puts no files in {{tmp}}");

    // The data area starts at the first "zlb\x1a" that every chunk offset
    // lines up from.
    let mut starts: Vec<u64> = files
        .iter()
        .map(|&(_, l)| u32le(recs[l], 8) as u64)
        .collect();
    starts.sort();
    starts.dedup();
    let base = (0..data.len().saturating_sub(4))
        .filter(|&i| data[i..].starts_with(CHUNK))
        .find(|&b| {
            starts.iter().all(|&s| {
                data.get(b + s as usize..)
                    .is_some_and(|d| d.starts_with(CHUNK))
            })
        })
        .context("can't find Setup.exe's data chunks")?;

    fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;
    for &start in &starts {
        let here: Vec<&(String, usize)> = files
            .iter()
            .filter(|(_, l)| u32le(recs[*l], 8) as u64 == start)
            .collect();
        let mut need = 0u64;
        let mut flags = 0u16;
        for &(_, l) in &here {
            let r = recs[*l];
            ensure!(
                u32le(r, 0) == 0 && u32le(r, 4) == 0,
                "Setup.exe keeps its files in separate setup-N.bin parts, which unrepack can't read yet"
            );
            flags = u16::from_le_bytes([r[72], r[73]]);
            ensure!(flags & ENCRYPTED == 0, "Setup.exe's files are encrypted");
            need = need.max(u64le(r, 12).saturating_add(u64le(r, 20)));
        }
        ensure!(need <= MAX_OUT, "a chunk in Setup.exe is too large");
        let body = chunk(
            &data,
            base + start as usize + CHUNK.len(),
            flags & COMPRESSED != 0,
            need,
        )?;
        for (name, l) in here {
            let r = recs[*l];
            let (sub, size) = (u64le(r, 12) as usize, u64le(r, 20) as usize);
            let mut file = body[sub..sub + size].to_vec();
            if u16::from_le_bytes([r[72], r[73]]) & CALL_FILTER != 0 {
                unfilter(&mut file);
            }
            ensure!(
                sha1(&file) == r[36..56],
                "{name} from Setup.exe doesn't match its SHA-1"
            );
            if name.to_ascii_lowercase().ends_with(".ini") {
                file = patch_ini(&file);
            }
            let path = out.join(name);
            fs::write(&path, &file).with_context(|| format!("writing {}", path.display()))?;
        }
    }
    Ok(files.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Inno's forward filter, to check that `unfilter` undoes it.
    fn filter(b: &mut [u8]) {
        for start in (0..b.len()).step_by(0x10000) {
            let end = (start + 0x10000).min(b.len());
            let mut i = start;
            while i + 4 < end {
                if b[i] == 0xE8 || b[i] == 0xE9 {
                    i += 1;
                    if b[i + 3] == 0 || b[i + 3] == 0xFF {
                        let addr = (i + 4) as u32 & 0xFF_FFFF;
                        let rel = b[i] as u32 | (b[i + 1] as u32) << 8 | (b[i + 2] as u32) << 16;
                        let v = rel.wrapping_add(addr) & 0xFF_FFFF;
                        if rel & 0x80_0000 != 0 {
                            b[i + 3] ^= 0xFF;
                        }
                        b[i..i + 3].copy_from_slice(&v.to_le_bytes()[..3]);
                    }
                    i += 4;
                } else {
                    i += 1;
                }
            }
        }
    }

    #[test]
    fn unfilter_undoes_the_call_filter() {
        // Pseudo-random bytes with plenty of E8/E9, across a 64 KiB boundary.
        let mut x = 0x1234_5678u32;
        let orig: Vec<u8> = (0..200_000)
            .map(|i| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                match i % 7 {
                    0 => 0xE8,
                    3 => [0, 0xFF][(x & 1) as usize],
                    _ => x as u8,
                }
            })
            .collect();
        let mut b = orig.clone();
        filter(&mut b);
        assert_ne!(b, orig);
        unfilter(&mut b);
        assert_eq!(b, orig);
    }

    #[test]
    fn sha1_known_values() {
        let hex = |d: [u8; 20]| d.iter().map(|b| format!("{b:02x}")).collect::<String>();
        assert_eq!(hex(sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            hex(sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            hex(sha1(&[b'a'; 1000])),
            "291e9a6c66994949b57ba5e650361e98fc36b1ba"
        );
    }

    #[test]
    fn small_pieces() {
        assert_eq!(lzma2_dict(0).unwrap(), 4096);
        assert_eq!(lzma2_dict(1).unwrap(), 6144);
        assert_eq!(lzma2_dict(28).unwrap(), 1 << 26);
        assert!(lzma2_dict(41).is_err());
        assert_eq!(
            patch_ini(b"TempPath={app}\r\nTmp={tmp}\\x\r\n{ap"),
            b"TempPath=.\r\nTmp=.\\x\r\n{ap"
        );
        assert!(safe_name("cls-srep.dll"));
        for bad in ["", ".", "..", "a\\b", "a/b", "c:x", "x.", "x "] {
            assert!(!safe_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn finds_tmp_entries() {
        let w = |s: &str| {
            let u: Vec<u8> = s.encode_utf16().flat_map(u16::to_le_bytes).collect();
            [(u.len() as u32).to_le_bytes().to_vec(), u].concat()
        };
        let entry = |dest: &str, loc: i32| {
            let mut e = w("src");
            e.extend(w(dest));
            for _ in 0..8 {
                e.extend(w(""));
            }
            e.extend([0u8; 20]);
            e.extend(loc.to_le_bytes());
            e
        };
        let h0 = [
            entry("{tmp}\\cls.ini", 1),
            entry("{tmp}\\..", 0),
            entry("{tmp}\\CLS.INI", 2),
            entry("{tmp}\\far.dll", 9),
            entry("{tmp}\\arc.ini", 0),
        ]
        .concat();
        assert_eq!(
            tmp_files(&h0, 3),
            [("cls.ini".to_string(), 1), ("arc.ini".to_string(), 0)]
        );
    }
}
