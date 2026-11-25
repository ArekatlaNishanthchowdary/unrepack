//! ZIP archives, read as one more kind of data file: every entry becomes a
//! block of its own (`storing` or `deflate`) at its data's offset, so
//! extracting, verifying and freeing work as they do for FreeArc.
//!
//! The record parsing follows HatchZip's `archive/zip.rs` (MIT or
//! Apache-2.0): the end-record search, ZIP64 and the local header offset
//! read from the ZIP64 extra field. Encrypted entries, methods other than
//! stored and deflate, and split archives are refused; HatchZip extracts
//! those.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};

use crate::freearc::{Archive, Block, Crc, Entry, safe_path};

const LFH: u32 = 0x0403_4b50;
const CDFH: u32 = 0x0201_4b50;
const EOCD: u32 = 0x0605_4b50;
const ZIP64_EOCD: u32 = 0x0606_4b50;
const ZIP64_LOCATOR: u32 = 0x0706_4b50;
/// A central directory this big is damage or hostile.
const INDEX_MAX: u64 = 1 << 30;

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

fn read_at(f: &mut File, at: u64, len: u64) -> Result<Vec<u8>> {
    f.seek(SeekFrom::Start(at))?;
    let mut b = vec![0; usize::try_from(len)?];
    f.read_exact(&mut b)?;
    Ok(b)
}

/// Whether `path` starts like a ZIP archive (a local file header first, so
/// no self-extractor stub in front).
pub fn sniff(path: &Path) -> bool {
    let mut head = [0u8; 4];
    File::open(path)
        .and_then(|mut f| f.read_exact(&mut head))
        .is_ok()
        && u32::from_le_bytes(head) == LFH
}

/// Uncompressed size, compressed size and local header offset of a central
/// directory record, from its ZIP64 extra field where the 32-bit ones are
/// saturated. That field lists only the saturated values, in that order.
fn sizes(record: &[u8], mut extra: &[u8]) -> Option<(u64, u64, u64)> {
    let mut v = [24, 20, 42].map(|at| u64::from(u32_at(record, at).unwrap_or(0)));
    let saturated: Vec<usize> = (0..3).filter(|&i| v[i] == 0xFFFF_FFFF).collect();
    if saturated.is_empty() {
        return Some((v[0], v[1], v[2]));
    }
    while extra.len() >= 4 {
        let id = u16_at(extra, 0)?;
        let len = u16_at(extra, 2)? as usize;
        let body = extra.get(4..4 + len)?;
        if id == 0x0001 {
            for (k, &i) in saturated.iter().enumerate() {
                v[i] = u64_at(body, k * 8)?;
            }
            return Some((v[0], v[1], v[2]));
        }
        extra = &extra[4 + len..];
    }
    None
}

pub fn open(path: &Path) -> Result<Archive> {
    let mut f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    // Partly freed by an extraction that stopped (`extract::run`).
    let freed = crate::extract::marker(path).exists();
    let len = f.metadata()?.len();
    let tail_len = len.min(22 + 0xFFFF + 20);
    let t = read_at(&mut f, len - tail_len, tail_len)?;
    // The comment length must reach exactly to the end, so a signature inside
    // the comment isn't taken for the record.
    let eocd = (0..t.len().saturating_sub(21))
        .rev()
        .find(|&i| {
            u32_at(&t, i) == Some(EOCD)
                && u16_at(&t, i + 20).is_some_and(|n| i + 22 + n as usize == t.len())
        })
        .context("no ZIP end record at the end of the file (damaged, or split into parts)")?;
    let eocd_at = len - tail_len + eocd as u64;
    ensure!(
        u16_at(&t, eocd + 4) == Some(0) && u16_at(&t, eocd + 6) == Some(0),
        "this ZIP is split into several files, which unrepack can't extract"
    );
    let zip64 = eocd >= 20 && u32_at(&t, eocd - 20) == Some(ZIP64_LOCATOR);
    let (cd_end, cd_size, cd_offset, count) = if zip64 {
        let rec_at = u64_at(&t, eocd - 20 + 8).context("short ZIP64 locator")?;
        let r = read_at(&mut f, rec_at, 56).context("reading the ZIP64 end record")?;
        ensure!(
            u32_at(&r, 0) == Some(ZIP64_EOCD),
            "the ZIP64 end record is damaged"
        );
        ensure!(
            u32_at(&r, 16) == Some(0) && u32_at(&r, 20) == Some(0),
            "this ZIP is split into several files, which unrepack can't extract"
        );
        (
            rec_at,
            u64_at(&r, 40).unwrap(),
            u64_at(&r, 48).unwrap(),
            u64_at(&r, 32).unwrap(),
        )
    } else {
        (
            eocd_at,
            u64::from(u32_at(&t, eocd + 12).unwrap()),
            u64::from(u32_at(&t, eocd + 16).unwrap()),
            u64::from(u16_at(&t, eocd + 10).unwrap()),
        )
    };
    // The file starts with a local header, so offsets count from 0 and the
    // index ends where the end records begin.
    ensure!(
        cd_offset.checked_add(cd_size) == Some(cd_end) && cd_size <= INDEX_MAX,
        "the ZIP index isn't where its end record says (damaged, or not a plain ZIP)"
    );
    let cd = read_at(&mut f, cd_offset, cd_size).context("reading the ZIP index")?;

    // (local header offset, block)
    let mut found: Vec<(u64, Block)> = Vec::new();
    let mut at = 0;
    while at < cd.len() {
        let damaged = || anyhow::anyhow!("the ZIP index is damaged");
        ensure!(u32_at(&cd, at) == Some(CDFH), damaged());
        let n = u16_at(&cd, at + 28).ok_or_else(damaged)? as usize;
        let x = u16_at(&cd, at + 30).ok_or_else(damaged)? as usize;
        let c = u16_at(&cd, at + 32).ok_or_else(damaged)? as usize;
        let rec = cd.get(at..at + 46 + n + x + c).ok_or_else(damaged)?;
        at += rec.len();
        let raw = &rec[46..46 + n];
        let name = String::from_utf8_lossy(raw);
        let flags = u16_at(rec, 8).unwrap();
        let method = u16_at(rec, 10).unwrap();
        ensure!(
            flags & 0x41 == 0,
            "{name} is encrypted; unrepack can't extract password-protected ZIPs"
        );
        // Unix symlinks keep their target as the data.
        let unix_mode = (u32_at(rec, 38).unwrap() >> 16) & 0o170000;
        ensure!(
            rec[5] != 3 || unix_mode != 0o120000,
            "{name} is a symbolic link, which unrepack doesn't make"
        );
        let (size, packed, offset) = sizes(rec, &rec[46 + n..46 + n + x]).ok_or_else(damaged)?;
        let is_dir = raw.ends_with(b"/") || raw.ends_with(b"\\");
        let method = match method {
            0 => "storing",
            8 => "deflate",
            m => bail!(
                "{name} is compressed with ZIP method {m}; unrepack extracts only stored and \
                 deflate entries"
            ),
        };
        ensure!(!is_dir || size == 0, "the folder entry {name} has data");
        let path = safe_path(b"", raw)?;
        // Where the data starts is only in the local header.
        let lh = read_at(&mut f, offset, 30).with_context(|| format!("reading {name}'s header"))?;
        // An entry --free-as-read already freed reads as zeros. A rerun only
        // checks such a block's files on disk, or finds it lost, so where its
        // data started doesn't matter.
        let start = if freed && lh.iter().all(|&b| b == 0) {
            offset + 30 + n as u64
        } else {
            ensure!(
                u32_at(&lh, 0) == Some(LFH),
                "{name}'s header isn't where the index says"
            );
            offset + 30 + u64::from(u16_at(&lh, 26).unwrap()) + u64::from(u16_at(&lh, 28).unwrap())
        };
        found.push((
            offset,
            Block {
                method: method.into(),
                start,
                packed,
                entries: vec![Entry {
                    path,
                    size,
                    crc: u32_at(rec, 16).unwrap(),
                    is_dir,
                }],
                crc: Crc::Ieee,
            },
        ));
    }
    ensure!(
        found.len() as u64 == count,
        "the ZIP index lists {} entries, its end record says {count}",
        found.len()
    );
    ensure!(!found.is_empty(), "the ZIP is empty");

    // Freeing reads each entry once, front to back: no entry may share bytes
    // with another, or with the index.
    found.sort_by_key(|(o, _)| *o);
    let mut end = 0;
    for (o, b) in &found {
        let b_end = b
            .start
            .checked_add(b.packed)
            .context("entry size overflows")?;
        ensure!(
            *o >= end,
            "entries in this ZIP overlap ({}), which unrepack refuses",
            b.entries[0].path.display()
        );
        ensure!(
            b_end <= cd_offset,
            "{} reaches into the ZIP index",
            b.entries[0].path.display()
        );
        end = b_end;
    }
    Ok(Archive {
        len,
        blocks: found.into_iter().map(|(_, b)| b).collect(),
    })
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::io::Write;

    pub struct Item<'a> {
        pub name: &'a str,
        pub data: &'a [u8],
        pub deflate: bool,
        pub flags: u16,
        pub method: Option<u16>,
    }

    pub fn item<'a>(name: &'a str, data: &'a [u8], deflate: bool) -> Item<'a> {
        Item {
            name,
            data,
            deflate,
            flags: 0,
            method: None,
        }
    }

    /// A ZIP as 7-Zip writes one, optionally with ZIP64 records throughout.
    pub fn build(items: &[Item], zip64: bool) -> Vec<u8> {
        let mut out = Vec::new();
        let mut cd = Vec::new();
        for it in items {
            let packed = if it.deflate {
                let mut e =
                    flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
                e.write_all(it.data).unwrap();
                e.finish().unwrap()
            } else {
                it.data.to_vec()
            };
            let method = it.method.unwrap_or(if it.deflate { 8 } else { 0 });
            let crc = crc32fast::hash(it.data);
            let offset = out.len() as u64;
            let sat = |v: u64| if zip64 { 0xFFFF_FFFF } else { v as u32 };
            let mut ex = Vec::new();
            if zip64 {
                ex.extend(1u16.to_le_bytes());
                ex.extend(24u16.to_le_bytes());
                ex.extend((it.data.len() as u64).to_le_bytes());
                ex.extend((packed.len() as u64).to_le_bytes());
                ex.extend(offset.to_le_bytes());
            }
            out.extend(LFH.to_le_bytes());
            out.extend([45, 0]);
            out.extend(it.flags.to_le_bytes());
            out.extend(method.to_le_bytes());
            out.extend([0; 4]);
            out.extend(crc.to_le_bytes());
            out.extend(sat(packed.len() as u64).to_le_bytes());
            out.extend(sat(it.data.len() as u64).to_le_bytes());
            out.extend((it.name.len() as u16).to_le_bytes());
            out.extend(0u16.to_le_bytes());
            out.extend(it.name.as_bytes());
            out.extend(&packed);

            cd.extend(CDFH.to_le_bytes());
            cd.extend([45, 0, 45, 0]);
            cd.extend(it.flags.to_le_bytes());
            cd.extend(method.to_le_bytes());
            cd.extend([0; 4]);
            cd.extend(crc.to_le_bytes());
            cd.extend(sat(packed.len() as u64).to_le_bytes());
            cd.extend(sat(it.data.len() as u64).to_le_bytes());
            cd.extend((it.name.len() as u16).to_le_bytes());
            cd.extend((ex.len() as u16).to_le_bytes());
            // Comment length, disk, internal and external attributes.
            cd.extend([0; 10]);
            cd.extend(sat(offset).to_le_bytes());
            cd.extend(it.name.as_bytes());
            cd.extend(&ex);
        }
        let cd_offset = out.len() as u64;
        out.extend(&cd);
        let n = items.len() as u64;
        if zip64 {
            let rec = out.len() as u64;
            out.extend(ZIP64_EOCD.to_le_bytes());
            out.extend(44u64.to_le_bytes());
            out.extend([45, 0, 45, 0]);
            out.extend([0; 8]);
            out.extend(n.to_le_bytes());
            out.extend(n.to_le_bytes());
            out.extend((cd.len() as u64).to_le_bytes());
            out.extend(cd_offset.to_le_bytes());
            out.extend(ZIP64_LOCATOR.to_le_bytes());
            out.extend(0u32.to_le_bytes());
            out.extend(rec.to_le_bytes());
            out.extend(1u32.to_le_bytes());
        }
        out.extend(EOCD.to_le_bytes());
        out.extend([0; 4]);
        let n16 = if zip64 { 0xFFFF } else { n as u16 };
        out.extend(n16.to_le_bytes());
        out.extend(n16.to_le_bytes());
        out.extend((if zip64 { 0xFFFF_FFFF } else { cd.len() as u32 }).to_le_bytes());
        out.extend((if zip64 { 0xFFFF_FFFF } else { cd_offset as u32 }).to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out
    }

    fn open_bytes(b: &[u8]) -> Result<Archive> {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.zip");
        std::fs::write(&p, b).unwrap();
        open(&p)
    }

    #[test]
    fn reads_the_index_plain_and_zip64() {
        for zip64 in [false, true] {
            let a = open_bytes(&build(
                &[
                    item("dir/", b"", false),
                    item("dir/a.txt", b"hello", false),
                    item("b.bin", &[7; 5000], true),
                ],
                zip64,
            ))
            .unwrap();
            assert_eq!(a.blocks.len(), 3);
            assert!(a.blocks[0].entries[0].is_dir);
            let b = &a.blocks[2];
            assert_eq!(b.method, "deflate");
            assert_eq!(b.entries[0].path, Path::new("b.bin"));
            assert_eq!(b.entries[0].size, 5000);
            assert_eq!(b.entries[0].crc, crc32fast::hash(&[7; 5000]));
            assert!(b.packed < 5000);
            assert_eq!(a.blocks[1].start, 30 + 4 + 30 + 9);
        }
    }

    /// 300 entries of 200 KB, each smaller than the 1 MiB punch unit: freeing
    /// must still give back nearly the whole archive as it goes.
    #[test]
    fn small_entries_are_freed_and_extract() {
        let data: Vec<Vec<u8>> = (0..300u32)
            .map(|i| {
                (0..200_000u32)
                    .map(|k| (k.wrapping_mul(2_654_435_761) ^ i) as u8)
                    .collect()
            })
            .collect();
        let names: Vec<String> = (0..300).map(|i| format!("d{}/f{i}.bin", i % 7)).collect();
        let items: Vec<Item> = names
            .iter()
            .zip(&data)
            .enumerate()
            .map(|(i, (n, d))| item(n, d, i % 2 == 0))
            .collect();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("game.zip");
        std::fs::write(&p, build(&items, false)).unwrap();
        let a = open(&p).unwrap();
        let dec = crate::extract::Decoders {
            dir: Err("none".into()),
            host: Default::default(),
            work: dir.path().join("work"),
            pending: Vec::new(),
        };
        let out = dir.path().join("out");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&p)
            .unwrap();
        crate::win::set_sparse(&file).unwrap();
        let mut p_ = crate::extract::Progress::new(a.unpacked());
        let mut freed = a.blocks[0].start;
        for b in &a.blocks {
            crate::extract::block(&p, &file, b, &dec, Some(&out), Some(&mut freed), &mut p_)
                .unwrap();
        }
        drop(file);
        let left = crate::win::allocated(&p).unwrap();
        assert!(left < 2 << 20, "{left} of {} bytes still allocated", a.len);
        for (n, d) in names.iter().zip(&data) {
            assert_eq!(&std::fs::read(out.join(n)).unwrap(), d, "{n}");
        }
    }

    #[test]
    fn refusals() {
        let mut enc = item("a", b"x", false);
        enc.flags = 1;
        let mut bz = item("a", b"x", false);
        bz.method = Some(12);
        for (bad, why) in [
            (build(&[enc], false), "encrypted"),
            (build(&[bz], false), "method 12"),
            (build(&[item("../a", b"x", false)], false), "unsafe"),
            (build(&[], false), "empty"),
        ] {
            let e = format!("{:#}", open_bytes(&bad).unwrap_err());
            assert!(e.contains(why), "{e}");
        }
        // Two entries pointing at the same bytes.
        let mut b = build(&[item("a", b"x", false), item("b", b"y", false)], false);
        let second = b.windows(4).rposition(|w| w == CDFH.to_le_bytes()).unwrap();
        b[second + 42..second + 46].copy_from_slice(&0u32.to_le_bytes());
        let e = format!("{:#}", open_bytes(&b).unwrap_err());
        assert!(e.contains("overlap"), "{e}");
        // Cut short: no end record.
        let e = format!("{:#}", open_bytes(&b[..b.len() - 3]).unwrap_err());
        assert!(e.contains("no ZIP end record"), "{e}");
    }
}
