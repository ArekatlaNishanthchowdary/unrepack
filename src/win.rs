//! The Windows calls: freeing a range of a file without changing its length
//! (sparse zeroing, adapted from HatchZip's `platform/windows.rs`, MIT or
//! Apache-2.0), free space, and which volume a path is on.

use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, GetDiskFreeSpaceExW,
    GetFileInformationByHandle, GetVolumeInformationByHandleW,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    FILE_ZERO_DATA_INFORMATION, FSCTL_SET_SPARSE, FSCTL_SET_ZERO_DATA,
};

/// `FILE_SUPPORTS_SPARSE_FILES`, which windows-sys keeps under SystemServices.
const FILE_SUPPORTS_SPARSE_FILES: u32 = 0x40;

/// NTFS frees sparse ranges in units of 16 clusters, at most 1 MiB, so
/// punches are rounded inwards to that.
pub const UNIT: u64 = 1 << 20;

/// One job object holds every decoder process and whatever those start, so a
/// failed stage can stop the whole chain (a CLS shim's helper exe outlives
/// its host and keeps the pipes open otherwise), and decoders die with us.
fn job() -> usize {
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    static JOB: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *JOB.get_or_init(|| unsafe {
        let j = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if !j.is_null() {
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                j,
                JobObjectExtendedLimitInformation,
                (&raw const info).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
        }
        j as usize
    })
}

/// Puts a decoder in the job. ponytail: assigned just after spawn, so a
/// grandchild started in that instant would escape; spawn suspended if seen.
pub fn adopt(child: &std::process::Child) {
    let j = job();
    if j != 0 {
        unsafe {
            windows_sys::Win32::System::JobObjects::AssignProcessToJobObject(
                j as _,
                child.as_raw_handle() as _,
            )
        };
    }
}

/// Kills every decoder. The job stays usable for later ones.
pub fn stop_decoders() {
    let j = job();
    if j != 0 {
        unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(j as _, 1) };
    }
}

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain([0]).collect()
}

fn ioctl(file: &File, code: u32, input: *const core::ffi::c_void, len: u32) -> io::Result<()> {
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            code,
            input,
            len,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Whether the volume holding `file` can free ranges of a file.
pub fn sparse_supported(file: &File) -> bool {
    let mut flags = 0u32;
    let ok = unsafe {
        GetVolumeInformationByHandleW(
            file.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut flags,
            std::ptr::null_mut(),
            0,
        )
    };
    ok != 0 && flags & FILE_SUPPORTS_SPARSE_FILES != 0
}

/// Marks the file sparse. Without it NTFS answers a zeroing request by
/// writing real zeros, which frees nothing.
pub fn set_sparse(file: &File) -> io::Result<()> {
    ioctl(file, FSCTL_SET_SPARSE, std::ptr::null(), 0)
}

/// Frees `[start, end)` rounded inwards to [`UNIT`]. The file must be open
/// for writing and already sparse.
pub fn punch(file: &File, start: u64, end: u64) -> io::Result<()> {
    let s = start.next_multiple_of(UNIT);
    let e = end / UNIT * UNIT;
    if s >= e {
        return Ok(());
    }
    let range = FILE_ZERO_DATA_INFORMATION {
        FileOffset: s as i64,
        BeyondFinalZero: e as i64,
    };
    ioctl(
        file,
        FSCTL_SET_ZERO_DATA,
        (&raw const range).cast(),
        size_of::<FILE_ZERO_DATA_INFORMATION>() as u32,
    )
}

/// Bytes the file really occupies, less than its length once punched.
#[cfg(test)]
pub fn allocated(path: &Path) -> io::Result<u64> {
    let mut high = 0u32;
    let low = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetCompressedFileSizeW(
            wide(path).as_ptr(),
            &mut high,
        )
    };
    if low == u32::MAX {
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(0) {
            return Err(e);
        }
    }
    Ok((u64::from(high) << 32) | u64::from(low))
}

/// Free bytes available to this user on the volume holding `dir`.
pub fn free_space(dir: &Path) -> io::Result<u64> {
    let mut free = 0u64;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide(dir).as_ptr(),
            &mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(free)
    }
}

/// The serial number of the volume holding `path`, file or folder.
pub fn volume(path: &Path) -> io::Result<u32> {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(f.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info.dwVolumeSerialNumber)
}

/// A file's MD5, through Windows' own BCrypt (no extra crate).
pub fn md5(path: &Path) -> io::Result<[u8; 16]> {
    use std::io::Read;
    use windows_sys::Win32::Security::Cryptography::{
        BCRYPT_MD5_ALG_HANDLE, BCryptCreateHash, BCryptDestroyHash, BCryptFinishHash,
        BCryptHashData,
    };
    let fail = |s: i32| io::Error::other(format!("MD5 failed (NTSTATUS {s:#x})"));
    let mut h = std::ptr::null_mut();
    let s = unsafe {
        BCryptCreateHash(
            BCRYPT_MD5_ALG_HANDLE,
            &mut h,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
            0,
            0,
        )
    };
    if s != 0 {
        return Err(fail(s));
    }
    let res = (|| {
        let mut f = File::open(path)?;
        let mut buf = vec![0u8; 4 << 20];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            let s = unsafe { BCryptHashData(h, buf.as_ptr(), n as u32, 0) };
            if s != 0 {
                return Err(fail(s));
            }
        }
        let mut out = [0u8; 16];
        let s = unsafe { BCryptFinishHash(h, out.as_mut_ptr(), 16, 0) };
        if s != 0 {
            return Err(fail(s));
        }
        Ok(out)
    })();
    unsafe { BCryptDestroyHash(h) };
    res
}

/// Memory Windows can still promise (commit limit minus what's committed),
/// and the limit itself.
fn commit_free() -> (u64, u64) {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    if unsafe { GlobalMemoryStatusEx(&mut m) } == 0 {
        return (u64::MAX, u64::MAX);
    }
    (m.ullAvailPageFile, m.ullTotalPageFile)
}

static LOWEST_FREE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);

/// Notes, twice a second for the rest of the run, the least memory Windows
/// had left to give. Decoders that can't get memory often just exit 1.
pub fn watch_memory() {
    std::thread::spawn(|| {
        loop {
            LOWEST_FREE.fetch_min(commit_free().0, std::sync::atomic::Ordering::Relaxed);
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    });
}

/// Said after a failure, if memory got nearly full during the run.
pub fn low_memory_note() -> Option<String> {
    let low = LOWEST_FREE.load(std::sync::atomic::Ordering::Relaxed);
    let limit = commit_free().1;
    // ponytail: 2 GB or 5% of the limit; the CONTROL failure had about 1 GB.
    (low < (2u64 << 30).max(limit / 20)).then(|| {
        format!(
            "Windows was nearly out of memory during this run (at one point only {:.1} GB of its \
             {:.1} GB limit was left), which can make a decoder quit without a message. Close other \
             programs or make the pagefile larger, then run again.",
            low as f64 / f64::from(1u32 << 30),
            limit as f64 / f64::from(1u32 << 30)
        )
    })
}

/// The console mode to put back, while QuickEdit is off.
pub struct QuickEditOff(Option<u32>);

/// Turns off the console's QuickEdit mode for the run: with it on, a click in
/// the window freezes every write to it, and the decoders (which share the
/// console) stall until Enter or Esc. No-op without a console (the GUI).
pub fn quick_edit_off() -> QuickEditOff {
    use windows_sys::Win32::System::Console::{
        ENABLE_EXTENDED_FLAGS, ENABLE_QUICK_EDIT_MODE, GetConsoleMode, GetStdHandle,
        STD_INPUT_HANDLE, SetConsoleMode,
    };
    unsafe {
        let h = GetStdHandle(STD_INPUT_HANDLE);
        let mut m = 0;
        let off = GetConsoleMode(h, &mut m) != 0
            && SetConsoleMode(h, (m & !ENABLE_QUICK_EDIT_MODE) | ENABLE_EXTENDED_FLAGS) != 0;
        QuickEditOff(off.then_some(m))
    }
}

impl Drop for QuickEditOff {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode};
        if let Some(m) = self.0 {
            unsafe { SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), m) };
        }
    }
}

/// ERROR_DISK_FULL and ERROR_HANDLE_DISK_FULL.
pub fn is_disk_full(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::StorageFull || matches!(e.raw_os_error(), Some(112 | 39))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn punching_frees_space_and_reads_back_zeros() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&p)
            .unwrap();
        f.write_all(&vec![7u8; 8 << 20]).unwrap();
        f.sync_all().unwrap();
        if !sparse_supported(&f) {
            eprintln!("temp volume can't free ranges; skipping");
            return;
        }
        let before = allocated(&p).unwrap();
        set_sparse(&f).unwrap();
        punch(&f, 100, 6 << 20).unwrap();
        f.sync_all().unwrap();
        let after = allocated(&p).unwrap();
        assert!(after + (4 << 20) <= before, "allocated {before} -> {after}");
        let data = std::fs::read(&p).unwrap();
        assert_eq!(data.len(), 8 << 20);
        assert!(
            data[..100].iter().all(|&b| b == 7),
            "bytes before the range kept"
        );
        assert!(data[UNIT as usize..6 << 20].iter().all(|&b| b == 0));
        assert!(data[6 << 20..].iter().all(|&b| b == 7));
    }
}
