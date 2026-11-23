//! User-visible xattr operations. punkfs reserves its identity attribute.
use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

const GUID: &[u8] = b"user.punkfs.guid";
const MAX_XATTR: usize = 65536;

fn args(path: &Path, name: &[u8]) -> io::Result<(CString, CString)> {
    if name == GUID {
        return Err(io::Error::from_raw_os_error(libc::EPERM));
    }
    if !name.starts_with(b"user.") {
        return Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP));
    }
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    let name = CString::new(name).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    Ok((path, name))
}

pub(crate) fn get(path: &Path, name: &[u8]) -> io::Result<Vec<u8>> {
    let (path, name) = args(path, name)?;
    let mut data = vec![0u8; MAX_XATTR];
    let n = unsafe {
        libc::lgetxattr(
            path.as_ptr(),
            name.as_ptr(),
            data.as_mut_ptr().cast(),
            data.len(),
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    data.truncate(n as usize);
    Ok(data)
}

pub(crate) fn list(path: &Path) -> io::Result<Vec<u8>> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    let mut data = vec![0u8; MAX_XATTR];
    let n = unsafe { libc::llistxattr(path.as_ptr(), data.as_mut_ptr().cast(), data.len()) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    data.truncate(n as usize);
    let mut out = Vec::new();
    for name in data.split(|b| *b == 0) {
        if name.starts_with(b"user.") && name != GUID {
            out.extend_from_slice(name);
            out.push(0);
        }
    }
    Ok(out)
}

pub(crate) fn set(path: &Path, name: &[u8], value: &[u8], flags: i32) -> io::Result<()> {
    let (path, name) = args(path, name)?;
    if unsafe {
        libc::lsetxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            flags,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn remove(path: &Path, name: &[u8]) -> io::Result<()> {
    let (path, name) = args(path, name)?;
    if unsafe { libc::lremovexattr(path.as_ptr(), name.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
