//! Platform-neutral view of one directory entry, as produced by a platform reader.

/// Object type of a directory entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Other,
}

/// Bit flags carried on an entry.
pub mod flags {
    /// File content is transparently compressed (APFS decmpfs, btrfs/ext4 compressed).
    pub const COMPRESSED: u32 = 1 << 0;
    /// Cloud placeholder whose content is not local (macOS `SF_DATALESS`).
    pub const DATALESS: u32 = 1 << 1;
    /// Hidden via file flag (macOS `UF_HIDDEN`); dotfiles are not flagged here.
    pub const HIDDEN: u32 = 1 << 2;
    /// Immutable / append-only (`UF_IMMUTABLE`, `SF_IMMUTABLE`, `STATX_ATTR_IMMUTABLE`).
    pub const IMMUTABLE: u32 = 1 << 3;
}

/// One entry returned by [`crate::platform::Reader::read`]. Borrowed from the
/// reader's buffer; copy what you need before the callback returns.
#[derive(Debug, Clone, Copy)]
pub struct EntryInfo<'a> {
    /// Raw name bytes (no NUL, no slash).
    pub name: &'a [u8],
    pub kind: Kind,
    /// Apparent length (`st_size`).
    pub logical: u64,
    /// Bytes allocated on disk (`st_blocks * 512`).
    pub alloc: u64,
    /// Bytes not shared with a clone or snapshot, when measured.
    pub private: Option<u64>,
    pub ino: u64,
    pub dev: u64,
    pub nlink: u32,
    /// Modification time, seconds since the Unix epoch.
    pub mtime: i64,
    pub flags: u32,
}

/// What a reader should collect.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReadOptions {
    /// Measure unshared ("private") bytes. Cheap on APFS (same bulk call);
    /// costs an open + FIEMAP per file on Linux, so off by default there.
    pub private: bool,
}
