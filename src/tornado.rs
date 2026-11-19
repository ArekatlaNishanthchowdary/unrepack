//! FreeArc's `tor` (Tornado) decoder, ported from the FreeArc 0.67 source
//! (github.com/mrkaban/FreeArc-0.67, `Compression/Tornado`): `tor_decompress`
//! in `Tornado.cpp`, the LZ77 decoders in `LZ77_Coder.cpp`, the bit stream,
//! Huffman and range decoders in `EntropyCoder.cpp`, and the table undiffing
//! in `DataTables.cpp`. (c) Bulat Ziganshin and Joachim Henke, under the GPL.
//!
//! Licence: not MIT OR Apache-2.0 like the rest of unrepack; see LICENSE-FREEARC.
//!
//! The stream carries its own header: the coder (1 byte, 2 bit, 3 Huffman,
//! 4 arithmetic), the minimum match length, and the dictionary size (u32
//! LE). Literals and matches follow, up to an end code. The Huffman and
//! arithmetic coders can also mark "data tables": runs of 2- or 4-byte
//! little-endian numbers, each stored as its difference from the one before.
//!
//! Unlike the C, the window keeps the bytes as decoded (matches copy the
//! differences too), and the tables are summed up again only in the copy
//! that is written out. Where the C trusts the stream, this refuses it.

use std::io::{self, Read, Write};

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("tornado: {what}"))
}

/// The C's `IMPOSSIBLE_LEN` and `IMPOSSIBLE_DIST`: `INT_MAX/2`.
const IMPOSSIBLE: u32 = 0x3fff_ffff;
/// The smallest window the C uses, and its largest write.
const LARGE_BUFFER: usize = 256 << 10;
const HUGE_BUFFER: usize = 8 << 20;
/// FreeArc's presets stop at 1 GB; anything past 2 GB is damage.
const MAX_DICT: u32 = 1 << 31;
/// How far the coders may read past the end of the data: they look a few
/// bytes ahead, but a stream that never reaches its end code is damaged.
const MAX_OVERRUN: u32 = 64;

/// Extra bits per length code (bit coder; Huffman and arithmetic), and per
/// distance code.
const LC_BITS: [u32; 8] = [0, 0, 0, 1, 2, 4, 8, 30];
const LC2_BITS: [u32; 16] = [0, 0, 0, 0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 8, 30];
const DC_BITS: [u32; 32] = [
    4, 4, 5, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13, 14, 14, 15, 15, 16, 17,
    18, 19, 21, 23, 30,
];
const LC_BASE: [u32; 8] = bases(LC_BITS);
const LC2_BASE: [u32; 16] = bases(LC2_BITS);
const DC_BASE: [u32; 32] = dist_bases();

/// `VLE::VLE`: each code's first value.
const fn bases<const N: usize>(bits: [u32; N]) -> [u32; N] {
    let mut b = [0; N];
    let (mut v, mut i) = (0u32, 0);
    while i < N {
        b[i] = v;
        v += 1 << bits[i];
        i += 1;
    }
    b
}

/// `DistanceCoder::DistanceCoder`: steps of 1 below 512, of 256 below 128 K,
/// then of 64 K, with its table-size cut-off kept.
const fn dist_bases() -> [u32; 32] {
    let mut b = [0; 32];
    let (mut dist, mut code) = (0u32, 0);
    while dist < 512 {
        b[code] = dist;
        dist += 1 << DC_BITS[code];
        code += 1;
    }
    dist >>= 8;
    while dist < 512 {
        b[code] = dist << 8;
        dist += 1 << (DC_BITS[code] - 8);
        code += 1;
    }
    dist >>= 8;
    while code < 32 {
        b[code] = dist << 16;
        let mut n = 0;
        while n < 1u32 << (DC_BITS[code] - 16) && 1024 + dist < 1024 + 16384 + 1 {
            dist += 1;
            n += 1;
        }
        code += 1;
    }
    b
}

/// Number of length and distance codes, and the codes after them.
const LEN_CODES: u32 = 16;
const DIST_CODES: u32 = 32 + 4;
const EOB: u32 = 256 + LEN_CODES * DIST_CODES;
const REPCHAR: u32 = EOB + 1;
const CODES: usize = EOB as usize + 10;

struct Input<'a> {
    r: &'a mut dyn Read,
    buf: Box<[u8]>,
    pos: usize,
    len: usize,
    eof: bool,
    over: u32,
}

impl<'a> Input<'a> {
    fn new(r: &'a mut dyn Read) -> Self {
        Input {
            r,
            buf: vec![0; 1 << 16].into_boxed_slice(),
            pos: 0,
            len: 0,
            eof: false,
            over: 0,
        }
    }

    /// The next byte; past the end, zeros (the C reads stale buffer bytes
    /// there, which never reach the output of an undamaged stream).
    fn byte(&mut self) -> io::Result<u32> {
        if self.pos == self.len {
            while !self.eof {
                match self.r.read(&mut self.buf) {
                    Ok(0) => self.eof = true,
                    Ok(n) => {
                        (self.pos, self.len) = (0, n);
                        break;
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            if self.pos == self.len {
                self.over += 1;
                if self.over > MAX_OVERRUN {
                    return Err(bad("the data ends before its end code"));
                }
                return Ok(0);
            }
        }
        self.pos += 1;
        Ok(self.buf[self.pos - 1] as u32)
    }

    /// A little-endian value of `n` bytes.
    fn le(&mut self, n: u32) -> io::Result<u32> {
        let mut v = 0;
        for i in 0..n {
            v |= self.byte()? << (8 * i);
        }
        Ok(v)
    }
}

/// `InputBitStream`: bits taken from the low end of little-endian u32s.
struct Bits<'a> {
    i: Input<'a>,
    buf: u64,
    count: u32,
}

impl<'a> Bits<'a> {
    fn new(i: Input<'a>) -> Self {
        Bits {
            i,
            buf: 0,
            count: 0,
        }
    }

    /// The next `n` (<= 32) bits, without using them up.
    fn need(&mut self, n: u32) -> io::Result<u32> {
        if self.count <= 32 {
            self.buf |= (self.i.le(4)? as u64) << self.count;
            self.count += 32;
        }
        Ok((self.buf & ((1u64 << n) - 1)) as u32)
    }

    fn dump(&mut self, n: u32) {
        self.buf >>= n;
        self.count -= n;
    }

    fn get(&mut self, n: u32) -> io::Result<u32> {
        let x = self.need(n)?;
        self.dump(n);
        Ok(x)
    }
}

enum Op {
    Lit(u8),
    /// Length and distance. A length of `IMPOSSIBLE` or more is the end
    /// code (with distance `IMPOSSIBLE`) or a data table: row size
    /// `len - IMPOSSIBLE`, `dist` rows.
    Match(u32, u32),
}

trait Coder {
    fn next(&mut self, minlen: u32) -> io::Result<Op>;
}

/// `LZ77_ByteDecoder`: 2-bit flags, 16 to a u32, say literal or match of 2,
/// 3 or 4+ bytes.
struct ByteCoder<'a> {
    i: Input<'a>,
    flags: u32,
    flagpos: u32,
}

impl Coder for ByteCoder<'_> {
    fn next(&mut self, minlen: u32) -> io::Result<Op> {
        self.flagpos -= 1;
        if self.flagpos != 0 {
            self.flags >>= 2;
        } else {
            self.flagpos = 16;
            self.flags = self.i.le(4)?;
        }
        let i = &mut self.i;
        let (len, dist) = match self.flags & 3 {
            0 => return Ok(Op::Lit(i.byte()? as u8)),
            1 => {
                let x = i.le(2)?;
                (x >> 12, x & 0xfff)
            }
            2 => {
                let x = i.le(3)?;
                (x >> 18, x & 0x3ffff)
            }
            _ => {
                let mut len = i.byte()?;
                let mut dist = 0;
                if len == 255 {
                    dist = i.byte()? << 24;
                    len = i.byte()?;
                }
                if len == 254 {
                    len = i.le(3)? << 8;
                    len = len.wrapping_add(i.byte()?);
                }
                (len, dist.wrapping_add(i.le(3)?))
            }
        };
        Ok(Op::Match(minlen.wrapping_add(len), dist))
    }
}

/// `LZ77_BitDecoder`: 9-bit codes; below 256 a literal, else a length code
/// (3 bits) and a distance code (5 bits), each followed by its extra bits.
struct BitCoder<'a>(Bits<'a>);

impl Coder for BitCoder<'_> {
    fn next(&mut self, minlen: u32) -> io::Result<Op> {
        let x = self.0.get(9)?;
        if x < 256 {
            return Ok(Op::Lit(x as u8));
        }
        let l = ((x >> 5) - 8) as usize;
        let len = minlen
            .wrapping_add(LC_BASE[l])
            .wrapping_add(self.0.get(LC_BITS[l])?);
        let d = (x & 31) as usize;
        let dist = DC_BASE[d].wrapping_add(self.0.get(DC_BITS[d])?);
        Ok(Op::Match(len, dist))
    }
}

/// What the Huffman and arithmetic decoders share.
trait Symbols {
    fn decode(&mut self) -> io::Result<u32>;
    fn getbits(&mut self, n: u32) -> io::Result<u32>;
}

/// `LZ77_Decoder`: one symbol per literal or match (length code and
/// distance code together, the first four distance codes repeating recent
/// distances), plus REPCHAR (one byte from the last distance).
struct Lz<S> {
    s: S,
    /// Recent distances, newest first.
    prev: [u32; 4],
}

impl<S: Symbols> Coder for Lz<S> {
    fn next(&mut self, minlen: u32) -> io::Result<Op> {
        let x = self.s.decode()?;
        if x < 256 {
            return Ok(Op::Lit(x as u8));
        }
        if x == REPCHAR {
            return Ok(Op::Match(1, self.prev[0]));
        }
        if x >= EOB {
            return Err(bad("unknown code"));
        }
        let l = (x % LEN_CODES) as usize;
        let len = LC2_BASE[l].wrapping_add(self.s.getbits(LC2_BITS[l])?);
        // Lengths 101..104 mark tables of 1..4-byte rows; longer ones are 4 up.
        let len = match len {
            0..=100 => len + minlen,
            101..=104 => len - 100 + IMPOSSIBLE,
            _ => (len - 4).wrapping_add(minlen),
        };
        let p = &mut self.prev;
        let dist = match (x - 256) / LEN_CODES {
            0 => p[0],
            1 => {
                p.swap(0, 1);
                p[0]
            }
            2 => {
                p[..3].rotate_right(1);
                p[0]
            }
            3 => {
                p.rotate_right(1);
                p[0]
            }
            d => {
                let d = d as usize - 4;
                let dist = DC_BASE[d]
                    .wrapping_add(self.s.getbits(DC_BITS[d])?)
                    .wrapping_add(1);
                p.rotate_right(1);
                p[0] = dist;
                dist
            }
        };
        Ok(Op::Match(len, dist))
    }
}

const FAST_BITS: u32 = 11;
/// The C allows any depth; real trees stay near 22 bits.
const MAX_HUF_BITS: u32 = 26;

/// `HuffmanDecoder` with its `HuffmanTree`: symbol counts rebuild the code
/// at every EOB symbol, which carries 3 bits saying how to age the counts.
struct Huffman<'a> {
    b: Bits<'a>,
    counter: Vec<u32>,
    bits: Vec<u8>,
    maxbits: u32,
    /// Codes of up to FAST_BITS bits, by their low FAST_BITS bits; -1 sends
    /// longer codes to `index`, by their low `maxbits` bits.
    fast: Vec<i32>,
    index: Vec<u16>,
}

impl<'a> Huffman<'a> {
    fn new(b: Bits<'a>, n: usize) -> io::Result<Self> {
        let mut h = Huffman {
            b,
            counter: vec![1; n],
            bits: vec![0; n],
            maxbits: 0,
            fast: vec![0; 1 << FAST_BITS],
            index: Vec::new(),
        };
        h.build(0)?;
        Ok(h)
    }

    /// `HuffmanTree::build_tree`, decoder side.
    fn build(&mut self, mode: u32) -> io::Result<()> {
        const FENCE: u32 = i32::MAX as u32;
        let b = self.counter.len();
        // Leaves sorted by count, ties by symbol (the C's stable sort).
        let mut order: Vec<usize> = (0..b).collect();
        order.sort_by_key(|&s| (self.counter[s], s));
        let size = 2 * b + 4;
        let mut cnt = vec![FENCE; size];
        let (mut left, mut right) = (vec![0usize; size], vec![0usize; size]);
        let (mut nbits, mut code) = (vec![0u32; size], vec![0u32; size]);
        for (i, &s) in order.iter().enumerate() {
            cnt[i] = self.counter[s];
            left[i] = s;
        }
        // Leaves come from p1, joined nodes from p2 and are made at p3.
        let (mut p1, mut p2, mut p3) = (0, b + 2, b + 2);
        while !(p1 == b && p3 - p2 == 1) {
            let (l, r) = if cnt[p1 + 1] < cnt[p2] {
                p1 += 2;
                (p1 - 2, p1 - 1)
            } else if cnt[p1] > cnt[p2 + 1] {
                p2 += 2;
                (p2 - 2, p2 - 1)
            } else {
                p1 += 1;
                p2 += 1;
                (p1 - 1, p2 - 1)
            };
            cnt[p3] = cnt[l].wrapping_add(cnt[r]);
            (left[p3], right[p3]) = (l, r);
            p3 += 1;
        }
        for i in (b + 2..=p2).rev() {
            let (l, r, nb) = (left[i], right[i], nbits[i]);
            if nb >= 31 {
                return Err(bad("Huffman code too long"));
            }
            (nbits[l], code[l]) = (nb + 1, code[i]);
            (nbits[r], code[r]) = (nb + 1, code[i] + (1 << nb));
        }
        let maxbits = nbits[..b].iter().copied().max().unwrap_or(0);
        if maxbits > MAX_HUF_BITS {
            return Err(bad("Huffman code too long"));
        }
        self.maxbits = maxbits;
        if maxbits > FAST_BITS && self.index.len() < 1 << maxbits {
            self.index.resize(1 << maxbits, 0);
        }
        for i in 0..b {
            let (s, sb, sc) = (left[i], nbits[i], code[i] as usize);
            self.bits[s] = sb as u8;
            if sb <= FAST_BITS {
                for j in 0..1usize << (FAST_BITS - sb) {
                    self.fast[sc + (j << sb)] = s as i32;
                }
            } else {
                self.fast[sc & ((1 << FAST_BITS) - 1)] = -1;
                for j in 0..1usize << (maxbits - sb) {
                    self.index[sc + (j << sb)] = s as u16;
                }
            }
        }
        let f = [2, 3, 4, 6, 8, 10, 12, 16][mode as usize];
        for c in &mut self.counter {
            *c -= if *c > 1 && *c < f { 1 } else { *c / f };
        }
        Ok(())
    }
}

impl Symbols for Huffman<'_> {
    fn decode(&mut self) -> io::Result<u32> {
        loop {
            let c = self.b.need(self.maxbits)? as usize;
            let x = match self.fast[c & ((1 << FAST_BITS) - 1)] {
                -1 => *self.index.get(c).ok_or_else(|| bad("damaged data"))? as usize,
                s => s as usize,
            };
            self.b.dump(self.bits[x] as u32);
            if x as u32 != EOB {
                self.counter[x] += 1;
                return Ok(x as u32);
            }
            let mode = self.b.get(3)?;
            self.build(mode)?;
        }
    }

    fn getbits(&mut self, n: u32) -> io::Result<u32> {
        self.b.get(n)
    }
}

const RANGE_BITS: u32 = 14;
const RANGE: u32 = 1 << RANGE_BITS;
const INDEXES: usize = 2048;

/// `ArithDecoder`: Schindler's range decoder with `TCounter`, whose counts
/// are refreshed every RANGE symbols.
struct Arith<'a> {
    i: Input<'a>,
    range: u32,
    buffer: u32,
    remainder: u32,
    cnt: Vec<u32>,
    cum: Vec<u32>,
    live: Vec<u32>,
    /// First symbol whose range reaches each RANGE/INDEXES step.
    index: Vec<u16>,
}

impl<'a> Arith<'a> {
    fn new(mut i: Input<'a>, n: usize) -> io::Result<Self> {
        let mut buffer = 0u32;
        for _ in 0..5 {
            buffer = (buffer << 8) + i.byte()?;
        }
        let r = RANGE as usize;
        let mut a = Arith {
            i,
            range: u32::MAX,
            buffer,
            remainder: 0,
            cnt: vec![0; n],
            cum: vec![0; n],
            live: (0..n)
                .map(|s| (r / n + usize::from(s < r % n)) as u32)
                .collect(),
            index: vec![0; INDEXES],
        };
        a.rescale();
        Ok(a)
    }

    fn rescale(&mut self) {
        let (mut total, mut ind) = (0u32, 0);
        self.remainder = RANGE;
        for s in 0..self.cnt.len() {
            let l = self.live[s];
            (self.cnt[s], self.cum[s]) = (l, total);
            total += l;
            self.live[s] -= if l > 1 && l < 6 { 1 } else { l / 6 };
            self.remainder = self.remainder.wrapping_sub(self.live[s]);
            while ind < INDEXES && total > (RANGE / INDEXES as u32) * ind as u32 {
                self.index[ind] = s as u16;
                ind += 1;
            }
        }
    }

    fn count(&mut self, bits: u32) -> io::Result<u32> {
        self.range >>= bits;
        if self.range == 0 {
            return Err(bad("damaged data"));
        }
        let c = self.buffer / self.range;
        if c >= 1 << bits {
            return Err(bad("damaged data"));
        }
        Ok(c)
    }

    fn update(&mut self, cum: u32, cnt: u32) -> io::Result<()> {
        self.buffer = self.buffer.wrapping_sub(cum.wrapping_mul(self.range));
        self.range = self.range.wrapping_mul(cnt);
        if self.range == 0 {
            return Err(bad("damaged data"));
        }
        while self.range < 1 << 24 {
            self.range <<= 8;
            self.buffer = (self.buffer << 8) + self.i.byte()?;
        }
        Ok(())
    }
}

impl Symbols for Arith<'_> {
    fn decode(&mut self) -> io::Result<u32> {
        let c = self.count(RANGE_BITS)?;
        let mut s = self.index[(c / (RANGE / INDEXES as u32)) as usize] as usize;
        while self.cum[s] + self.cnt[s] - 1 < c {
            s += 1;
            if s == self.cnt.len() {
                return Err(bad("damaged data"));
            }
        }
        self.update(self.cum[s], self.cnt[s])?;
        self.live[s] += 1;
        self.remainder = self.remainder.wrapping_sub(1);
        if self.remainder == 0 {
            self.rescale();
        }
        Ok(s as u32)
    }

    fn getbits(&mut self, n: u32) -> io::Result<u32> {
        if n <= 24 {
            let x = self.count(n)?;
            self.update(x, 1)?;
            return Ok(x);
        }
        let lo = self.count(15)?;
        self.update(lo, 1)?;
        let hi = self.count(n - 15)?;
        self.update(hi, 1)?;
        Ok((hi << 15) + lo)
    }
}

/// A data table still being written: rows of `n` bytes from `start`.
struct Table {
    start: u64,
    n: usize,
    rows: u64,
    /// The first row not yet written in full, and the row before it as
    /// written (summed up).
    next: u64,
    prev: [u8; 4],
    /// A row cut by a write: its first `got` bytes, summed, and the carry
    /// out of them. The ring may have moved on by the next write.
    row: [u8; 4],
    got: usize,
    carry: u16,
}

/// The window, as a ring of at least LARGE_BUFFER bytes, and what of it is
/// written out.
struct Window<'a> {
    ring: Vec<u8>,
    at: usize,
    pos: u64,
    done: u64,
    /// Where the next write happens: every `chunk` bytes and at the ring's end.
    limit: u64,
    chunk: u64,
    tables: Vec<Table>,
    found: u32,
    out: &'a mut dyn Write,
    buf: Vec<u8>,
}

impl Window<'_> {
    fn put(&mut self, b: u8) -> io::Result<()> {
        self.ring[self.at] = b;
        self.at += 1;
        self.pos += 1;
        if self.pos == self.limit {
            self.flush()?;
        }
        Ok(())
    }

    fn copy(&mut self, len: u32, dist: u32) -> io::Result<()> {
        let size = self.ring.len();
        let d = dist as usize;
        if len == 0 || d == 0 || d > size || dist as u64 > self.pos {
            return Err(bad("a match reaches outside the data"));
        }
        let mut from = if self.at >= d {
            self.at - d
        } else {
            self.at + size - d
        };
        for _ in 0..len {
            let b = self.ring[from];
            from += 1;
            if from == size {
                from = 0;
            }
            self.put(b)?;
        }
        Ok(())
    }

    fn table(&mut self, n: u32, rows: u32, dict: u32) -> io::Result<()> {
        if !(1..=4).contains(&n) || rows as u64 * n as u64 > 2 * dict as u64 {
            return Err(bad("a data table is malformed"));
        }
        if let Some(t) = self.tables.last()
            && self.pos < t.start + t.rows * t.n as u64
        {
            return Err(bad("data tables overlap"));
        }
        self.tables.push(Table {
            start: self.pos,
            n: n as usize,
            rows: rows as u64,
            next: 0,
            prev: [0; 4],
            row: [0; 4],
            got: 0,
            carry: 0,
        });
        self.found += 1;
        Ok(())
    }

    /// Writes out what was decoded since the last write, with the tables
    /// in it summed up: each row is itself plus the row before it, as one
    /// little-endian number of `n` bytes.
    fn flush(&mut self) -> io::Result<()> {
        let size = self.ring.len() as u64;
        let (ws, we) = (self.done, self.pos);
        let s = (ws % size) as usize;
        self.buf.clear();
        self.buf
            .extend_from_slice(&self.ring[s..s + (we - ws) as usize]);
        for t in &mut self.tables {
            while t.next < t.rows {
                let es = t.start + t.next * t.n as u64;
                if es >= we {
                    break;
                }
                // Bytes before `got` went out with the last write, so the
                // rest of the row starts at `ws`.
                let have = ((we - es) as usize).min(t.n);
                for k in t.got..have {
                    let at = (es + k as u64 - ws) as usize;
                    t.row[k] = if t.next == 0 {
                        self.buf[at]
                    } else {
                        let v = self.buf[at] as u16 + t.prev[k] as u16 + t.carry;
                        t.carry = v >> 8;
                        v as u8
                    };
                    self.buf[at] = t.row[k];
                }
                if have < t.n {
                    t.got = have;
                    break;
                }
                (t.prev, t.next, t.got, t.carry) = (t.row, t.next + 1, 0, 0);
            }
        }
        self.tables.retain(|t| t.next < t.rows);
        self.out.write_all(&self.buf)?;
        self.done = we;
        if self.at == self.ring.len() {
            self.at = 0;
        }
        self.limit = we + self.chunk.min(size - we % size);
        Ok(())
    }
}

/// Decodes one `tor` stream from `input` to `out`, then reads what's left
/// of `input` so the stage feeding it isn't cut off. Returns the bytes
/// written.
pub fn decode(input: &mut dyn Read, out: &mut dyn Write) -> io::Result<u64> {
    decode_with(input, out, HUGE_BUFFER as u64).map(|(n, _)| n)
}

/// [`decode`] writing every `chunk` bytes; also returns the tables found.
fn decode_with(input: &mut dyn Read, out: &mut dyn Write, chunk: u64) -> io::Result<(u64, u32)> {
    let res = {
        let mut i = Input::new(&mut *input);
        let coder = i.byte()?;
        let minlen = i.byte()?;
        let dict = i.le(4)?;
        if i.over > 0 {
            return Err(bad("the data ends inside its header"));
        }
        if dict == 0 || dict > MAX_DICT {
            return Err(bad(&format!("dictionary size {dict} is out of range")));
        }
        let mut ring = Vec::new();
        let size = (dict as usize).max(LARGE_BUFFER);
        ring.try_reserve_exact(size)
            .map_err(|_| bad("not enough memory for the dictionary"))?;
        ring.resize(size, 0);
        let w = Window {
            ring,
            at: 0,
            pos: 0,
            done: 0,
            limit: chunk.min(size as u64),
            chunk,
            tables: Vec::new(),
            found: 0,
            out,
            buf: Vec::new(),
        };
        match coder {
            1 => run(
                ByteCoder {
                    i,
                    flags: 0,
                    flagpos: 1,
                },
                w,
                minlen,
                dict,
            ),
            2 => run(BitCoder(Bits::new(i)), w, minlen, dict),
            3 => run(
                Lz {
                    s: Huffman::new(Bits::new(i), CODES)?,
                    prev: [0; 4],
                },
                w,
                minlen,
                dict,
            ),
            4 => run(
                Lz {
                    s: Arith::new(i, CODES)?,
                    prev: [0; 4],
                },
                w,
                minlen,
                dict,
            ),
            c => Err(bad(&format!("unknown coder {c}"))),
        }
    }?;
    io::copy(input, &mut io::sink())?;
    Ok(res)
}

/// `tor_decompress0`: literals and matches into the window up to the end
/// code.
fn run(mut c: impl Coder, mut w: Window<'_>, minlen: u32, dict: u32) -> io::Result<(u64, u32)> {
    loop {
        match c.next(minlen)? {
            Op::Lit(b) => w.put(b)?,
            Op::Match(len, dist) if len < IMPOSSIBLE => w.copy(len, dist)?,
            Op::Match(IMPOSSIBLE, IMPOSSIBLE) => break,
            Op::Match(len, rows) => w.table(len - IMPOSSIBLE, rows, dict)?,
        }
    }
    w.flush()?;
    Ok((w.pos, w.found))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Made by FreeArc 0.67's own tor.exe from testdata/gen.py's output
    // (see testdata/README.md): its length and CRC-32.
    const BIG: (usize, u32) = (656986, 0x5932f5a6);
    const SMALL: (usize, u32) = (123616, 0x65d817f5);

    /// A writer that refuses to grow past `max`, so damaged data can't run
    /// a test out of memory.
    struct Capped(Vec<u8>, usize);
    impl Write for Capped {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            if self.0.len() + b.len() > self.1 {
                return Err(io::Error::other("too much output"));
            }
            self.0.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn check(packed: &[u8], want: (usize, u32), chunk: u64) -> u32 {
        let mut out = Capped(Vec::new(), want.0);
        let (n, tables) = decode_with(&mut &packed[..], &mut out, chunk).unwrap();
        assert_eq!((n as usize, out.0.len()), (want.0, want.0));
        assert_eq!(crc32fast::hash(&out.0), want.1);
        tables
    }

    #[test]
    fn bit_tables() {
        assert_eq!(LC_BASE, [0, 1, 2, 3, 5, 9, 25, 281]);
        assert_eq!(LC2_BASE[7..], [7, 9, 11, 15, 19, 27, 35, 51, 307]);
        assert_eq!(DC_BASE[..10], [0, 16, 32, 64, 96, 128, 192, 256, 384, 512]);
        assert_eq!(
            DC_BASE[24..],
            [
                98304, 131072, 196608, 327680, 589824, 1114112, 3211264, 11599872
            ]
        );
        assert_eq!((EOB, CODES), (832, 842));
    }

    #[test]
    fn every_coder() {
        for f in [
            &include_bytes!("../testdata/tor-c1.bin")[..],
            include_bytes!("../testdata/tor-c2.bin"),
            include_bytes!("../testdata/tor-c3t0.bin"),
            include_bytes!("../testdata/tor-c4t0.bin"),
            include_bytes!("../testdata/tor-c4opt.bin"),
        ] {
            check(f, SMALL, HUGE_BUFFER as u64);
        }
    }

    /// Larger than the 256 KB ring, with tables, written in pieces that
    /// cut rows apart.
    #[test]
    fn tables_and_wraparound() {
        for f in [
            &include_bytes!("../testdata/tor-c3t1.bin")[..],
            include_bytes!("../testdata/tor-c4t1.bin"),
        ] {
            for chunk in [HUGE_BUFFER as u64, 65536, 4093, 7] {
                assert!(check(f, BIG, chunk) > 0, "no tables in the test data");
            }
        }
    }

    /// A 4-byte table whose rows sit 3 bytes off the 256 KB ring's end, with
    /// a full ring decoded before the next write: the cut row must not be
    /// re-read from the ring (testdata/gen-edge.py).
    #[test]
    fn table_row_cut_at_the_ring_end() {
        let f = include_bytes!("../testdata/tor-edge.bin");
        for chunk in [HUGE_BUFFER as u64, 65536] {
            assert!(check(f, (538143, 0x7c8c4568), chunk) > 0);
        }
    }

    #[test]
    fn damaged() {
        let good = include_bytes!("../testdata/tor-c4t1.bin");
        let mut out = Capped(Vec::new(), 2 * BIG.0);
        // Cut short.
        assert!(decode(&mut &good[..good.len() / 2], &mut out).is_err());
        // Bytes flipped: an error or wrong data, never a panic or a hang.
        for (k, f) in [
            &good[..],
            include_bytes!("../testdata/tor-c3t1.bin"),
            include_bytes!("../testdata/tor-c1.bin"),
            include_bytes!("../testdata/tor-c2.bin"),
        ]
        .into_iter()
        .enumerate()
        {
            for at in [6, 7, 40, 1000, f.len() / 3, f.len() - 3] {
                let mut bad = f.to_vec();
                bad[at] ^= 0x5a + k as u8;
                out.0.clear();
                let _ = decode(&mut &bad[..], &mut out);
            }
        }
        // Unknown coder, empty input.
        assert!(decode(&mut &[9u8, 4, 0, 0, 1, 0][..], &mut out).is_err());
        assert!(decode(&mut &[][..], &mut out).is_err());
    }

    /// What follows the end code is read and dropped.
    #[test]
    fn drains_its_input() {
        let mut data = include_bytes!("../testdata/tor-c4opt.bin").to_vec();
        data.extend_from_slice(&[7; 100_000]);
        let mut src = &data[..];
        let mut out = Capped(Vec::new(), SMALL.0);
        assert_eq!(decode(&mut src, &mut out).unwrap(), SMALL.0 as u64);
        assert!(src.is_empty());
    }
}
