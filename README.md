<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/logo-dark.svg">
    <img src="docs/logo-light.svg" width="400" alt="unrepack logo: a green tile with three white layers, the top one lifting off">
  </picture>
</p>

<h1 align="center">unrepack</h1>

<h3 align="center">Unpack game repacks without their installer</h3>

<p align="center">
  <a href="https://github.com/ArekatlaNishanthchowdary/unrepack/releases/latest"><img src="https://img.shields.io/github/v/release/ArekatlaNishanthchowdary/unrepack?style=for-the-badge&label=release&color=1f6feb" alt="Latest release" /></a>
  <a href="#license"><img src="https://img.shields.io/badge/license-MIT%20%2F%20Apache--2.0-e8743b?style=for-the-badge" alt="MIT or Apache-2.0 licensed, except src/builtin.rs and src/tornado.rs" /></a>
  <a href="https://github.com/ArekatlaNishanthchowdary/unrepack/releases"><img src="https://img.shields.io/github/downloads/ArekatlaNishanthchowdary/unrepack/total?style=for-the-badge&label=downloads&color=3fb950" alt="Downloads" /></a>
  <a href="https://github.com/sponsors/ArekatlaNishanthchowdary"><img src="https://img.shields.io/badge/sponsor-ea4aaa?style=for-the-badge&logo=githubsponsors&logoColor=white" alt="Sponsor on GitHub" /></a>
  <a href="https://github.com/ArekatlaNishanthchowdary/unrepack/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/ArekatlaNishanthchowdary/unrepack/ci.yml?branch=master&style=for-the-badge&label=ci" alt="CI status" /></a>
  <img src="https://img.shields.io/badge/platform-windows-4ca571?style=for-the-badge" alt="Windows only" />
</p>

<h4 align="center">
  <a href="https://github.com/ArekatlaNishanthchowdary/unrepack/releases/latest">Download</a> ·
  <a href="#usage">Usage</a> ·
  <a href="#status">Status</a> ·
  <a href="#is-unrepack-safe">Safety</a> ·
  <a href="#how-it-works">How it works</a> ·
  <a href="#build-and-test">Build</a> ·
  <a href="#license">License</a> ·
  <a href="#sponsor">Sponsor</a>
</h4>

<p align="center">
  <img src="docs/intro.gif" width="960" alt="A repack's data file, drawn as a stack of grey layers, peels away one layer at a time; each layer unpacks into files that stack up as the game, while a drive-usage bar grows only to the game's size and never reaches the repack-plus-game line an installer would need.">
</p>

<p align="center">
  The repack can give its space back while it is read, so the repack and the game<br>
  never have to fit on the disk <em>together</em>.
</p>

unrepack is a free, open-source app and command line tool for Windows. It extracts the data files of game repacks built on FreeArc, such as DODI and FitGirl repacks, straight into a game folder. It checks every file's CRC as it goes. It does not run the installer, so there is no installer UI, registry editing or redistributable setup. When the installer does more than unpack, such as rebuilding game files from pieces afterwards, unrepack reads those steps from the installer's script and follows them (see [Setup's install steps](#setups-install-steps)).

A repack's installer needs room for the repack **and** the whole game at once. Crusader Kings III [DODI Repack] is 6.05 GB packed and 19.87 GB installed, so it needs about 26 GB of disk. With `--free-as-read`, unrepack needs 13.94 GB. It gives each data file's bytes back to the disk as soon as they are decoded, so the space the repack frees holds the game files that follow.

## Usage

### Download

Get `unrepack-<version>-windows-x64.zip` from [the latest release](https://github.com/ArekatlaNishanthchowdary/unrepack/releases/latest) and unzip it anywhere. It holds the ready-to-run programs, `unrepack-gui.exe`, `unrepack.exe` and `unrepack-clshost.exe`, so you don't need the source code or Rust. The `.sha256` file next to it is the zip's SHA-256, to check the download with `Get-FileHash <zip>` in PowerShell.

### The app

Run `unrepack-gui.exe` as administrator; the repacks' decoders don't work without it. Pick the repack folder and where to install. If the repack offers languages or extras (movies, artwork, soundtracks), tick the ones you want, as in the installer: English starts ticked and the other languages don't. The decoders are read out of the repack's `Setup.exe`, so their folder can stay empty. Then choose what happens to the repack and press **Check space** or **Unpack**. The app runs `unrepack.exe` next to it and shows its progress and log, so it does exactly what the command line does, with the same checks. It asks before any mode that deletes or frees the repack, and before stopping a run that would lose it.

Keep `unrepack-gui.exe`, `unrepack.exe` and `unrepack-clshost.exe` in the same folder.

### See whether it fits

```bash
unrepack extract "Game [DODI Repack]" -o "D:\Games\Game" --check --free-as-read
```

`--check` reads every data file's index and the disk's free space, and changes nothing.

### Extract

```bash
unrepack extract "Game [DODI Repack]" -o "D:\Games\Game"                  # keep the repack
unrepack extract "Game [DODI Repack]" -o "D:\Games\Game" --delete-each    # delete each data file once it's out
unrepack extract "Game [DODI Repack]" -o "D:\Games\Game" --free-as-read   # free the repack while reading it
unrepack list "Game [DODI Repack]\data1.doi"                                # what's inside
unrepack parts "Game [DODI Repack]"                                         # which data files are languages or extras
unrepack decoders "Game [DODI Repack]\Setup.exe" -o <folder>               # copy the decoders out
```

The input can be a repack folder (every FreeArc data file in it is used) or one or more data files.

### Languages and extras

Repacks put each language and each extra in a data file of its own, and the installer asks which to install. `unrepack parts` sorts a folder's data files by name: DODI's `data<N>` and FitGirl's `fg-<N>` always install, language packs (`English.doi`, `fg-selective-french.bin`) are languages, and anything else is an extra. To choose, name the data files you want:

```bash
unrepack extract "Game [DODI Repack]\data1.doi" "Game [DODI Repack]\data2.doi" "Game [DODI Repack]\English.doi" -o "D:\Games\Game"
```

Language packs often hold the same file, such as `sound\desktop.ini`. As with the installer, the copy from the data file named later is kept, and unrepack says which. The sorting goes by file names alone. The installer's own choices and defaults are in its compiled script, which unrepack reads only for its install steps.

### Setup's install steps

Some repacks need more than unpacking. A FitGirl installer can unpack its own tools first, extract the data files in an order of its choosing, then run commands that rebuild game files from stored pieces, edit `.ini` files and delete what's left. unrepack reads those steps out of the compiled script in `Setup.exe` (Inno Setup 5.5) and does them in Setup's order. When the script only unpacks the data files, unrepack extracts them by name as usual.

- **Commands run as Setup runs them**, in the game folder or Setup's temp folder, and their output goes to the log. A non-zero exit code is logged, not fatal, because Setup ignores it too: FitGirl's batch scripts delete themselves as their last line, which `cmd` reports as an error.
- **Each rebuilt file must exist afterwards.** Setup names the file each rebuild step makes. If one is missing when the steps finish, the run fails and says the game folder is incomplete.
- **Setup's own files go in the game folder**, for the components being installed only: FitGirl's `_Redist` folder with its MD5 list and one hash list per language. If one of them has the same name as a file in the data files, unrepack refuses to run rather than guess.
- **Steps behind a wizard checkbox are left out**, such as low-RAM modes. So are registry entries and redistributables.
- **A script unrepack can't follow is refused before anything changes**: a program started through `Exec2`, or files unpacked somewhere other than the game or temp folder. A `Setup.exe` that can't be read at all, usually because an antivirus blocked it, is refused too, because extracting by name would quietly skip the rebuild.

The repack's own decoders are needed too: `cls-*.dll` plug-ins, their `cls-*_x64.exe` / `cls-*_x86.exe` helpers, `cls.ini`, and `arc.ini` with any external decoders it names (FitGirl's `rzw.exe`, for example). unrepack reads them out of the repack's `Setup.exe` (Inno Setup 5.5, as DODI and FitGirl use) into a temporary folder, the way the installer would put them in `{tmp}`, without running anything in it. Every file is checked against the SHA-1 the installer stores, Inno's CALL/JMP filter is undone, and `{app}` / `{tmp}` in the `.ini` files become the decoders' working folder. The folder is removed when the run ends. `--decoders <folder>` uses a folder of decoders instead, and so does a repack folder that already holds them. Antivirus programs often flag a repack's `Setup.exe` (Windows Defender removed FitGirl's as `HackTool:Win32/Crack!MTB` when unrepack opened it); without it, unrepack says so and needs `--decoders`.

| Option | Effect |
|---|---|
| `-o, --output <dir>` | Output folder |
| `--decoders <dir>` | Folder with the repack's decoders, instead of reading them out of `Setup.exe` (see above) |
| `--host <exe>` | The 32-bit CLS host. Default: `unrepack-clshost.exe` next to `unrepack.exe` |
| `--work <dir>` | Where decoders run and keep their temp files. Default: `<output>\.unrepack-work` |
| `--log <file>` | Also write every message, and a progress line every 5 s, to a file |
| `--check` | Plan only: say whether it fits, change nothing |
| `--delete-each` | Delete each data file once all of it is extracted. Crash safe |
| `--free-as-read` | Free each data file's bytes as they are decoded. **Not crash safe** (see [Safety](#is-unrepack-safe)) |
| `--no-verify` | Skip the decode-only pass that checks every CRC before anything is freed or deleted. Halves the time and removes the safety net |
| `-y, --yes` | Don't ask before freeing or deleting |

### Space each mode needs

Measured by `--check` on Crusader Kings III [DODI Repack] (6.05 GB packed, 19.87 GB of game):

| Mode | Needs free | Crash safe |
|---|---|---|
| Default | 19.99 GB | Yes, the repack is kept |
| `--delete-each` | 19.49 GB | Yes, each data file is deleted only once it is fully extracted |
| `--free-as-read` | 13.94 GB | **No.** A crash or a decoder failure after the first freed byte loses the repack |

The `--check` figures come before any decoding. They assume the freed input keeps pace with the output, and they leave out the decoders' temp files. The verify pass measures both and checks the space again before anything is freed.

## Status

Tested on real repacks. Every file's CRC-32 matched the one stored in the archive.

| Repack | Methods | Unpacked |
|---|---|---|
| Crusader Kings III (DODI), 2 data files | `cls-lolz` + `cls-srep` | 52,644 files, 19.9 GB, about 6 min |
| A FitGirl repack, 6 data files | `srep`, `magic2`, `mpzz`, `xt2png`, `dispack070`, `delta`, `4x4:rzw` | 14,015 files, 5.0 GB |
| A FitGirl repack with rebuild steps, 9 data files, English voice | `magic2` and others, then Setup's xtool and builder steps | 5,113 files, 88.1 GB, about 52 min |

Freeing the repack while reading it gave the same files as keeping it. Against FitGirl's MD5 list, 4,210 of 14,011 files of the first FitGirl repack match byte for byte: the rest are images and audio its installer encrypts after unpacking, plus one movie inside `Setup.exe`. That run was before unrepack followed Setup's steps. The repack with rebuild steps, installed by following them, matches FitGirl's MD5 list in every one of its 5,113 files.

No repack using `tor` or `lzma` has been tried yet. Archives made by FreeArc 0.67's own `Arc.exe` from 137 MB of game files, programs and text extract with every CRC matching. That covers `tor:1` to `tor:11`, `lzma` fast, normal and max, and `4x4` around both, including frames 4x4 stored because they didn't compress. FreeArc's `-m1` to `-m9` presets also use `rep`, `exe`, `grzip` or `ppmd`, so archives made with them are still refused.

| Part | State |
|---|---|
| FreeArc archives and CRCs, CLS plug-ins (`cls-*.dll`), `arc.ini` decoders | Works |
| Built-ins `lzma`, `tor`, `delta`, `dispack070`, and `4x4` around them or an `arc.ini` decoder | Works |
| Choosing languages and extras | Works, by data file name |
| Other built-ins (`ppmd`, `rep`, `exe`, `lzma2`, …), `4x4` around a CLS plug-in | Refused before anything changes |
| Resume after a crash | Not yet |
| Setup's install steps: tool archives, data file order, rebuild commands, `.ini` edits, deletes | Works, for Inno Setup 5.5 installers |
| Registry entries, redistributables, steps behind a wizard checkbox | Not done |

## Is unrepack safe

> [!WARNING]
> **unrepack runs the decoders that came with the download.** A repack's `cls-*.dll`, its helper `.exe` files and the programs `arc.ini` names are programs from the download. So are the tools and batch scripts that Setup's install steps run. Use repacks you trust.

- **Nothing is freed until the whole repack decodes.** Before `--free-as-read` or `--delete-each` changes anything, a verify pass decodes every data file and checks every file's CRC. That pass also measures the decoders' temp files and how far the output runs ahead of the input, and checks the free space again with those numbers. `--no-verify` skips it.
- **A full disk pauses the run instead of ending it.**
- **A half-freed data file is never used again.** A marker file, `<data file>.unrepack-consumed`, makes any later run refuse it.
- **The decoders can't hang the run.** A decoder's error dialog is written to the log instead of waiting for a click. Every decoder process goes into one job object: if one stage fails, the whole chain stops.
- **Built-in decoders don't trust the stream.** Where FreeArc's C code trusts sizes read from the data, the ports refuse bad ones.
- **Decoders stay in their folder.** An `arc.ini` entry that names a program outside the decoders folder is refused.

What it can't protect against yet: with `--free-as-read`, a crash, a power cut, or a decoder failure after the first byte is freed loses the repack. That includes a decoder's own temp file hitting a full disk, which the measured check above is the only guard against. Use `--delete-each`, or keep the repack, when it can't be downloaded again.

## How it works

A FreeArc data file is a series of solid blocks, each packed by a chain of methods (for example `srep+dispack070+delta+rzwb`), with a directory at the end that lists every file, its block and its CRC. unrepack reads that directory, then decodes each block by running the chain from right to left as a pipeline of stages:

- **CLS plug-ins** are 32-bit DLLs. Each one runs in `unrepack-clshost.exe`, a small 32-bit host process, and data goes through pipes.
- **External decoders** from `arc.ini` run as processes in the work folder.
- **`4x4`**, **`delta`**, **`dispack070`**, **`tor`** and **`lzma`** run as threads inside unrepack. `delta`, `dispack070` and `tor` are Rust ports of FreeArc 0.67's decoders, and the tables of the first two match the repack's own `unarc.dll` byte for byte. `lzma` is FreeArc's raw LZMA stream, with its settings taken from the method string.

The last stage's output is cut into files, and each file's CRC is checked. With `--free-as-read`, unrepack zeroes the parts of the data file it has read as a sparse range (`FSCTL_SET_ZERO_DATA`), so NTFS gives that space back while the file keeps its size and offsets. Once a data file is fully read, it is deleted.

## Build and test

Only needed to change unrepack; to use it, [download](#download) the zip. Windows, with [Rust](https://rustup.rs) and a 32-bit MinGW `gcc` for the CLS host:

```bash
cargo build --release
gcc -m32 -O2 -s -static -o target/release/unrepack-clshost.exe clshost/clshost.c
cargo test --release
```

`cargo build --release` builds both `unrepack.exe` and the app, `unrepack-gui.exe` (egui, a single native exe). The tests write FreeArc archives in the layout measured from real repacks and run the binary on them. They use `storing`, `copy`, and a CLS codec built from `clshost/test_copy.c`, so the host process runs as it does with a repack's decoders. `delta` and `dispack070` are tested on hand-built streams. `tor` and `lzma` are tested on streams made by FreeArc 0.67's own Tornado compressor and by liblzma, and `4x4` around them on streams cut from archives FreeArc 0.67's `Arc.exe` made (see [testdata/README.md](testdata/README.md)). `UNREPACK_SAMPLE=<data file> cargo test real_sample` also parses a real data file, and `UNREPACK_DUMP=<dir>` saves every stage's output for debugging.

## Sponsor

If unrepack saved you a download or some disk space, you can support it on [GitHub Sponsors](https://github.com/sponsors/ArekatlaNishanthchowdary). In India, you can also pay over UPI: scan the code with any UPI app, or pay `nishanthchowdary1234@pingpay`.

<p align="center">
  <img src="docs/upi-qr.png" width="240" alt="UPI QR code for nishanthchowdary1234@pingpay">
</p>

## License

unrepack is licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option, **except `src/builtin.rs` and `src/tornado.rs`**.

`src/builtin.rs` holds the ports of FreeArc's `delta` and `dispack070` decoders, and `src/tornado.rs` the port of its `tor` decoder. Both stay under the terms of the FreeArc source they come from ([LICENSE-FREEARC](LICENSE-FREEARC)). The `delta` decoder is free to use only for decompressing FreeArc archives, and it is not open source. `dispack070` and `tor` come from FreeArc's GPL-2.0 tree. Without those two files, unrepack is open source throughout, but it can't decode repacks that use those three methods.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in unrepack by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
