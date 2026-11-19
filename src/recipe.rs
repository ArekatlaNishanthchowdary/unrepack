//! What an ISDone-based installer does, read from its [Code] script: which
//! data files it unpacks where, the decoder settings it changes, and the
//! commands it runs after unpacking (FitGirl rebuilds game archives that
//! way). Paths keep Inno's `{tmp}`, `{app}`, `{src}` and `{cmd}` until
//! [`Places::expand`] fills them in.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

use crate::ifps::{Call, Val};

#[derive(Debug, PartialEq)]
pub enum Step {
    /// SetIniString on a file in `{tmp}`.
    Ini {
        file: String,
        section: String,
        key: String,
        value: String,
    },
    /// ISArcExtract of `{src}\<data>`. `tools` unpacks into `{tmp}` (the
    /// installer's own helpers), else into `{app}`. `sub`: only what is
    /// under that folder in the archive, with the folder taken off.
    /// `component`: the wizard choice it belongs to (`local\eng`), if any.
    Extract {
        data: String,
        tools: bool,
        sub: String,
        component: Option<String>,
    },
    /// ISExec: a program, its parameters and the folder it runs in.
    /// `makes`: what Setup shows while it runs, which for FitGirl's rebuild
    /// steps is the game file they make (`archive\pc\...\x.archive`).
    Run {
        exe: String,
        params: String,
        dir: String,
        makes: String,
    },
    DelTree(String),
    DeleteFile(String),
}

/// Calls that change something and that a recipe can't stand in for.
const UNSUPPORTED: &[&str] = &[
    "ISPrecompExtract",
    "ISSrepExtract",
    "ISxDeltaExtract",
    "IS7zipExtract",
    "ISRarExtract",
    "ISPackZIP",
    "Exec",
    "ShellExec",
    "SaveStringToFile",
    "RenameFile",
    "FileCopy",
    "RegWriteStringValue",
    "RegWriteDWordValue",
];

fn text(c: &Call, i: usize) -> Result<&str> {
    match c.args.get(i) {
        Some(Val::Str(s)) => Ok(s),
        _ => bail!(
            "Setup's {} call has an argument unrepack can't work out (number {})",
            c.name,
            i + 1
        ),
    }
}

/// A file straight inside `{src}` or `{tmp}`: its name.
fn plain(path: &str, place: &str) -> Option<String> {
    let rest = path.strip_prefix(place)?.strip_prefix('\\')?;
    (!rest.is_empty() && !rest.contains(['\\', '/', ':']) && rest != "..").then(|| rest.to_string())
}

/// The steps in script order, or why the script can't be followed.
pub fn steps(calls: &[Call]) -> Result<Vec<Step>> {
    let mut out = Vec::new();
    // ISDone numbers components from 1 in the order the script asks
    // IsComponentSelected about them.
    let mut comps: Vec<String> = Vec::new();
    for c in calls {
        let name = c.name.as_str();
        // FitGirl's "limit RAM" choices, off unless ticked.
        if c.optional {
            continue;
        }
        if name.eq_ignore_ascii_case("ISCOMPONENTSELECTED")
            && let Ok(s) = text(c, 0)
            && !comps.iter().any(|x| x.eq_ignore_ascii_case(s))
        {
            comps.push(s.to_string());
        }
        if UNSUPPORTED.iter().any(|u| u.eq_ignore_ascii_case(name)) {
            bail!("Setup calls {name}, which unrepack can't do yet");
        }
        match name.to_ascii_uppercase().as_str() {
            "ISARCEXTRACT" => {
                // (component, progress, file, to, sub-folder, delete, unarc.dll, arc.ini, work, pcf)
                let src = text(c, 2)?;
                let data = plain(src, "{src}")
                    .with_context(|| format!("Setup unpacks {src}, which isn't next to it"))?;
                let tools = match text(c, 3)?.trim_end_matches('\\') {
                    "{tmp}" => true,
                    "{app}" => false,
                    to => bail!("Setup unpacks {data} into {to}, which unrepack can't follow"),
                };
                let cfg = text(c, 7)?;
                ensure!(
                    plain(cfg, "{tmp}").is_some_and(|n| n.eq_ignore_ascii_case("arc.ini")),
                    "Setup unpacks {data} with {cfg}, not its arc.ini"
                );
                let sub = text(c, 4)?.trim_matches('\\').to_string();
                ensure!(
                    !sub.split('\\').any(|p| p == ".." || p.contains(':')),
                    "Setup unpacks {data} from an odd folder: {sub}"
                );
                let component = match c.args.first() {
                    Some(Val::Int(k)) if *k >= 1 => comps.get(*k as usize - 1).cloned(),
                    _ => None,
                };
                out.push(Step::Extract {
                    data,
                    tools,
                    sub,
                    component,
                });
            }
            "ISEXEC" => {
                // (component, progress, time, file, params, folder, label, show)
                out.push(Step::Run {
                    exe: text(c, 3)?.to_string(),
                    params: text(c, 4)?.to_string(),
                    dir: text(c, 5)?.to_string(),
                    makes: text(c, 6).unwrap_or_default().to_string(),
                });
            }
            "SETINISTRING" => {
                let file = text(c, 3)?;
                out.push(Step::Ini {
                    file: plain(file, "{tmp}").with_context(|| {
                        format!("Setup changes {file}, outside its temp folder")
                    })?,
                    section: text(c, 0)?.to_string(),
                    key: text(c, 1)?.to_string(),
                    value: text(c, 2)?.to_string(),
                });
            }
            "DELTREE" => out.push(Step::DelTree(text(c, 0)?.to_string())),
            "DELETEFILE" => out.push(Step::DeleteFile(text(c, 0)?.to_string())),
            // The uninstaller, run on the failure path only.
            "EXEC2" if text(c, 0)? == "{uninstallexe}" => {}
            "EXEC2" => bail!("Setup runs {}, which unrepack can't follow", text(c, 0)?),
            _ => {}
        }
    }
    Ok(out)
}

/// The game file a rebuild step makes, if its label names one: a relative
/// path, no spaces ("Preparing installation..." isn't one).
pub fn product(makes: &str) -> Option<&str> {
    (makes.contains('\\')
        && !makes.contains([' ', ':'])
        && !makes.starts_with('\\')
        && !makes.split('\\').any(|p| p == ".." || p.is_empty()))
    .then_some(makes)
}

/// Whether following the script matters: it unpacks helper tools for later
/// steps, or runs commands after unpacking. Otherwise the plain number-order
/// extraction is the same thing.
pub fn needed(steps: &[Step]) -> bool {
    let mut unpacked = false;
    steps.iter().any(|s| match s {
        Step::Extract { tools: true, .. } => true,
        Step::Extract { .. } => {
            unpacked = true;
            false
        }
        Step::Run { .. } => unpacked,
        _ => false,
    })
}

/// Where Inno's constants point for this run.
pub struct Places {
    /// Stands in for the installer's `{tmp}`: its decoders and helper tools.
    pub tmp: PathBuf,
    pub app: PathBuf,
    pub src: PathBuf,
}

impl Places {
    /// `s` with the constants filled in. Any other constant is refused.
    pub fn expand(&self, s: &str) -> Result<String> {
        let cmd =
            std::env::var("ComSpec").unwrap_or_else(|_| r"C:\Windows\System32\cmd.exe".into());
        let path = |p: &Path| p.display().to_string();
        let out = s
            .replace("{tmp}", &path(&self.tmp))
            .replace("{app}", &path(&self.app))
            .replace("{src}", &path(&self.src))
            .replace("{cmd}", &cmd);
        if let Some(i) = out.find('{')
            && out[i..].find('}').is_some_and(|j| {
                j > 1 && out[i + 1..i + j].bytes().all(|c| c.is_ascii_alphanumeric())
            })
        {
            bail!(
                "Setup uses {}, which unrepack can't fill in",
                &out[i..=i + out[i..].find('}').unwrap()]
            );
        }
        Ok(out)
    }

    /// A path the script deletes, if it is inside `{tmp}` or `{app}`.
    pub fn inside(&self, s: &str) -> Result<PathBuf> {
        let p = PathBuf::from(self.expand(s)?.trim_end_matches('\\'));
        ensure!(
            !p.components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
                && (p.starts_with(&self.tmp) || p.starts_with(&self.app))
                && p != self.app
                && p != self.tmp,
            "Setup deletes {}, outside what unrepack made",
            p.display()
        );
        Ok(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, args: &[&str]) -> Call {
        Call {
            name: name.into(),
            args: args.iter().map(|a| Val::Str(a.to_string())).collect(),
            optional: false,
        }
    }

    fn extract(file: &str, to: &str, sub: &str) -> Call {
        let mut c = call(
            "ISArcExtract",
            &[
                "",
                "",
                file,
                to,
                sub,
                "",
                r"{tmp}\unarc.dll",
                r"{tmp}\arc.ini",
                to,
                "",
            ],
        );
        c.args[0] = Val::Int(0);
        c
    }

    #[test]
    fn cyberpunk_shaped_script() {
        let calls = vec![
            call(
                "SETINISTRING",
                &[
                    "External compressor:bz3",
                    "unpackcmd",
                    "bzip3.exe -dc -j 4",
                    r"{tmp}\arc.ini",
                ],
            ),
            extract(r"{src}\fg-07.bin", r"{tmp}\", ""),
            extract(r"{src}\fg-05.bin", r"{app}\", ""),
            call("DIREXISTS", &[r"{app}\temp-fitgirl"]),
            call("DELTREE", &[r"{app}\temp-fitgirl\"]),
            extract(r"{src}\fg-08.bin", r"{tmp}\", "work"),
            call(
                "ISExec",
                &[
                    "",
                    "",
                    "",
                    "{cmd}",
                    r#"/C "{tmp}\xt3\xtool.exe" decode a.xtl a a.archive"#,
                    r"{app}\archive\",
                    "a",
                    "",
                ],
            ),
            call("Exec2", &["{uninstallexe}", "/VERYSILENT", ""]),
        ];
        let mut calls = calls;
        // Ticked only: skipped.
        let mut low_ram = call(
            "SETINISTRING",
            &["Srep", "Memory", "512m", r"{tmp}\CLS.ini"],
        );
        low_ram.optional = true;
        calls.insert(0, low_ram);
        calls.insert(0, call("ISCOMPONENTSELECTED", &[r"local\eng"]));
        let mut eng = extract(r"{src}\fg-selective-english-vo.bin", r"{app}\", "");
        eng.args[0] = Val::Int(1);
        calls.push(eng);
        let s = steps(&calls).unwrap();
        assert_eq!(s.len(), 7);
        assert_eq!(
            s[1],
            Step::Extract {
                data: "fg-07.bin".into(),
                tools: true,
                sub: String::new(),
                component: None,
            }
        );
        assert_eq!(
            s[4],
            Step::Extract {
                data: "fg-08.bin".into(),
                tools: true,
                sub: "work".into(),
                component: None,
            }
        );
        assert!(matches!(&s[6], Step::Extract { component: Some(c), .. } if c == r"local\eng"));
        assert!(needed(&s));
        assert!(!needed(&s[2..3]));

        let p = Places {
            tmp: r"C:\t".into(),
            app: r"D:\Game".into(),
            src: r"E:\Repack".into(),
        };
        assert_eq!(
            p.expand(r"{tmp}\xt4\xtool.exe").unwrap(),
            r"C:\t\xt4\xtool.exe"
        );
        assert!(p.expand(r"{userdocs}\x").is_err());
        assert_eq!(
            p.inside(r"{app}\temp-fitgirl\").unwrap(),
            PathBuf::from(r"D:\Game\temp-fitgirl")
        );
        assert!(p.inside(r"{app}\").is_err());
        assert!(p.inside(r"{app}\..\x").is_err());
        assert!(p.inside(r"{src}\fg-01.bin").is_err());
    }

    #[test]
    fn refusals() {
        assert!(steps(&[extract(r"{src}\..\x.bin", r"{app}\", "")]).is_err());
        assert!(steps(&[extract(r"{src}\a.bin", r"{app}\sub\", "")]).is_err());
        assert!(steps(&[call("ISPrecompExtract", &[])]).is_err());
        assert!(steps(&[call("Exec2", &[r"{app}\x.exe", "", ""])]).is_err());
        let mut c = call("ISExec", &["", "", "", "{cmd}", "", "", "", ""]);
        c.args[4] = Val::Unknown;
        assert!(steps(&[c]).is_err());
    }

    #[test]
    fn product_takes_only_a_plain_relative_path() {
        assert_eq!(
            product(r"archive\pc\ep1\x.archive"),
            Some(r"archive\pc\ep1\x.archive")
        );
        for bad in [
            "Preparing installation...",
            "...",
            "",
            r"\x\y",
            r"C:\x\y",
            r"a\..\..\y",
            r"a\\y",
        ] {
            assert_eq!(product(bad), None, "{bad}");
        }
    }
}
