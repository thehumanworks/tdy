//! Portable reader over `std::fs`. Used on platforms without a specialised
//! reader, and in tests as a reference implementation.

use crate::entry::{EntryInfo, Kind, ReadOptions};
use std::ffi::CStr;
use std::io;

pub struct Reader {
    _opts: ReadOptions,
}

/// Never constructed on this platform: `read_split` stats every entry
/// itself (bulk attribute reads return sizes with the names).
pub struct Rest {
    _never: std::convert::Infallible,
}

impl Rest {
    pub fn len(&self) -> usize {
        match self._never {}
    }
    pub fn is_empty(&self) -> bool {
        match self._never {}
    }
    pub fn name(&self, _i: usize) -> &[u8] {
        match self._never {}
    }
    pub fn stat_range(&self, _range: std::ops::Range<usize>, _f: &mut dyn FnMut(usize, &crate::entry::EntryInfo<'_>)) {
        match self._never {}
    }
}

impl Reader {
    /// Same as `read` here; see the Linux reader for the parallel split.
    pub fn read_split(
        &mut self,
        path: &std::ffi::CStr,
        _inline: usize,
        f: &mut dyn FnMut(&crate::entry::EntryInfo<'_>),
    ) -> std::io::Result<Option<Rest>> {
        self.read(path, f).map(|_| None)
    }

    pub fn new(opts: ReadOptions) -> Self {
        Reader { _opts: opts }
    }

    pub fn read(&mut self, path: &CStr, f: &mut dyn FnMut(&EntryInfo<'_>)) -> io::Result<()> {
        let p = crate::path::cstr_to_path(path);
        for ent in std::fs::read_dir(p)? {
            let Ok(ent) = ent else { continue };
            let Ok(md) = ent.metadata() else { continue };
            let ft = md.file_type();
            let kind = if ft.is_dir() {
                Kind::Dir
            } else if ft.is_symlink() {
                Kind::Symlink
            } else if ft.is_file() {
                Kind::File
            } else {
                Kind::Other
            };
            let name = ent.file_name();
            #[cfg(unix)]
            let (nb, alloc, ino, dev, nlink, mtime) = {
                use std::os::unix::ffi::OsStrExt;
                use std::os::unix::fs::MetadataExt;
                (name.as_bytes().to_vec(), md.blocks() * 512, md.ino(), md.dev(), md.nlink() as u32, md.mtime())
            };
            #[cfg(not(unix))]
            let (nb, alloc, ino, dev, nlink, mtime) = {
                let mtime =
                    md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0);
                (name.to_string_lossy().as_bytes().to_vec(), md.len(), 0u64, 0u64, 1u32, mtime)
            };
            f(&EntryInfo { name: &nb, kind, logical: md.len(), alloc, private: None, ino, dev, nlink, mtime, flags: 0 });
        }
        Ok(())
    }
}

pub fn supports_sharing(_path: &CStr) -> bool {
    false
}
