//! Output: messages go to the console and, with `--log`, to a file too; a
//! live status line is drawn on the console only, and written to the log
//! every few seconds as a plain line.

use std::fs::{File, OpenOptions};
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};

static LOG: Mutex<Option<File>> = Mutex::new(None);
/// Width of the status line on screen, 0 when none is drawn.
static STATUS: AtomicUsize = AtomicUsize::new(0);

pub fn open(path: &Path) -> Result<()> {
    let f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening the log {}", path.display()))?;
    *LOG.lock().unwrap() = Some(f);
    Ok(())
}

fn clear_status() {
    let w = STATUS.swap(0, Ordering::Relaxed);
    if w > 0 {
        eprint!("\r{}\r", " ".repeat(w));
    }
}

/// A message, on stdout (`err` false) or stderr, and in the log.
pub fn line(text: &str, err: bool) {
    clear_status();
    if err {
        eprintln!("{text}");
    } else {
        println!("{text}");
    }
    to_file(text);
}

pub fn to_file(text: &str) {
    if let Some(f) = LOG.lock().unwrap().as_mut() {
        let _ = writeln!(f, "{text}");
    }
}

pub fn console() -> bool {
    std::io::stderr().is_terminal()
}

/// Redraws the status line in place.
pub fn status(text: &str) {
    let w = STATUS.load(Ordering::Relaxed);
    let pad = w.saturating_sub(text.len());
    eprint!("\r{text}{}", " ".repeat(pad));
    let _ = std::io::stderr().flush();
    STATUS.store(text.len(), Ordering::Relaxed);
}

#[macro_export]
macro_rules! say {
    ($($t:tt)*) => { $crate::log::line(&format!($($t)*), false) };
}

#[macro_export]
macro_rules! note {
    ($($t:tt)*) => { $crate::log::line(&format!($($t)*), true) };
}
