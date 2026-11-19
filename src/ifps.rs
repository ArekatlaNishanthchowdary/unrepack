//! Reads an Inno Setup [Code] script (RemObjects PascalScript bytecode,
//! "IFPS") far enough to list what a procedure calls, with the text of
//! each argument. Layouts follow uPSRuntime.pas (LoadData, ReadVariable,
//! RunScript). ponytail: a straight walk with no branches taken; every
//! call along the way is reported, so the caller decides what each means.

use anyhow::{Context, Result, bail, ensure};

const HEADER: u32 = 0x5350_4649; // "IFPS"
const STACK_NEG: u32 = 0x4000_0000;
const STACK_START: u32 = 0x6000_0000;

/// Base types the walk needs to tell apart (uPSUtils.pas).
mod bt {
    pub const U8: u8 = 1;
    pub const S8: u8 = 2;
    pub const U16: u8 = 3;
    pub const S16: u8 = 4;
    pub const U32: u8 = 5;
    pub const S32: u8 = 6;
    pub const SINGLE: u8 = 7;
    pub const DOUBLE: u8 = 8;
    pub const EXTENDED: u8 = 9;
    pub const STRING: u8 = 10;
    pub const RECORD: u8 = 11;
    pub const ARRAY: u8 = 12;
    pub const PCHAR: u8 = 14;
    pub const S64: u8 = 17;
    pub const CHAR: u8 = 18;
    pub const WIDESTRING: u8 = 19;
    pub const WIDECHAR: u8 = 20;
    pub const PROCPTR: u8 = 21;
    pub const STATICARRAY: u8 = 22;
    pub const SET: u8 = 23;
    pub const CURRENCY: u8 = 24;
    pub const CLASS: u8 = 25;
    pub const INTERFACE: u8 = 26;
    pub const UNICODESTRING: u8 = 28;
    pub const U64: u8 = 29;
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Val {
    Str(String),
    Int(i64),
    /// Whether a wizard checkbox is ticked (`true`), or its opposite.
    Ticked(bool),
    #[default]
    Unknown,
}

/// One call to a procedure outside the script, its arguments first to last.
#[derive(Debug, PartialEq)]
pub struct Call {
    pub name: String,
    pub args: Vec<Val>,
    /// Made only when a checkbox on the wizard is ticked (FitGirl's
    /// "limit RAM" option), which it isn't by default.
    pub optional: bool,
}

struct Type {
    base: u8,
    /// A set's size in bytes, which its constants take.
    set_bytes: usize,
}

enum Proc {
    /// Imported: its name (for a DLL import, the function's), how many
    /// parameters it takes and whether it returns something.
    External {
        name: String,
        params: usize,
        result: bool,
    },
    Internal {
        code: std::ops::Range<usize>,
        export: String,
    },
}

pub struct Script {
    data: Vec<u8>,
    types: Vec<Type>,
    procs: Vec<Proc>,
    globals: usize,
}

struct Rd<'a> {
    b: &'a [u8],
    at: usize,
}

impl Rd<'_> {
    fn bytes(&mut self, n: usize) -> Result<&[u8]> {
        let s = self
            .b
            .get(self.at..self.at.checked_add(n).context("script: bad length")?)
            .context("script: unexpected end")?;
        self.at += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into()?))
    }
    fn lstr(&mut self) -> Result<Vec<u8>> {
        let n = self.u32()? as usize;
        Ok(self.bytes(n)?.to_vec())
    }
}

fn ansi(b: &[u8]) -> String {
    // ponytail: Latin-1 for ANSI text; paths and commands here are ASCII.
    b.iter().map(|&c| c as char).collect()
}

impl Script {
    pub fn parse(data: &[u8]) -> Result<Script> {
        let mut r = Rd { b: data, at: 0 };
        ensure!(r.u32()? == HEADER, "not a PascalScript script");
        let build = r.u32()?;
        ensure!(
            (12..=23).contains(&build),
            "PascalScript build {build} isn't supported"
        );
        let (ntypes, nprocs, nvars) = (r.u32()?, r.u32()?, r.u32()?);
        let _main = r.u32()?;
        let _imports = r.u32()?;
        let mut s = Script {
            data: data.to_vec(),
            types: Vec::new(),
            procs: Vec::new(),
            globals: nvars as usize,
        };
        for _ in 0..ntypes {
            let raw = r.u8()?;
            let base = raw & 0x7f;
            let mut set_bytes = 0;
            match base {
                bt::CLASS | bt::PROCPTR => {
                    r.lstr()?;
                }
                bt::INTERFACE => {
                    r.bytes(16)?;
                }
                bt::SET => set_bytes = (r.u32()? as usize).div_ceil(8),
                bt::STATICARRAY => {
                    r.bytes(12)?;
                }
                bt::ARRAY => {
                    r.u32()?;
                }
                bt::RECORD => {
                    let n = r.u32()?;
                    r.bytes(4 * n as usize)?;
                }
                1..=10 | 13..=20 | 24 | 28 | 29 => {}
                _ => bail!("script: unknown type {base}"),
            }
            if raw & 0x80 != 0 {
                r.lstr()?;
            }
            s.types.push(Type { base, set_bytes });
            if build >= 21 {
                s.attributes(&mut r)?;
            }
        }
        for _ in 0..nprocs {
            let flags = r.u8()?;
            let proc = if flags & 1 != 0 {
                let n = r.u8()? as usize;
                let name = ansi(r.bytes(n)?);
                let decl = if flags & 3 == 3 {
                    r.lstr()?
                } else {
                    Vec::new()
                };
                // A DLL import is nameless; its decl is "dll:<dll>\0<function>\0"
                // then call convention, delay load, altered search path. What
                // follows, as a built-in's whole decl: a result flag, then one
                // byte per parameter.
                let (name, sig) = match decl.strip_prefix(b"dll:") {
                    Some(rest) => {
                        let mut parts = rest.splitn(3, |&c| c == 0);
                        let (_dll, func, tail) = (parts.next(), parts.next(), parts.next());
                        (
                            ansi(func.unwrap_or_default()),
                            tail.and_then(|t| t.get(3..)),
                        )
                    }
                    // "class:<class>|<member>|" then the signature, the
                    // object counted among the parameters.
                    None if decl.starts_with(b"class:") => {
                        let cut = decl
                            .iter()
                            .rposition(|&c| c == b'|')
                            .map_or(decl.len(), |i| i + 1);
                        (ansi(&decl[..cut]), Some(&decl[cut..]))
                    }
                    None => (name, Some(&decl[..])),
                };
                let sig = sig.unwrap_or_default();
                Proc::External {
                    name,
                    params: sig.len().saturating_sub(1),
                    result: sig.first().is_some_and(|&c| c != 0),
                }
            } else {
                let (off, len) = (r.u32()? as usize, r.u32()? as usize);
                ensure!(
                    off.checked_add(len).is_some_and(|e| e <= data.len()),
                    "script: proc out of range"
                );
                let mut export = String::new();
                if flags & 2 != 0 {
                    export = ansi(&r.lstr()?);
                    r.lstr()?;
                }
                Proc::Internal {
                    code: off..off + len,
                    export,
                }
            };
            if flags & 4 != 0 {
                s.attributes(&mut r)?;
            }
            s.procs.push(proc);
        }
        for _ in 0..nvars {
            let _ty = r.u32()?;
            if r.u8()? & 1 != 0 {
                r.lstr()?;
            }
        }
        Ok(s)
    }

    fn attributes(&self, r: &mut Rd) -> Result<()> {
        for _ in 0..r.u32()? {
            r.lstr()?;
            for _ in 0..r.u32()? {
                let t = r.u32()?;
                self.constant(r, t)?;
            }
        }
        Ok(())
    }

    /// A constant of type `t`, as ReadVariable reads one.
    fn constant(&self, r: &mut Rd, t: u32) -> Result<Val> {
        let ty = self.types.get(t as usize).context("script: bad type")?;
        Ok(match ty.base {
            bt::U8 | bt::CHAR => Val::Int(r.u8()? as i64),
            bt::S8 => Val::Int(r.u8()? as i8 as i64),
            bt::U16 | bt::WIDECHAR => Val::Int(u16::from_le_bytes(r.bytes(2)?.try_into()?) as i64),
            bt::S16 => Val::Int(i16::from_le_bytes(r.bytes(2)?.try_into()?) as i64),
            bt::U32 | bt::PROCPTR => Val::Int(r.u32()? as i64),
            bt::S32 => Val::Int(r.u32()? as i32 as i64),
            bt::S64 | bt::U64 | bt::CURRENCY => {
                Val::Int(i64::from_le_bytes(r.bytes(8)?.try_into()?))
            }
            bt::SINGLE => {
                r.bytes(4)?;
                Val::Unknown
            }
            bt::DOUBLE => {
                r.bytes(8)?;
                Val::Unknown
            }
            bt::EXTENDED => {
                r.bytes(10)?;
                Val::Unknown
            }
            bt::SET => {
                r.bytes(ty.set_bytes)?;
                Val::Unknown
            }
            bt::STRING | bt::PCHAR => Val::Str(ansi(&r.lstr()?)),
            bt::WIDESTRING | bt::UNICODESTRING => {
                let n = r.u32()? as usize;
                let units: Vec<u16> = r
                    .bytes(n.checked_mul(2).context("script: bad length")?)?
                    .chunks(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                Val::Str(String::from_utf16_lossy(&units))
            }
            b => bail!("script: no constants of type {b}"),
        })
    }

    /// Every call to an outside procedure in the exported procedure `name`
    /// (and in the script procedures it calls), in code order.
    pub fn calls(&self, name: &str) -> Result<Vec<Call>> {
        let i = self
            .procs
            .iter()
            .position(
                |p| matches!(p, Proc::Internal { export, .. } if export.eq_ignore_ascii_case(name)),
            )
            .with_context(|| format!("the script has no {name}"))?;
        let mut m = Mem {
            globals: vec![Val::Unknown; self.globals],
            // CurStepChanged(CurStep): the parameter, then the return address.
            stack: vec![Cell::default(), Cell::default()],
            base: 1,
        };
        let mut out = Vec::new();
        self.walk(i, &mut m, &mut out, 0, false)?;
        Ok(out)
    }

    /// `optional`: the caller only gets here with a checkbox ticked.
    fn walk(
        &self,
        proc: usize,
        m: &mut Mem,
        out: &mut Vec<Call>,
        depth: usize,
        optional: bool,
    ) -> Result<()> {
        let Proc::Internal { code, .. } = &self.procs[proc] else {
            unreachable!()
        };
        let mut r = Rd {
            b: &self.data[code.clone()],
            at: 0,
        };
        // Ends of code that runs only with a checkbox ticked.
        let mut ticked_until: Vec<usize> = Vec::new();
        let mut flag = Val::Unknown;
        while r.at < r.b.len() {
            ticked_until.retain(|&end| r.at < end);
            let optional = optional || !ticked_until.is_empty();
            let op = r.u8()?;
            match op {
                0 => {
                    // assign
                    let d = self.var(&mut r, m)?;
                    let v = self.read(&mut r, m)?;
                    m.set(d, v);
                }
                1 => {
                    // calculate: dest op= src
                    let kind = r.u8()?;
                    let d = self.var(&mut r, m)?;
                    let v = self.read(&mut r, m)?;
                    let new = match (kind, m.get(d), v) {
                        (0, Val::Str(a), Val::Str(b)) => Val::Str(a + &b),
                        (0, Val::Int(a), Val::Int(b)) => Val::Int(a.wrapping_add(b)),
                        _ => Val::Unknown,
                    };
                    m.set(d, new);
                }
                2 => {
                    // push a copy
                    let val = self.read(&mut r, m)?;
                    m.stack.push(Cell { val, to: None });
                }
                3 => {
                    // push a reference: writes to it land in the variable
                    let to = self.var(&mut r, m)?;
                    m.stack.push(Cell { val: m.get(to), to });
                }
                4 => {
                    m.stack.pop().context("script: pop from an empty stack")?;
                }
                5 => {
                    let n = r.u32()? as usize;
                    match self.procs.get(n).context("script: bad proc")? {
                        Proc::External {
                            name,
                            params,
                            result,
                        } => {
                            // Parameters are pushed last first, then the
                            // result's slot, so it is on top, then the first.
                            let top = m.stack.len() - usize::from(*result);
                            let args: Vec<Val> = (top.saturating_sub(*params)..top)
                                .rev()
                                .map(|i| m.get(Some(Loc::Stack(i))))
                                .collect();
                            if *result && let Some(v) = result_of(name, &args) {
                                m.set(Some(Loc::Stack(top)), v);
                            }
                            if name.ends_with("|CHECKED|") {
                                for i in top.saturating_sub(*params)..m.stack.len() {
                                    m.set(Some(Loc::Stack(i)), Val::Ticked(true));
                                }
                            }
                            out.push(Call {
                                name: name.clone(),
                                args,
                                optional,
                            });
                        }
                        Proc::Internal { .. } if depth < 8 => {
                            m.stack.push(Cell::default()); // return address
                            let (keep, base) = (m.stack.len(), m.base);
                            m.base = keep - 1;
                            self.walk(n, m, out, depth + 1, optional)?;
                            m.stack.truncate(keep - 1);
                            m.base = base;
                        }
                        Proc::Internal { .. } => bail!("script: calls nest too deep"),
                    }
                }
                18 => {
                    // jump if the flag is set; see 7 and 8
                    let skip = r.u32()? as usize;
                    if flag == Val::Ticked(false) {
                        ticked_until.push(r.at + skip);
                    }
                }
                6 | 25 | 26 => {
                    r.u32()?;
                }
                7 | 8 => {
                    // Jump if the variable is true (7) or false (8). The code
                    // skipped over runs only when the checkbox is ticked if
                    // the jump is taken when it isn't.
                    let skip = r.u32()? as usize;
                    let v = self.var(&mut r, m)?;
                    if let Val::Ticked(t) = m.get(v)
                        && t == (op == 8)
                    {
                        ticked_until.push(r.at + skip);
                    }
                }
                9 => break,
                10 => {
                    r.bytes(8)?;
                }
                11 => {
                    r.u32()?;
                    m.stack.push(Cell::default());
                }
                12 => {
                    // compare into a variable
                    r.u8()?;
                    let d = self.var(&mut r, m)?;
                    self.read(&mut r, m)?;
                    self.read(&mut r, m)?;
                    m.set(d, Val::Unknown);
                }
                15 => {
                    // boolean not
                    let d = self.var(&mut r, m)?;
                    let v = match m.get(d) {
                        Val::Ticked(t) => Val::Ticked(!t),
                        _ => Val::Unknown,
                    };
                    m.set(d, v);
                }
                13 | 16 | 21 | 23 | 24 => {
                    let d = self.var(&mut r, m)?;
                    m.set(d, Val::Unknown);
                }
                14 | 22 => {
                    self.var(&mut r, m)?;
                    self.var(&mut r, m)?;
                }
                17 => {
                    // set the flag from a variable, or from its opposite
                    let v = self.var(&mut r, m)?;
                    let not = r.u8()? != 0;
                    flag = match m.get(v) {
                        Val::Ticked(t) => Val::Ticked(t != not),
                        _ => Val::Unknown,
                    };
                }
                19 => {
                    r.bytes(16)?;
                }
                20 => {
                    r.u8()?;
                }
                255 => {}
                _ => bail!(
                    "script: unknown instruction {op} at {}",
                    code.start + r.at - 1
                ),
            }
        }
        Ok(())
    }

    /// A variable operand, as a place; None for a record field or array
    /// element, which the walk doesn't track.
    fn var(&self, r: &mut Rd, m: &Mem) -> Result<Option<Loc>> {
        let kind = r.u8()?;
        let p = r.u32()?;
        match kind {
            0 => Ok(m.loc(p)),
            2 | 3 => {
                r.u32()?;
                Ok(None)
            }
            _ => bail!("script: a constant where a variable belongs"),
        }
    }

    fn read(&self, r: &mut Rd, m: &Mem) -> Result<Val> {
        let kind = r.u8()?;
        let p = r.u32()?;
        Ok(match kind {
            0 => m.get(m.loc(p)),
            1 => self.constant(r, p)?,
            2 | 3 => {
                r.u32()?;
                Val::Unknown
            }
            _ => bail!("script: unknown operand kind {kind}"),
        })
    }
}

#[derive(Clone, Copy)]
enum Loc {
    Global(usize),
    Stack(usize),
}

#[derive(Default)]
struct Cell {
    val: Val,
    /// Pushed as a reference: reads and writes go to this place.
    to: Option<Loc>,
}

struct Mem {
    globals: Vec<Val>,
    stack: Vec<Cell>,
    /// The running procedure's return address; its locals sit above.
    base: usize,
}

impl Mem {
    fn loc(&self, p: u32) -> Option<Loc> {
        if p < STACK_NEG {
            return Some(Loc::Global(p as usize));
        }
        let i = self.base as i64 + p.wrapping_sub(STACK_START) as i32 as i64;
        (0..self.stack.len() as i64)
            .contains(&i)
            .then_some(Loc::Stack(i as usize))
    }

    /// Follows a reference to the place it stands for.
    fn deref(&self, mut l: Loc) -> Loc {
        for _ in 0..16 {
            match l {
                Loc::Stack(i) => match self.stack.get(i).and_then(|c| c.to) {
                    Some(t) => l = t,
                    None => break,
                },
                Loc::Global(_) => break,
            }
        }
        l
    }

    fn get(&self, l: Option<Loc>) -> Val {
        match l.map(|l| self.deref(l)) {
            Some(Loc::Global(i)) => self.globals.get(i).cloned(),
            Some(Loc::Stack(i)) => self.stack.get(i).map(|c| c.val.clone()),
            None => None,
        }
        .unwrap_or(Val::Unknown)
    }

    fn set(&mut self, l: Option<Loc>, v: Val) {
        match l.map(|l| self.deref(l)) {
            Some(Loc::Global(i)) => {
                if let Some(g) = self.globals.get_mut(i) {
                    *g = v;
                }
            }
            Some(Loc::Stack(i)) => {
                if let Some(c) = self.stack.get_mut(i) {
                    c.val = v;
                }
            }
            None => {}
        }
    }
}

/// What a built-in returns, when the walk can say.
fn result_of(name: &str, args: &[Val]) -> Option<Val> {
    match name.to_ascii_uppercase().as_str() {
        // Left as-is: the caller maps {app}, {tmp} and {src} itself.
        "EXPANDCONSTANT" | "REMOVEBACKSLASH" => args.first().cloned(),
        "CLASS:TCHECKBOX|CHECKED|" | "CLASS:TNEWCHECKBOX|CHECKED|" => Some(Val::Ticked(true)),
        "ADDBACKSLASH" => Some(match args.first() {
            Some(Val::Str(s)) if !s.ends_with('\\') => Val::Str(format!("{s}\\")),
            Some(v) => v.clone(),
            None => Val::Unknown,
        }),
        "EXTRACTFILEPATH" => Some(match args.first() {
            Some(Val::Str(s)) => Val::Str(s[..s.rfind('\\').map_or(0, |i| i + 1)].to_string()),
            _ => Val::Unknown,
        }),
        "ADDQUOTES" => Some(match args.first() {
            Some(Val::Str(s)) => Val::Str(format!("\"{s}\"")),
            _ => Val::Unknown,
        }),
        // Unknown result, or a procedure with none: the slot below stays
        // as pushed, unknown.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unicode-string constant operand.
    fn text(s: &str) -> Vec<u8> {
        let mut b = vec![1, 0, 0, 0, 0]; // constant of type 0
        b.extend((s.encode_utf16().count() as u32).to_le_bytes());
        b.extend(s.encode_utf16().flat_map(u16::to_le_bytes));
        b
    }

    fn lstr(s: &[u8]) -> Vec<u8> {
        [&(s.len() as u32).to_le_bytes()[..], s].concat()
    }

    /// CurStepChanged doing SetIniString('S', 'K', ExpandConstant('{app}'),
    /// 'F'), with ExpandConstant's result passed by reference, as the
    /// compiler does.
    #[test]
    fn walks_calls_and_references() {
        let slot = |k: u32| (STACK_START + k).to_le_bytes();
        let mut code = Vec::new();
        code.push(2);
        code.extend(text("F")); // [2] file
        code.extend([11, 0, 0, 0, 0]); // [3] value slot
        code.push(2);
        code.extend(text("{app}")); // [4] ExpandConstant's argument
        code.extend([3, 0]);
        code.extend(slot(2)); // [5] its result: a reference to [3]
        code.extend([5, 0, 0, 0, 0, 4, 4]);
        code.push(2);
        code.extend(text("K"));
        code.push(2);
        code.extend(text("S"));
        code.extend([11, 0, 0, 0, 0]); // SetIniString's result
        code.extend([5, 1, 0, 0, 0, 9]);

        let mut procs = Vec::new();
        for (name, decl) in [
            ("EXPANDCONSTANT", &[1u8, 0][..]),
            ("SETINISTRING", &[1, 0, 0, 0, 0]),
        ] {
            procs.push(3);
            procs.push(name.len() as u8);
            procs.extend(name.as_bytes());
            procs.extend(lstr(decl));
        }
        let head_len =
            28 + 1 + procs.len() + 1 + 8 + lstr(b"CURSTEPCHANGED").len() + lstr(b"").len();
        procs.push(2);
        procs.extend((head_len as u32).to_le_bytes());
        procs.extend((code.len() as u32).to_le_bytes());
        procs.extend(lstr(b"CURSTEPCHANGED"));
        procs.extend(lstr(b""));

        let mut data = Vec::new();
        for v in [HEADER, 20, 1, 3, 0, 2, 0] {
            data.extend(v.to_le_bytes());
        }
        data.push(bt::UNICODESTRING);
        data.extend(procs);
        assert_eq!(data.len(), head_len);
        data.extend(code);

        let calls = Script::parse(&data)
            .unwrap()
            .calls("CurStepChanged")
            .unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].args, [Val::Str("{app}".into())]);
        assert_eq!(calls[1].name, "SETINISTRING");
        assert_eq!(
            calls[1].args,
            ["S", "K", "{app}", "F"].map(|s| Val::Str(s.into()))
        );
    }
}
