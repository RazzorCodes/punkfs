//! Local namespace mutations and attribute changes.
use super::error;
use crate::store::{SetattrChanges, StoreError};
use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

pub(super) fn setattr(path: &Path, changes: SetattrChanges) -> Result<(), StoreError> {
    if changes.accessed_nanos.is_some_and(|n| n >= 1_000_000_000)
        || changes.modified_nanos.is_some_and(|n| n >= 1_000_000_000)
    {
        return Err(StoreError::Local(libc::EINVAL));
    }
    if changes.uid.is_some() || changes.gid.is_some() {
        let raw = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| StoreError::Local(libc::EINVAL))?;
        if unsafe {
            libc::chown(
                raw.as_ptr(),
                changes.uid.unwrap_or(u32::MAX),
                changes.gid.unwrap_or(u32::MAX),
            )
        } != 0
        {
            return Err(error(io::Error::last_os_error()));
        }
    }
    if let Some(mode) = changes.mode {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(error)?;
    }
    if let Some(size) = changes.size {
        OpenOptions::new()
            .write(true)
            .open(path)
            .map_err(error)?
            .set_len(size)
            .map_err(error)?;
    }
    if changes.accessed_unix.is_some() || changes.modified_unix.is_some() {
        let raw = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| StoreError::Local(libc::EINVAL))?;
        let time = |value: Option<i64>, nanos: Option<u32>| libc::timespec {
            tv_sec: value.unwrap_or(0) as libc::time_t,
            tv_nsec: if value.is_some() {
                nanos.unwrap_or(0) as libc::c_long
            } else {
                libc::UTIME_OMIT
            },
        };
        let times = [
            time(changes.accessed_unix, changes.accessed_nanos),
            time(changes.modified_unix, changes.modified_nanos),
        ];
        if unsafe { libc::utimensat(libc::AT_FDCWD, raw.as_ptr(), times.as_ptr(), 0) } != 0 {
            return Err(error(io::Error::last_os_error()));
        }
    }
    Ok(())
}

pub(super) fn statfs(path: &Path) -> Result<crate::store::StatFs, StoreError> {
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| StoreError::Local(libc::EINVAL))?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(error(io::Error::last_os_error()));
    }
    let stat = unsafe { stat.assume_init() };
    Ok(crate::store::StatFs {
        blocks: stat.f_blocks,
        bfree: stat.f_bfree,
        bavail: stat.f_bavail,
        files: stat.f_files,
        ffree: stat.f_ffree,
        block_size: stat.f_bsize as u32,
        name_length: stat.f_namemax as u32,
        fragment_size: stat.f_frsize as u32,
    })
}

pub(super) fn rename(old: &Path, new: &Path, flags: fuser::RenameFlags) -> Result<(), StoreError> {
    let old = std::ffi::CString::new(old.as_os_str().as_bytes())
        .map_err(|_| StoreError::Local(libc::EINVAL))?;
    let new = std::ffi::CString::new(new.as_os_str().as_bytes())
        .map_err(|_| StoreError::Local(libc::EINVAL))?;
    let rc = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            old.as_ptr(),
            libc::AT_FDCWD,
            new.as_ptr(),
            flags.bits(),
        )
    };
    if rc != 0 {
        return Err(error(io::Error::last_os_error()));
    }
    Ok(())
}
