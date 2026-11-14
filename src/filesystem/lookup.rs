//! Path resolution and metadata queries: lookup, getattr.
//!
//! These are the two ops every other filesystem activity starts with:
//! the kernel asks `lookup` to turn a (parent dir, name) pair into an
//! inode, and `getattr` to describe any inode.
use std::ffi::OsStr;
use std::path::Path;

use fuser::Errno;
use fuser::FileHandle;
use fuser::Generation;
use fuser::INodeNo;
use fuser::ReplyAttr;
use fuser::ReplyEntry;
use fuser::Request;

use super::Fs;
use crate::TIMEOUT;

pub(crate) fn lookup(fs: &Fs, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
    // 1. Resolve the parent's inode to its path in the store.
    let Some(parent_path) = fs.store.get_path(parent.0) else {
        reply.error(Errno::ENOENT); // unknown parent => the dir doesn't exist
        return;
    };

    // 2. Make sure the server's listing of this dir is stored (no-op if
    //    it is; swallows errors so lookups on offline dirs keep working).
    let _ = fs.store.ensure_listed(&parent_path);

    // 3. Join parent + child name and look up that path's inode.
    let full_path = Path::new(&parent_path).join(name);
    let Some(full_path) = full_path.to_str() else {
        // Non-UTF8 name (punkfs paths are Strings for now)
        reply.error(Errno::EINVAL);
        return;
    };
    let Some(inode) = fs.store.get_inode(full_path) else {
        reply.error(Errno::ENOENT); // name not in the (freshly fetched) listing
        return;
    };
    // 4. Serve the entry: inode + attrs, with a zero TTL so the kernel
    //    re-asks us instead of caching (valid while our attrs are fake).
    match fs.attr_for(inode, req.uid(), req.gid()) {
        Some(attr) => reply.entry(&TIMEOUT, &attr, Generation(0)),
        None => reply.error(Errno::ENOENT),
    }
}

pub(crate) fn getattr(fs: &Fs, req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
    // Straight attr lookup by inode: no path resolution, no server round
    // trip — everything comes from `attr_for`.
    match fs.attr_for(ino.0, req.uid(), req.gid()) {
        Some(attr) => reply.attr(&TIMEOUT, &attr),
        None => reply.error(Errno::ENOENT), // unknown inode
    }
}
