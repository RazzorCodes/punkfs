pub mod cache;
pub mod punkfs;

struct PunkFS {}

impl Filesystem for PunkFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        if u64::from(parent) == 1 && name.to_str() == Some("hello.txt") {
            reply.entry(&TTL, &HELLO_TXT_ATTR, fuser::Generation(0));
        } else if u64::from(parent) == 1 && name.to_str() == Some("stats-per-thread") {
            let content = self.stats_content();
            let mut attr = STATS_PER_THREAD_ATTR;
            attr.size = content.len() as u64;
            // Must use zero TTL, otherwise previous size is cached.
            reply.entry(&Duration::ZERO, &attr, fuser::Generation(0));
        } else {
            reply.error(Errno::ENOENT);
        }
    }
}
