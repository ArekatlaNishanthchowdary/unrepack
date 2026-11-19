//! Runs the real binary on FreeArc archives written here, in the layout read
//! from real repack data files. Methods are `storing` and `copy`, a CLS codec
//! built from clshost/test_copy.c, so the host processes run as they would
//! with a repack's decoders. CLS tests are skipped when gcc can't build it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

fn varint(v: u64) -> Vec<u8> {
    for n in 1..=8u32 {
        if n < 8 && v >= 1u64 << (7 * n) {
            continue;
        }
        if n == 8 && v >= 1u64 << 56 {
            break;
        }
        let x = (u128::from(v) << n) | ((1u128 << (n - 1)) - 1);
        return x.to_le_bytes()[..n as usize].to_vec();
    }
    let mut out = vec![0xff];
    out.extend_from_slice(&v.to_le_bytes());
    out
}

fn descriptor(kind: u8, method: &str, data: &[u8]) -> Vec<u8> {
    let mut d = b"ArC\x01".to_vec();
    d.push(kind);
    d.extend_from_slice(method.as_bytes());
    d.push(0);
    d.extend(varint(data.len() as u64));
    d.extend(varint(data.len() as u64));
    d.extend(crc32fast::hash(data).to_le_bytes());
    let own = crc32fast::hash(&d);
    d.extend(own.to_le_bytes());
    d
}

struct File {
    dir: &'static str,
    name: &'static str,
    data: Vec<u8>,
}

/// Writes one archive: one solid block holding `files` with `method`, and an
/// empty `storing` block holding the folder entries, as repacks do.
fn write_archive(path: &Path, method: &str, files: &[File], corrupt_crc: bool) {
    let mut a = b"ArC\x01\x00\x00\x06\x07".to_vec();
    let header = a.clone();
    a.extend(descriptor(2, "storing", &header));
    let block_start = a.len() as u64;
    for f in files {
        a.extend_from_slice(&f.data);
    }
    let packed = a.len() as u64 - block_start;

    let mut dirs: Vec<&str> = vec![""];
    for f in files {
        if !dirs.contains(&f.dir) {
            dirs.push(f.dir);
        }
    }
    let folder_entries: Vec<&str> = dirs.iter().copied().filter(|d| !d.is_empty()).collect();
    let dir_start = a.len() as u64;
    let mut d = varint(2);
    d.extend(varint(folder_entries.len() as u64));
    d.extend(varint(files.len() as u64));
    for m in ["storing", method] {
        d.extend_from_slice(m.as_bytes());
        d.push(0);
    }
    d.extend(varint(dir_start - block_start));
    d.extend(varint(dir_start - block_start));
    d.extend(varint(0));
    d.extend(varint(packed));
    d.extend(varint(dirs.len() as u64));
    for dir in &dirs {
        d.extend_from_slice(dir.as_bytes());
        d.push(0);
    }
    // Folder entries are named in the root folder by their full path.
    for fe in &folder_entries {
        d.extend_from_slice(fe.as_bytes());
        d.push(0);
    }
    for f in files {
        d.extend_from_slice(f.name.as_bytes());
        d.push(0);
    }
    for _ in &folder_entries {
        d.extend(varint(0));
    }
    for f in files {
        d.extend(varint(dirs.iter().position(|x| *x == f.dir).unwrap() as u64));
    }
    for _ in &folder_entries {
        d.extend(varint(0));
    }
    for f in files {
        d.extend(varint(f.data.len() as u64));
    }
    for _ in 0..folder_entries.len() + files.len() {
        d.extend(0x6abc_0000u32.to_le_bytes());
    }
    d.extend(std::iter::repeat_n(1u8, folder_entries.len()));
    d.extend(std::iter::repeat_n(0u8, files.len()));
    for _ in &folder_entries {
        d.extend(0u32.to_le_bytes());
    }
    for (k, f) in files.iter().enumerate() {
        let mut c = crc32fast::hash(&f.data);
        if corrupt_crc && k == files.len() - 1 {
            c ^= 1;
        }
        d.extend(c.to_le_bytes());
    }
    d.push(0);
    a.extend_from_slice(&d);
    a.extend(descriptor(6, "storing", &d));

    let footer_start = a.len() as u64;
    let mut ft = varint(2);
    for (kind, start, data) in [(2u8, 0u64, &header), (6u8, dir_start, &d)] {
        ft.push(kind);
        ft.extend_from_slice(b"storing\0");
        ft.extend(varint(footer_start - start));
        ft.extend(varint(data.len() as u64));
        ft.extend(varint(data.len() as u64));
        ft.extend(crc32fast::hash(data).to_le_bytes());
    }
    ft.extend([0, 0, 0, 0]);
    a.extend_from_slice(&ft);
    a.extend(descriptor(8, "storing", &ft));
    fs::write(path, a).unwrap();
}

fn sample_files() -> Vec<File> {
    let mut big = Vec::with_capacity(3 << 20);
    let mut x = 0x1234_5678u32;
    for _ in 0..(3 << 20) {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        big.push(x as u8);
    }
    vec![
        File {
            dir: "game/data",
            name: "big.bin",
            data: big,
        },
        File {
            dir: "game",
            name: "readme.txt",
            data: b"hello".to_vec(),
        },
        File {
            dir: "game",
            name: "empty.cfg",
            data: vec![],
        },
        File {
            dir: "",
            name: "root.ini",
            data: b"[x]".to_vec(),
        },
    ]
}

/// The host and a copy codec named `cls-copy.dll`, or None without gcc.
fn decoders() -> Option<&'static (PathBuf, PathBuf)> {
    static D: OnceLock<Option<(PathBuf, PathBuf)>> = OnceLock::new();
    D.get_or_init(|| {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("decoders");
        fs::create_dir_all(&dir).ok()?;
        let host = dir.join("unrepack-clshost.exe");
        let gcc = |args: &[&std::ffi::OsStr]| {
            Command::new("gcc")
                .args(args)
                .status()
                .is_ok_and(|s| s.success())
        };
        let ok = gcc(&[
            "-m32".as_ref(),
            "-O2".as_ref(),
            "-static".as_ref(),
            "-o".as_ref(),
            host.as_os_str(),
            root.join("clshost/clshost.c").as_os_str(),
        ]) && gcc(&[
            "-m32".as_ref(),
            "-O2".as_ref(),
            "-shared".as_ref(),
            "-static".as_ref(),
            "-o".as_ref(),
            dir.join("cls-copy.dll").as_os_str(),
            root.join("clshost/test_copy.c").as_os_str(),
        ]);
        if !ok {
            eprintln!("gcc -m32 unavailable; skipping CLS tests");
            return None;
        }
        Some((dir, host))
    })
    .as_ref()
}

fn unrepack(args: &[&std::ffi::OsStr]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_unrepack"))
        .args(args)
        .output()
        .unwrap()
}

fn assert_extracted(out: &Path, files: &[File]) {
    for f in files {
        let p = out.join(f.dir).join(f.name);
        assert_eq!(
            fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display())),
            f.data,
            "{}",
            p.display()
        );
    }
    assert!(out.join("game/data").is_dir());
    assert!(
        !out.join(".unrepack-work").exists(),
        "work folder left behind"
    );
}

fn extract(method: &str, extra: &[&str]) -> (tempfile::TempDir, PathBuf, std::process::Output) {
    let t = tempfile::tempdir().unwrap();
    let arc = t.path().join("data1.doi");
    write_archive(&arc, method, &sample_files(), false);
    let out = t.path().join("out");
    let mut args: Vec<&std::ffi::OsStr> = vec![
        "extract".as_ref(),
        arc.as_os_str(),
        "-o".as_ref(),
        out.as_os_str(),
        "--yes".as_ref(),
    ];
    let (dir, host);
    if method != "storing" {
        (dir, host) = decoders().cloned().unwrap();
        args.extend([
            "--decoders".as_ref(),
            dir.as_os_str(),
            "--host".as_ref(),
            host.as_os_str(),
        ]);
        let o = unrepack(
            &[
                args.as_slice(),
                &extra.iter().map(|s| s.as_ref()).collect::<Vec<_>>(),
            ]
            .concat(),
        );
        return (t, arc, o);
    }
    let o = unrepack(
        &[
            args.as_slice(),
            &extra.iter().map(|s| s.as_ref()).collect::<Vec<_>>(),
        ]
        .concat(),
    );
    (t, arc, o)
}

fn ok(o: &std::process::Output) {
    assert!(
        o.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
}

#[test]
fn stored_block_extracts_and_keeps_the_archive() {
    let (t, arc, o) = extract("storing", &[]);
    ok(&o);
    assert_extracted(&t.path().join("out"), &sample_files());
    assert!(arc.exists(), "archive kept without --free-as-read");
}

#[test]
fn stored_block_freed_as_read_is_deleted_at_the_end() {
    let (t, arc, o) = extract("storing", &["--free-as-read"]);
    ok(&o);
    let stdout = String::from_utf8_lossy(&o.stdout);
    assert!(stdout.contains("measured:"), "{stdout}");
    assert_extracted(&t.path().join("out"), &sample_files());
    assert!(!arc.exists(), "archive deleted");
    assert!(
        !t.path().join("data1.doi.unrepack-consumed").exists(),
        "marker removed"
    );
}

#[test]
fn one_cls_stage_through_the_host() {
    if decoders().is_none() {
        return;
    }
    let (t, arc, o) = extract("copy", &["--free-as-read"]);
    ok(&o);
    assert_extracted(&t.path().join("out"), &sample_files());
    assert!(!arc.exists());
}

#[test]
fn two_cls_stages_chained() {
    if decoders().is_none() {
        return;
    }
    let (t, _, o) = extract("copy+copy:x", &["--delete-each"]);
    ok(&o);
    assert_extracted(&t.path().join("out"), &sample_files());
}

/// A stage that fails while its helper still holds the pipes (FitGirl's srep
/// shim did) must end the run at once, not hang until the helper exits.
#[test]
fn a_failing_stage_stops_the_chain() {
    if decoders().is_none() {
        return;
    }
    let t0 = std::time::Instant::now();
    let (_t, _, o) = extract("copy+copy:fail", &[]);
    assert!(!o.status.success());
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(30),
        "took {:?}",
        t0.elapsed()
    );
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("failed"), "{err}");
}

#[test]
fn a_bad_crc_stops_the_verify_pass_before_anything_is_freed() {
    let t = tempfile::tempdir().unwrap();
    let arc = t.path().join("data1.doi");
    write_archive(&arc, "storing", &sample_files(), true);
    let before = fs::read(&arc).unwrap();
    let out = t.path().join("out");
    let o = unrepack(&[
        "extract".as_ref(),
        arc.as_os_str(),
        "-o".as_ref(),
        out.as_os_str(),
        "--yes".as_ref(),
        "--free-as-read".as_ref(),
    ]);
    assert!(!o.status.success());
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("CRC"),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert_eq!(fs::read(&arc).unwrap(), before, "archive untouched");
    assert!(!t.path().join("data1.doi.unrepack-consumed").exists());
}

#[test]
fn refusals_happen_before_anything_changes() {
    let t = tempfile::tempdir().unwrap();
    let arc = t.path().join("data1.doi");
    write_archive(&arc, "lolz", &sample_files(), false);
    let before = fs::read(&arc).unwrap();
    let out = t.path().join("out");
    let run = |extra: &[&str]| {
        let mut a: Vec<&std::ffi::OsStr> = vec![
            "extract".as_ref(),
            arc.as_os_str(),
            "-o".as_ref(),
            out.as_os_str(),
            "--yes".as_ref(),
            "--free-as-read".as_ref(),
        ];
        a.extend(extra.iter().map(|s| -> &std::ffi::OsStr { s.as_ref() }));
        unrepack(&a)
    };
    // No decoders folder for a CLS method.
    let o = run(&[]);
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("--decoders"),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    // A decoders folder without cls-lolz.dll.
    let empty = t.path().join("empty");
    fs::create_dir(&empty).unwrap();
    let o = run(&["--decoders", empty.to_str().unwrap()]);
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("cls-lolz.dll"),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert_eq!(fs::read(&arc).unwrap(), before);

    // An output file in the way.
    write_archive(&arc, "storing", &sample_files(), false);
    fs::create_dir_all(out.join("game")).unwrap();
    fs::write(out.join("game/readme.txt"), "mine").unwrap();
    let o = run(&[]);
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("already exists"),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert_eq!(
        fs::read_to_string(out.join("game/readme.txt")).unwrap(),
        "mine"
    );

    // A marker from an earlier, interrupted run.
    fs::write(t.path().join("data1.doi.unrepack-consumed"), "x").unwrap();
    let o = run(&[]);
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("partly freed"),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
}

/// A DODI-style folder: `parts` says what each data file is, and two
/// language packs holding the same file extract with the later one's copy.
#[test]
fn language_packs_are_parts_and_the_later_copy_wins() {
    let t = tempfile::tempdir().unwrap();
    let pack = |name: &str, ini: &'static [u8], voice: &'static str| {
        write_archive(
            &t.path().join(name),
            "storing",
            &[
                File {
                    dir: "sound",
                    name: "desktop.ini",
                    data: ini.to_vec(),
                },
                File {
                    dir: "sound",
                    name: voice,
                    data: vec![7; 300],
                },
            ],
            false,
        );
    };
    write_archive(
        &t.path().join("data1.doi"),
        "storing",
        &sample_files(),
        false,
    );
    // As GoW's data2.doi: the base game's copy of a file each language
    // pack replaces. Sorted by name it would come after the packs.
    write_archive(
        &t.path().join("data2.doi"),
        "storing",
        &[File {
            dir: "sound",
            name: "desktop.ini",
            data: b"[base]".to_vec(),
        }],
        false,
    );
    pack("English.doi", b"[en]", "vm_enu.fsb");
    pack("French.doi", b"[fr]", "vm_fra.fsb");
    write_archive(
        &t.path().join("Artwork.doi"),
        "storing",
        &[File {
            dir: "art",
            name: "a.png",
            data: vec![1; 50],
        }],
        false,
    );

    let o = unrepack(&["parts".as_ref(), t.path().as_os_str()]);
    ok(&o);
    let s = String::from_utf8_lossy(&o.stdout);
    let lines: Vec<(&str, &str, &str)> = s
        .lines()
        .map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            (f[0], f[1], f[4])
        })
        .collect();
    assert_eq!(
        lines,
        [
            ("required", "on", "data1.doi"),
            ("required", "on", "data2.doi"),
            ("optional", "on", "Artwork.doi"),
            ("language", "on", "English.doi"),
            ("language", "off", "French.doi"),
        ]
    );

    let out = t.path().join("out");
    let o = unrepack(&[
        "extract".as_ref(),
        t.path().join("data1.doi").as_os_str(),
        t.path().join("English.doi").as_os_str(),
        t.path().join("French.doi").as_os_str(),
        "-o".as_ref(),
        out.as_os_str(),
    ]);
    ok(&o);
    assert_extracted(&out, &sample_files());
    assert_eq!(fs::read(out.join("sound/desktop.ini")).unwrap(), b"[fr]");
    assert!(out.join("sound/vm_enu.fsb").is_file() && out.join("sound/vm_fra.fsb").is_file());
    assert!(!out.join("art").exists());
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("French.doi's copy is kept"),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );

    // The whole folder: the required files go first, so a language pack's
    // copy wins over the base game's, as with the installer.
    let all = t.path().join("all");
    ok(&unrepack(&[
        "extract".as_ref(),
        t.path().as_os_str(),
        "-o".as_ref(),
        all.as_os_str(),
    ]));
    assert_eq!(fs::read(all.join("sound/desktop.ini")).unwrap(), b"[fr]");
}

#[test]
fn list_and_check_work_on_a_folder() {
    let t = tempfile::tempdir().unwrap();
    write_archive(
        &t.path().join("data1.doi"),
        "storing",
        &sample_files(),
        false,
    );
    write_archive(
        &t.path().join("data2.doi"),
        "storing",
        &[File {
            dir: "game/data",
            name: "extra.pak",
            data: vec![9; 1000],
        }],
        false,
    );
    fs::write(t.path().join("Setup.exe"), "MZ not an archive").unwrap();
    let o = unrepack(&["list".as_ref(), t.path().join("data1.doi").as_os_str()]);
    ok(&o);
    let out = t.path().join("out");
    let o = unrepack(&[
        "extract".as_ref(),
        t.path().as_os_str(),
        "-o".as_ref(),
        out.as_os_str(),
        "--check".as_ref(),
        "--free-as-read".as_ref(),
    ]);
    ok(&o);
    let s = String::from_utf8_lossy(&o.stdout);
    assert!(s.contains("2 data file(s)") && s.contains("it fits"), "{s}");
}
