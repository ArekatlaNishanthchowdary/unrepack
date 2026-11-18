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
