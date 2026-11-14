pub struct FsEntry {
    pub inode: u64,
    pub path: String,
}

pub struct FsTree {
    pub entries: Vec<FsEntry>,
}

impl FsTree {
    pub fn get_inode(&self, path: &str) -> Option<u64> {
        self.entries
            .iter()
            .find(|e| e.path == path)
            .map(|e| e.inode)
    }

    pub fn get_path(&self, inode: u64) -> Option<&str> {
        self.entries
            .iter()
            .find(|e| e.inode == inode)
            .map(|e| e.path.as_str())
    }

    pub fn upsert_entry(&mut self, path: &str, inode: u64) -> Option<FsEntry> {
        let inode = if inode == 0 {
            self.entries.iter().map(|e| e.inode).max().unwrap_or(1) + 1
        } else {
            inode
        };

        let path = if path.is_empty() {
            self.entries.iter().find(|e| e.inode == inode)?.path.clone()
        } else {
            path.to_owned()
        };

        let entry = FsEntry { inode, path };

        let Some(first) = self
            .entries
            .iter()
            .position(|e| e.inode == entry.inode || e.path == entry.path)
        else {
            self.entries.push(entry);
            return None;
        };

        let mut idx = 0;
        self.entries.retain(|e| {
            let i = idx;
            idx += 1;
            i <= first || !(e.inode == entry.inode || e.path == entry.path)
        });

        Some(std::mem::replace(&mut self.entries[first], entry))
    }

    pub fn get_children_of(&self, path: &str) -> Vec<&FsEntry> {
        // "/" already ends with a slash; "/foo" needs one added so "/foobar" can't match
        let prefix = if path.ends_with('/') {
            path.to_owned()
        } else {
            format!("{path}/")
        };

        self.entries
            .iter()
            .filter(|e| {
                e.path
                    .strip_prefix(prefix.as_str())
                    .is_some_and(|rest| !rest.is_empty() && !rest.contains('/'))
            })
            .collect()
    }
}
