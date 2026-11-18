# unrepack

Extracts game repacks built on FreeArc (DODI, FitGirl and similar) without running the installer, and can give the repack's space back while it is read, so the repack and the game never have to fit on the disk together. Inspired by [HatchZip](https://github.com/xevrion/hatchzip). Version 0.1, Windows only.

## Status

| Part | State |
|---|---|
| FreeArc index (footer, directory, every CRC) | Works on real DODI data files (Crusader Kings III: 55,412 entries, 0.06 s) |
| Decode chain through `unrepack-clshost.exe` | Works with the repack's real `cls-lolz` and `cls-srep`: CK3 `data2.doi`, 1,177 files and 496,166,375 bytes, every CRC matching, verify plus extract in about 10 s; `data1.doi` (5.74 GB block): 51,467 files, 19,371,573,360 bytes, every CRC matching, about 5.5 min for verify plus extract, decoders' temp files 0 B |
| Freeing as read (`--free-as-read`) | Works with real decoders: a copy of CK3 `data2.doi` was freed as read and deleted, its 1,177 files identical to a run that kept it. Not yet run on `data1.doi` |
| Resume after a crash | Not in 0.1 |
| Installer steps (registry, redistributables, language choice) | Not done, and not planned: only the game files are extracted |

## Use

```bash
unrepack list "Game [DODI Repack]\data1.doi"
unrepack extract "Game [DODI Repack]" -o "D:\Games\Game" --decoders <folder> --check --free-as-read
unrepack extract "Game [DODI Repack]" -o "D:\Games\Game" --decoders <folder> --free-as-read
```

`--decoders` is a folder holding the repack's `cls-*.dll`, `cls-*_x64.exe`, `cls-*_x86.exe` and `cls.ini`, as its installer unpacks them. Those are programs from the download, and unrepack runs them. Carved straight out of `Setup.exe` they must have Inno's CALL/JMP filter undone first (`repack-probe/inno_chunks.py` does).

Known gap: if a decoder's own temp file hits a full disk during `--free-as-read`, the decoder fails and the repack is lost; the measured check above is the only guard.

Modes, with the space each needs measured by `--check` on Crusader Kings III [DODI Repack] (6.05 GB packed, 19.87 GB of game):

| Mode | Needs free | Crash safe |
|---|---|---|
| Default | 19.99 GB | Yes, the repack is kept |
| `--delete-each` | 19.49 GB | Yes, each data file is deleted only once it is fully extracted |
| `--free-as-read` | 13.94 GB | **No.** A crash or a decoder failure after the first freed byte loses the repack |

`--free-as-read` safety net: a verify pass decodes everything and checks every file's CRC before anything is freed (`--no-verify` skips it), and measures the decoders' temp files and how far the output runs ahead of the input, then checks the free space again with those numbers; a full disk pauses the run instead of ending it; a marker file `<data file>.unrepack-consumed` makes any later run refuse a half-freed file; a decoder's error dialog is written to the log instead of waiting for a click. The `--check` figures in the table come before any decoding: they assume the freed input keeps pace with the output and leave out the decoders' temp files, which only the verify pass measures.

## Build and test

```bash
cargo build --release
gcc -m32 -O2 -s -static -o target/release/unrepack-clshost.exe clshost/clshost.c
cargo test --release
```

The tests write FreeArc archives in the measured layout and run the binary on them, with `storing` and `copy`, a CLS codec built from `clshost/test_copy.c`, so host processes run as with a repack's decoders. `UNREPACK_SAMPLE=<data file> cargo test real_sample` also parses a real one.
