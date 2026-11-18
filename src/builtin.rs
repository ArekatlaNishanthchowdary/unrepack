//! FreeArc's built-in `delta` and `dispack070` decoders, ported from the
//! FreeArc 0.67 source (github.com/mrkaban/FreeArc-0.67, a GPL-2.0 tree):
//! `Compression/Delta/Delta.cpp` (`delta_decompress`, Bulat Ziganshin; its
//! header allows free use of the decompression part to decompress FreeArc
//! archives) and `Compression/DisPack/C_DisPack.cpp` + `DisPack.cpp`
//! (`DISPACK_METHOD::decompress` and `DisUnFilter`, Fabian Giesen's x86
//! filter). The opcode tables below were generated from that source and
//! match FitGirl's unarc.dll byte for byte.
//!
//! Where the C code trusts sizes from the stream, these refuse them instead.
//!
//! Licence: not MIT OR Apache-2.0 like the rest of unrepack; see LICENSE-FREEARC.

use std::io::{self, Read, Write};

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

/// A little-endian u32, or `None` at a clean end of input (FreeArc's
/// `READ4_OR_EOF`).
fn read4_or_eof(r: &mut dyn Read, what: &str) -> io::Result<Option<u32>> {
    let mut b = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        match r.read(&mut b[got..])? {
            0 => break,
            n => got += n,
        }
    }
    match got {
        0 => Ok(None),
        4 => Ok(Some(u32::from_le_bytes(b))),
        _ => Err(bad(&format!("{what}: input ends inside a header"))),
    }
}

fn read4(r: &mut dyn Read, what: &str) -> io::Result<u32> {
    read4_or_eof(r, what)?.ok_or_else(|| bad(&format!("{what}: input ends inside a header")))
}

fn read_exact(r: &mut dyn Read, buf: &mut [u8], what: &str) -> io::Result<()> {
    r.read_exact(buf).map_err(|e| match e.kind() {
        io::ErrorKind::UnexpectedEof => bad(&format!("{what}: input ends inside a block")),
        _ => e,
    })
}

// --- delta ----------------------------------------------------------------

/// Largest block accepted. FreeArc's default is 8 MB; anything past this is
/// damage, not a real setting.
const DELTA_MAX: usize = 1 << 30;
/// `MAX_ELEMENT_SIZE` in Delta.cpp.
const MAX_ELEMENT: usize = 30;

/// Each block: data size, table-list size, three u32 arrays (bytes to skip
/// before each table, its type, its row count), then the data. A type's top
/// set bit is the row width N; the bits below it mark immutable columns.
pub fn delta(r: &mut dyn Read, w: &mut dyn Write) -> io::Result<u64> {
    let (mut data, mut tables, mut tmp) = (Vec::new(), Vec::new(), Vec::new());
    let mut total = 0;
    while let Some(size) = read4_or_eof(r, "delta")? {
        let size = size as usize;
        let tsize = read4(r, "delta")? as usize;
        if size > DELTA_MAX || tsize > DELTA_MAX {
            return Err(bad("delta: implausible block size"));
        }
        tables.resize(3 * tsize, 0);
        read_exact(r, &mut tables, "delta")?;
        data.resize(size, 0);
        read_exact(r, &mut data, "delta")?;
        let field = |k: usize, i: usize| {
            let at = k * tsize + 4 * i;
            u32::from_le_bytes(tables[at..at + 4].try_into().unwrap())
        };
        let mut p = 0usize;
        for i in 0..tsize / 4 {
            let (skip, ty, rows) = (field(0, i), field(1, i), field(2, i));
            let mut imm = [false; MAX_ELEMENT];
            let (mut n, mut t) = (0, ty);
            while t > 1 {
                if n == MAX_ELEMENT {
                    return Err(bad("delta: table rows too wide"));
                }
                imm[n] = t & 1 == 1;
                n += 1;
                t >>= 1;
            }
            let len = (rows as usize).checked_mul(n);
            p = p
                .checked_add(skip as usize)
                .filter(|_| skip <= i32::MAX as u32 && rows <= i32::MAX as u32)
                .ok_or_else(|| bad("delta: bad table position"))?;
            let end = len
                .and_then(|l| p.checked_add(l))
                .filter(|&e| e <= size)
                .ok_or_else(|| bad("delta: table runs past its block"))?;
            let t = &mut data[p..end];
            unreorder(t, n, rows as usize, &imm, &mut tmp);
            undiff(t, n, &imm);
            p = end;
        }
        w.write_all(&data)?;
        total += size as u64;
    }
    Ok(total)
}

/// Adds each row to the one before it, bytewise with carries from the lower
/// address, in the columns that were diffed (`undiff_table`).
fn undiff(t: &mut [u8], n: usize, imm: &[bool]) {
    if n == 0 {
        return;
    }
    for r in (n..t.len()).step_by(n) {
        let mut carry = 0u32;
        for i in 0..n {
            if imm[i] {
                carry = 0;
            } else {
                let sum = u32::from(t[r + i]) + u32::from(t[r + i - n]) + carry;
                t[r + i] = sum as u8;
                carry = sum >> 8;
            }
        }
    }
}

/// Immutable columns were stored first, column-major per row group; puts
/// every byte back in its row (`unreorder_table`).
fn unreorder(t: &mut [u8], n: usize, rows: usize, imm: &[bool], tmp: &mut Vec<u8>) {
    let k = imm[..n].iter().filter(|&&x| x).count();
    if k == 0 || k == n {
        return;
    }
    tmp.clear();
    tmp.extend_from_slice(t);
    let (mut q, mut q1, mut p) = (0, k * rows, 0);
    for _ in 0..rows {
        for &is_imm in &imm[..n] {
            let src = if is_imm { &mut q } else { &mut q1 };
            t[p] = tmp[*src];
            *src += 1;
            p += 1;
        }
    }
}

// --- dispack --------------------------------------------------------------

const TAG_DATA: u32 = 0xC71B_3AE1;
const TAG_EXE: u32 = TAG_DATA + 1;

/// The chunk container: a chunk size, then chunks that are raw data (full
/// chunks untagged, others behind `TAG_DATA` + length) or `TAG_EXE`,
/// original size, filtered size and the filtered code. Code is filtered as
/// if loaded at an address that starts at 1 GiB and moves with the output.
pub fn dispack(r: &mut dyn Read, w: &mut dyn Write, block: usize) -> io::Result<u64> {
    let is_tag = |x: u32| (x ^ TAG_DATA) < 0x10;
    let Some(chunk) = read4_or_eof(r, "dispack")? else {
        return Ok(0);
    };
    let chunk = chunk as usize;
    if chunk > block || chunk < 4 {
        return Err(bad("dispack: bad chunk size"));
    }
    let in_max = block + block / 4 + 1024;
    let (mut inb, mut out) = (Vec::new(), Vec::new());
    let mut base: u32 = 1 << 30;
    let mut total = 0u64;
    while let Some(tag) = read4_or_eof(r, "dispack")? {
        if !is_tag(tag) || tag == TAG_DATA {
            let (done, len) = if tag == TAG_DATA {
                (0, read4(r, "dispack")? as usize)
            } else {
                (4, chunk)
            };
            if len > in_max {
                return Err(bad("dispack: data chunk too large"));
            }
            inb.resize(len, 0);
            if done == 4 {
                inb[..4].copy_from_slice(&tag.to_le_bytes());
            }
            read_exact(r, &mut inb[done..], "dispack")?;
            w.write_all(&inb)?;
            base = base.wrapping_add(len as u32);
            total += len as u64;
        } else if tag == TAG_EXE {
            let size = read4(r, "dispack")? as usize;
            let packed = read4(r, "dispack")? as usize;
            if size > block || packed > in_max {
                return Err(bad("dispack: code chunk too large"));
            }
            inb.resize(packed, 0);
            read_exact(r, &mut inb, "dispack")?;
            out.resize(size, 0);
            if !unfilter(&inb, &mut out, base) {
                return Err(bad("dispack: damaged code chunk"));
            }
            w.write_all(&out)?;
            base = base.wrapping_add(size as u32);
            total += size as u64;
        } else {
            return Err(bad("dispack: unknown chunk tag"));
        }
        if base >= 3 << 30 {
            base -= 2 << 30;
        }
    }
    Ok(total)
}

// Streams (`enum Streams`); the aliases share a stream with another field.
const ST_OP: usize = 0;
const ST_SIB: usize = 1;
const ST_CALL_IDX: usize = 2;
const ST_DISP8_R0: usize = 3;
const ST_JUMP8: usize = 11;
const ST_IMM8: usize = 12;
const ST_IMM16: usize = 13;
const ST_IMM32: usize = 14;
const ST_DISP32: usize = 15;
const ST_ADDR32: usize = 16;
const ST_CALL32: usize = 17;
const ST_JUMP32: usize = 18;
const ST_MAX: usize = 19;
const ST_MODRM: usize = ST_OP;
const ST_OP2: usize = ST_OP;
const ST_AJUMP32: usize = ST_JUMP32;
const ST_JUMPTBL_COUNT: usize = ST_OP;

const OP_2BYTE: u8 = 0x0f;
const OP_OSIZE: u8 = 0x66;
const OP_CALLF: u8 = 0x9a;
const OP_RETNI: u8 = 0xc2;
const OP_RETN: u8 = 0xc3;
const OP_ENTER: u8 = 0xc8;
const OP_INT3: u8 = 0xcc;
const OP_CALLN: u8 = 0xe8;
const OP_JMPF: u8 = 0xea;
const ESCAPE: u8 = 0xf1; // ICEBP
const JUMPTAB: u8 = 0xce; // INTO

const F_AM: u8 = 0x1;
const F_MR: u8 = 0x2;
const F_MEXTRA: u8 = 0x3;
const F_MODE: u8 = 0x3;
const F_TYPE: u8 = 0xc;
// With no ModRM: immediate size. In address mode: address kind.
const F_BI: u8 = 0x4;
const F_WI: u8 = 0x8;
const F_DI: u8 = 0xc;
const F_AD: u8 = 0x0;
const F_DA: u8 = 0x4;
const F_BR: u8 = 0x8;
const F_DR: u8 = 0xc;

/// The split streams being read back, each a range of the source.
struct Streams<'a> {
    src: &'a [u8],
    pos: [usize; ST_MAX],
    end: [usize; ST_MAX],
}

impl Streams<'_> {
    fn get(&mut self, st: usize, n: usize) -> Option<&[u8]> {
        let p = self.pos[st];
        if self.end[st] - p < n {
            return None;
        }
        self.pos[st] = p + n;
        Some(&self.src[p..p + n])
    }
    fn u8(&mut self, st: usize) -> Option<u8> {
        self.get(st, 1).map(|b| b[0])
    }
    /// Values over 8 bits are stored big endian.
    fn u32b(&mut self, st: usize) -> Option<u32> {
        self.get(st, 4)
            .map(|b| u32::from_be_bytes(b.try_into().unwrap()))
    }
}

/// `MoveToFront`: shifts `t[0..pos]` up one and puts `val` first.
fn mtf(t: &mut [u32; 256], pos: usize, val: u32) -> u32 {
    t.copy_within(0..pos, 1);
    t[0] = val;
    val
}

/// `DisUnFilter`. Output must fill `dest` exactly.
pub fn unfilter(source: &[u8], dest: &mut [u8], mem_start: u32) -> bool {
    unfilter_inner(source, dest, mem_start) == Some(dest.len())
}

fn unfilter_inner(source: &[u8], dest: &mut [u8], mem_start: u32) -> Option<usize> {
    if source.len() < ST_MAX * 4 {
        return None;
    }
    let mut s = Streams {
        src: source,
        pos: [0; ST_MAX],
        end: [0; ST_MAX],
    };
    let mut cur = ST_MAX * 4;
    for i in 0..ST_MAX {
        s.pos[i] = cur;
        let len = u32::from_le_bytes(source[4 * i..4 * i + 4].try_into().unwrap()) as usize;
        cur = cur.checked_add(len)?;
        s.end[i] = cur;
    }
    if cur != source.len() {
        return None;
    }

    let mut funcs = [0u32; 256];
    let mut next_is_func = true;
    let mut d = 0usize;
    let put = |dest: &mut [u8], d: &mut usize, b: &[u8]| -> Option<()> {
        dest.get_mut(*d..*d + b.len())?.copy_from_slice(b);
        *d += b.len();
        Some(())
    };
    // Copy8/16/32: big endian in the stream, little endian in the code.
    macro_rules! copy8 {
        ($st:expr) => {{
            let v = s.u8($st)?;
            put(dest, &mut d, &[v])?;
            v
        }};
    }
    macro_rules! copy16 {
        ($st:expr) => {{
            let b = s.get($st, 2)?;
            let v = [b[1], b[0]];
            put(dest, &mut d, &v)?;
        }};
    }
    macro_rules! copy32 {
        ($st:expr) => {{
            let v = s.u32b($st)?;
            put(dest, &mut d, &v.to_le_bytes())?;
        }};
    }
    macro_rules! call_target {
        () => {{
            let ind = s.u8(ST_CALL_IDX)? as usize;
            if ind != 0 {
                let v = funcs[ind - 1];
                mtf(&mut funcs, ind - 1, v)
            } else {
                let t = s.u32b(ST_CALL32)?;
                mtf(&mut funcs, 255, t)
            }
        }};
    }

    while s.pos[ST_OP] < s.end[ST_OP] {
        let start = d;
        let memory = mem_start.wrapping_add(d as u32);
        let mut code = s.u8(ST_OP)?;

        if code == JUMPTAB {
            let count = s.u8(ST_JUMPTBL_COUNT)? as usize + 1;
            for _ in 0..count {
                let target = call_target!();
                put(dest, &mut d, &target.to_le_bytes())?;
            }
            continue;
        }

        if next_is_func && code != OP_INT3 {
            mtf(&mut funcs, 255, memory);
            next_is_func = false;
        }

        if code == ESCAPE {
            copy8!(ST_OP);
            continue;
        }

        put(dest, &mut d, &[code])?;
        let mut o16 = false;
        if code == OP_OSIZE {
            o16 = true;
            code = copy8!(ST_OP);
        }
        if code == OP_RETNI || code == OP_RETN || code == OP_INT3 {
            next_is_func = true;
        }
        let mut flags = if code == OP_2BYTE {
            TABLE2[copy8!(ST_OP2) as usize]
        } else {
            TABLE1[code as usize]
        };

        if code == OP_CALLF || code == OP_JMPF || code == OP_ENTER {
            copy16!(ST_IMM16);
        }

        // As in the C code, fERR (0xf) also takes this branch.
        if flags & F_MR != 0 {
            let modrm = copy8!(ST_MODRM);
            let mut sib = 0;
            if flags == F_MEXTRA {
                flags = TABLEX
                    [(((modrm >> 3) & 7) | ((code & 0x01) << 3) | ((code & 0x08) << 1)) as usize];
            }
            if modrm & 0x07 == 4 && modrm < 0xc0 {
                sib = copy8!(ST_SIB);
            }
            if modrm & 0xc0 == 0x40 {
                copy8!((modrm & 0x07) as usize + ST_DISP8_R0);
            }
            if modrm & 0xc0 == 0x80 || modrm & 0xc7 == 0x05 || (modrm < 0x40 && sib & 0x07 == 0x05)
            {
                copy32!(if modrm & 0xc7 == 5 {
                    ST_ADDR32
                } else {
                    ST_DISP32
                });
            }
        }

        if flags & F_MODE == F_AM {
            match flags & F_TYPE {
                F_AD => copy32!(ST_ADDR32),
                F_DA => copy32!(ST_AJUMP32),
                F_BR => {
                    copy8!(ST_JUMP8);
                }
                F_DR => {
                    let target = if code == OP_CALLN {
                        call_target!()
                    } else {
                        s.u32b(ST_JUMP32)?
                    };
                    let rel = target.wrapping_sub(((d - start) as u32 + 4).wrapping_add(memory));
                    put(dest, &mut d, &rel.to_le_bytes())?;
                }
                _ => unreachable!(),
            }
        } else {
            match flags & F_TYPE {
                F_BI => {
                    copy8!(ST_IMM8);
                }
                F_WI => copy16!(ST_IMM16),
                F_DI if o16 => copy16!(ST_IMM16),
                F_DI => copy32!(ST_IMM32),
                _ => {}
            }
        }
    }
    Some(d)
}

/// Generated from DisPack.cpp by evaluating its flag expressions; equal to
/// the tables inside FitGirl's unarc.dll (offsets 0x47610, 0x47510, 0x474f0).
const TABLE1: [u8; 256] = [
    0x2, 0x2, 0x2, 0x2, 0x4, 0xc, 0x0, 0x0, 0x2, 0x2, 0x2, 0x2, 0x4, 0xc, 0x0, 0x0, 0x2, 0x2, 0x2,
    0x2, 0x4, 0xc, 0x0, 0x0, 0x2, 0x2, 0x2, 0x2, 0x4, 0xc, 0x0, 0x0, 0x2, 0x2, 0x2, 0x2, 0x4, 0xc,
    0x0, 0x0, 0x2, 0x2, 0x2, 0x2, 0x4, 0xc, 0x0, 0x0, 0x2, 0x2, 0x2, 0x2, 0x4, 0xc, 0x0, 0x0, 0x2,
    0x2, 0x2, 0x2, 0x4, 0xc, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
    0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
    0x0, 0x0, 0x0, 0x2, 0x2, 0x0, 0x0, 0x0, 0x0, 0xc, 0xe, 0x4, 0x6, 0x0, 0x0, 0x0, 0x0, 0x9, 0x9,
    0x9, 0x9, 0x9, 0x9, 0x9, 0x9, 0x9, 0x9, 0x9, 0x9, 0x9, 0x9, 0x9, 0x9, 0x6, 0xe, 0x6, 0x6, 0x2,
    0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
    0x0, 0x0, 0x5, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0, 0x4, 0xc, 0x0,
    0x0, 0x0, 0x0, 0x0, 0x0, 0x4, 0x4, 0x4, 0x4, 0x4, 0x4, 0x4, 0x4, 0xc, 0xc, 0xc, 0xc, 0xc, 0xc,
    0xc, 0xc, 0x6, 0x6, 0x8, 0x0, 0x2, 0x2, 0x6, 0xe, 0x4, 0x0, 0x8, 0x0, 0x0, 0x4, 0xf, 0x0, 0x2,
    0x2, 0x2, 0x2, 0x4, 0x4, 0x0, 0x0, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x9, 0x9, 0x9, 0x9,
    0x4, 0x4, 0x4, 0x4, 0xd, 0xd, 0x1, 0x9, 0x0, 0x0, 0x0, 0x0, 0x0, 0xf, 0x0, 0x0, 0x0, 0x0, 0x3,
    0x3, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x3, 0x3,
];
const TABLE2: [u8; 256] = [
    0xf, 0xf, 0xf, 0xf, 0xf, 0xf, 0x0, 0xf, 0x0, 0x0, 0xf, 0xf, 0xf, 0xf, 0xf, 0xf, 0x2, 0x2, 0x2,
    0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0xf, 0xf, 0xf, 0xf, 0xf, 0xf, 0xf, 0x2, 0x2, 0x2, 0x2, 0xf, 0xf,
    0xf, 0xf, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0xf, 0x0, 0xf,
    0xf, 0xf, 0xf, 0xf, 0xf, 0xf, 0xf, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2,
    0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2,
    0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x6, 0x6,
    0x6, 0x6, 0x2, 0x2, 0x2, 0x0, 0xf, 0xf, 0xf, 0xf, 0xf, 0xf, 0x2, 0x2, 0xd, 0xd, 0xd, 0xd, 0xd,
    0xd, 0xd, 0xd, 0xd, 0xd, 0xd, 0xd, 0xd, 0xd, 0xd, 0xd, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2,
    0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x0, 0x0, 0x0, 0x2, 0x6, 0x2, 0x2, 0x2, 0xf, 0xf, 0xf,
    0x2, 0x6, 0x2, 0xf, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0xf, 0xf, 0xf, 0x2, 0x2, 0x2,
    0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x2,
    0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2,
    0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2,
    0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0xf,
];
const TABLEX: [u8; 32] = [
    0x6, 0xf, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0xe, 0xf, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0xf,
    0xf, 0xf, 0xf, 0xf, 0xf, 0x2, 0x2, 0x2, 0xf, 0x2, 0xf, 0x2, 0xf,
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Forward transforms as in Delta.cpp (`diff_table`, `reorder_table`),
    /// only here to build test input.
    #[allow(clippy::needless_range_loop)] // indexed like the C it mirrors
    fn diff(t: &mut [u8], n: usize, imm: &[bool]) {
        let rows = t.len() / n;
        for r in (1..rows).rev() {
            let mut carry = 0i32;
            for i in 0..n {
                let (a, b) = (r * n + i, (r - 1) * n + i);
                if imm[i] {
                    carry = 0;
                } else {
                    let sub = i32::from(t[b]) + carry;
                    let newcarry = i32::from(i32::from(t[a]) < sub);
                    t[a] = (i32::from(t[a]) - sub) as u8;
                    carry = newcarry;
                }
            }
        }
    }
    fn reorder(t: &mut [u8], n: usize, imm: &[bool]) {
        let tmp = t.to_vec();
        let mut p = 0;
        for want in [true, false] {
            for (k, &b) in tmp.iter().enumerate() {
                if imm[k % n] == want {
                    t[p] = b;
                    p += 1;
                }
            }
        }
    }

    #[test]
    fn delta_round_trip() {
        // 3 bytes of junk, a table of 5 rows of 4 bytes (u16 counter with a
        // carry, one immutable byte, one diffed byte), 2 bytes of junk.
        let rows: Vec<[u8; 4]> = (0..5u16)
            .map(|i| {
                let v = 0x00f0 + i * 0x20;
                [v as u8, (v >> 8) as u8, 0xAA, (7 * i) as u8]
            })
            .collect();
        let plain: Vec<u8> = [&[1u8, 2, 3][..], &rows.concat(), &[9, 9]].concat();
        let imm = [false, false, true, false];
        let mut packed_tab = rows.concat();
        diff(&mut packed_tab, 4, &imm);
        reorder(&mut packed_tab, 4, &imm);
        let packed_data: Vec<u8> = [&[1u8, 2, 3][..], &packed_tab, &[9, 9]].concat();
        let ty: u32 = (1 << 4) | (1 << 2); // N=4, column 2 immutable
        let mut s = Vec::new();
        s.extend((packed_data.len() as u32).to_le_bytes());
        s.extend(4u32.to_le_bytes());
        s.extend(3u32.to_le_bytes()); // skip
        s.extend(ty.to_le_bytes());
        s.extend(5u32.to_le_bytes()); // rows
        s.extend(&packed_data);
        // A second block with no tables.
        s.extend(2u32.to_le_bytes());
        s.extend(0u32.to_le_bytes());
        s.extend([7, 8]);
        let mut out = Vec::new();
        assert_eq!(
            delta(&mut &s[..], &mut out).unwrap(),
            plain.len() as u64 + 2
        );
        assert_eq!(out, [&plain[..], &[7, 8]].concat());

        // A table that claims more rows than its block holds is refused.
        let mut bad = s[..8].to_vec();
        bad.extend(3u32.to_le_bytes());
        bad.extend(ty.to_le_bytes());
        bad.extend(99u32.to_le_bytes());
        bad.extend(&packed_data);
        assert!(delta(&mut &bad[..], &mut Vec::new()).is_err());
        assert!(delta(&mut &s[..s.len() - 1], &mut Vec::new()).is_err());
    }

    /// Hand-built DisFilter output: streams for
    /// `push ebp; call +0xFA (via CALL32); call same (via the MTF table);
    /// mov ax,0x1234; <escaped 0xD6>; ret`.
    fn filtered() -> (Vec<u8>, Vec<u8>) {
        let base = 1u32 << 30;
        let mut st: Vec<Vec<u8>> = vec![Vec::new(); ST_MAX];
        st[ST_OP] = vec![0x55, 0xE8, 0xE8, 0x66, 0xB8, ESCAPE, 0xD6, 0xC3];
        st[ST_CALL_IDX] = vec![0, 1];
        st[ST_CALL32] = (base + 0x100).to_be_bytes().to_vec();
        st[ST_IMM16] = vec![0x12, 0x34];
        let mut src = Vec::new();
        for s in &st {
            src.extend((s.len() as u32).to_le_bytes());
        }
        for s in &st {
            src.extend(s);
        }
        // rel = target - (address after the 4-byte operand)
        let plain = vec![
            0x55, 0xE8, 0xFA, 0x00, 0x00, 0x00, 0xE8, 0xF5, 0x00, 0x00, 0x00, 0x66, 0xB8, 0x34,
            0x12, 0xD6, 0xC3,
        ];
        (src, plain)
    }

    #[test]
    fn unfilter_decodes_calls_mtf_escape_and_imm16() {
        let (src, plain) = filtered();
        let mut out = vec![0u8; plain.len()];
        assert!(unfilter(&src, &mut out, 1 << 30));
        assert_eq!(out, plain);
        // Wrong output size, or a cut stream, is refused.
        assert!(!unfilter(&src, &mut vec![0u8; plain.len() + 1], 1 << 30));
        assert!(!unfilter(&src[..src.len() - 1], &mut out, 1 << 30));
    }

    #[test]
    fn dispack_container() {
        let (code, plain) = filtered();
        let raw = *b"ABCDEFGH"; // an untagged full chunk (chunk size 8)
        let mut s = Vec::new();
        s.extend(8u32.to_le_bytes());
        s.extend(TAG_EXE.to_le_bytes());
        s.extend((plain.len() as u32).to_le_bytes());
        s.extend((code.len() as u32).to_le_bytes());
        s.extend(&code);
        s.extend(raw);
        s.extend(TAG_DATA.to_le_bytes());
        s.extend(3u32.to_le_bytes());
        s.extend(b"xyz");
        let mut out = Vec::new();
        let n = dispack(&mut &s[..], &mut out, 8 << 20).unwrap();
        let want = [&plain[..], &raw, b"xyz"].concat();
        assert_eq!((n, out), (want.len() as u64, want));
        assert_eq!(dispack(&mut &[][..], &mut Vec::new(), 8 << 20).unwrap(), 0);
        let mut bad = s.clone();
        bad[4] ^= 0x0c; // tag 0xC71B3AEE: in the tag range, but not one FreeArc uses
        assert!(dispack(&mut &bad[..], &mut Vec::new(), 8 << 20).is_err());
    }
}
