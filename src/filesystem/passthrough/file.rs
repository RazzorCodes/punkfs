//! Direct positional file operations against the covered directory.
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;

use super::error;
use crate::store::StoreError;

pub(super) fn read(path: &Path, offset: u64, size: u32) -> Result<Vec<u8>, StoreError> {
    let file = File::open(path).map_err(error)?;
    let mut data = vec![0; size as usize];
    let count = file.read_at(&mut data, offset).map_err(error)?;
    data.truncate(count);
    Ok(data)
}

pub(super) fn write(path: &Path, offset: u64, data: &[u8]) -> Result<u32, StoreError> {
    let file = OpenOptions::new().write(true).open(path).map_err(error)?;
    Ok(file.write_at(data, offset).map_err(error)? as u32)
}

pub(super) fn fsync(path: &Path, datasync: bool) -> Result<(), StoreError> {
    let file = File::open(path).map_err(error)?;
    if datasync {
        file.sync_data()
    } else {
        file.sync_all()
    }
    .map_err(error)
}
