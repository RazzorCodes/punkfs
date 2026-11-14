mod cache;
mod db;

use cache::Cache;

use fuser::Generation;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::time::Duration;
use std::time::SystemTime;

use fuser::Errno;
use fuser::FileAttr;
use fuser::FileHandle;
use fuser::FileType;
use fuser::Filesystem;
use fuser::INodeNo;
use fuser::MountOption;
use fuser::ReplyAttr;
use fuser::ReplyDirectory;
use fuser::ReplyEntry;
use fuser::Request;

#[path = "client/grpc/gen/punk_fs.rs"]
pub mod punk_fs;

pub struct PunkFs {
    cache: Cache,
}

impl PunkFs {
    pub fn new(server_url: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let cache = Cache::new(server_url)?; // root is seeded inside Cache::new
        Ok(PunkFs { cache })
    }

    pub fn attr_for(&self, inode: u64, uid: u32, gid: u32) -> Option<FileAttr> {
        let path = self.cache.get_path(inode)?;

        let (kind, nlink) = if path == "/" {
            (FileType::Directory, 2)
        } else {
            (FileType::RegularFile, 1)
        };

        let now = SystemTime::now();
        Some(FileAttr {
            ino: INodeNo(inode),
            size: 0,
            blocks: 0,
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            kind,
            perm: 0o777,
            nlink,
            uid,
            gid,
            rdev: 0,
            blksize: 512,
            flags: 0,
        })
    }

    pub fn mount(
        server_url: &str,
        mountpoint: &Path,
    ) -> Result<fuser::BackgroundSession, Box<dyn std::error::Error>> {
        let fs = PunkFs::new(server_url)?;

        let mut cfg = fuser::Config::default();
        cfg.mount_options = vec![MountOption::FSName("punkfs".into())];

        Ok(fuser::spawn_mount(fs, mountpoint, &cfg)?)
    }
}

const TTL: Duration = Duration::from_secs(0);

impl Filesystem for PunkFs {
    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(parent_path) = self.cache.get_path(parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };

        let _ = self.cache.ensure_listed(&parent_path);

        let full_path = Path::new(&parent_path).join(name);
        let Some(full_path) = full_path.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let Some(inode) = self.cache.get_inode(full_path) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match self.attr_for(inode, req.uid(), req.gid()) {
            Some(attr) => reply.entry(&TTL, &attr, Generation(0)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.attr_for(ino.0, req.uid(), req.gid()) {
            Some(attr) => reply.attr(&TTL, &attr),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let Some(dir_path) = self.cache.get_path(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let mut entries: Vec<(u64, FileType, OsString)> = vec![
            (ino.0, FileType::Directory, ".".into()),
            (ino.0, FileType::Directory, "..".into()),
        ];
        entries.extend(
            self.cache
                .get_children(&dir_path)
                .unwrap_or_default()
                .into_iter()
                .map(|(inode, name)| (inode, FileType::RegularFile, OsString::from(name))),
        );
        for (i, (child_ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(child_ino), (i + 1) as u64, kind, name) {
                break;
            }
        }
        reply.ok();
    }
}
