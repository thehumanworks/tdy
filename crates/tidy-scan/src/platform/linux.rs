//! Linux reader: `openat` + `getdents64` + `statx` relative to the open
//! directory fd, so the kernel never re-resolves the full path per file.

use crate::entry::{EntryInfo, Kind, ReadOptions, flags};
use std::ffi::CStr;
use std::io;

const DIRENT_BUF: usize = 64 * 1024;

pub struct Reader {
    buf: Vec<u64>, // u64 for 8-byte alignment of linux_dirent64
    opts: ReadOptions,
    fiemap: Vec<u8>,
}

impl Reader {
    pub fn new(opts: ReadOptions) -> Self {
        Reader { buf: vec![0u64; DIRENT_BUF / 8], opts, fiemap: Vec::new() }
    }

    /// Read every entry of the directory at `path` (NUL-terminated), calling
    /// `f` for each. `.` and `..` are skipped. Entries that vanish between
    /// `getdents64` and `statx` are skipped silently.
    pub fn read(&mut self, path: &CStr, f: &mut dyn FnMut(&EntryInfo<'_>)) -> io::Result<()> {
        let fd =
            unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NOCTTY) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let _guard = FdGuard(fd);
        let private = self.opts.private;
        loop {
            let n = unsafe { libc::syscall(libc::SYS_getdents64, fd, self.buf.as_mut_ptr() as *mut libc::c_void, DIRENT_BUF) };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            if n == 0 {
                break;
            }
            let base = self.buf.as_ptr() as *const u8;
            let mut off = 0usize;
            while off < n as usize {
                // struct linux_dirent64 { u64 d_ino; i64 d_off; u16 d_reclen; u8 d_type; char d_name[]; }
                let rec = unsafe { base.add(off) };
                let reclen = unsafe { std::ptr::read_unaligned(rec.add(16) as *const u16) } as usize;
                let name_ptr = unsafe { rec.add(19) } as *const libc::c_char;
                off += reclen;
                let name = unsafe { CStr::from_ptr(name_ptr) };
                let nb = name.to_bytes();
                if nb == b"." || nb == b".." {
                    continue;
                }
                let mut stx: libc::statx = unsafe { std::mem::zeroed() };
                let rc = unsafe {
                    libc::statx(
                        fd,
                        name_ptr,
                        libc::AT_SYMLINK_NOFOLLOW | libc::AT_STATX_DONT_SYNC | libc::AT_NO_AUTOMOUNT,
                        libc::STATX_TYPE | libc::STATX_SIZE | libc::STATX_BLOCKS | libc::STATX_INO | libc::STATX_NLINK | libc::STATX_MTIME,
                        &mut stx,
                    )
                };
                if rc != 0 {
                    continue; // raced with a delete, or EACCES on a single entry
                }
                let kind = match (stx.stx_mode as u32) & libc::S_IFMT {
                    libc::S_IFREG => Kind::File,
                    libc::S_IFDIR => Kind::Dir,
                    libc::S_IFLNK => Kind::Symlink,
                    _ => Kind::Other,
                };
                let alloc = stx.stx_blocks * 512;
                let mut fl = 0u32;
                if stx.stx_attributes & (libc::STATX_ATTR_COMPRESSED as u64) != 0 {
                    fl |= flags::COMPRESSED;
                }
                if stx.stx_attributes & (libc::STATX_ATTR_IMMUTABLE as u64) != 0 {
                    fl |= flags::IMMUTABLE;
                }
                let priv_bytes =
                    if private && kind == Kind::File && alloc > 0 { fiemap_private(fd, name, alloc, &mut self.fiemap) } else { None };
                f(&EntryInfo {
                    name: nb,
                    kind,
                    logical: stx.stx_size,
                    alloc,
                    private: priv_bytes,
                    ino: stx.stx_ino,
                    dev: libc::makedev(stx.stx_dev_major, stx.stx_dev_minor),
                    nlink: stx.stx_nlink,
                    mtime: stx.stx_mtime.tv_sec,
                    flags: fl,
                });
            }
        }
        Ok(())
    }
}

struct FdGuard(libc::c_int);
impl Drop for FdGuard {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

// ---- FIEMAP (shared-extent accounting on btrfs / xfs / ocfs2) -------------

const FS_IOC_FIEMAP: libc::c_ulong = 0xC020_660B;
const FIEMAP_EXTENT_LAST: u32 = 0x0000_0001;
const FIEMAP_EXTENT_SHARED: u32 = 0x0000_2000;
const FIEMAP_HDR: usize = 32; // struct fiemap
const FIEMAP_EXT: usize = 56; // struct fiemap_extent
const EXTENTS_PER_CALL: usize = 128;

/// Bytes of `name` (relative to `dirfd`) that are not shared with any other
/// file. Returns `None` when the filesystem cannot answer.
fn fiemap_private(dirfd: libc::c_int, name: &CStr, alloc: u64, buf: &mut Vec<u8>) -> Option<u64> {
    let fd = unsafe { libc::openat(dirfd, name.as_ptr(), libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK) };
    if fd < 0 {
        return None;
    }
    let _g = FdGuard(fd);
    shared_bytes_fd(fd, buf).map(|shared| alloc.saturating_sub(shared))
}

/// Sum of extent bytes flagged `FIEMAP_EXTENT_SHARED` for an open file.
pub fn shared_bytes_fd(fd: libc::c_int, buf: &mut Vec<u8>) -> Option<u64> {
    let size = FIEMAP_HDR + FIEMAP_EXT * EXTENTS_PER_CALL;
    buf.clear();
    buf.resize(size + 8, 0);
    // align to 8
    let shift = buf.as_ptr().align_offset(8);
    let mut start: u64 = 0;
    let mut shared: u64 = 0;
    loop {
        let p = unsafe { buf.as_mut_ptr().add(shift) };
        unsafe {
            std::ptr::write_bytes(p, 0, size);
            // fm_start, fm_length, fm_flags, fm_mapped_extents, fm_extent_count, fm_reserved
            std::ptr::write(p as *mut u64, start);
            std::ptr::write(p.add(8) as *mut u64, u64::MAX - start);
            std::ptr::write(p.add(16) as *mut u32, 0);
            std::ptr::write(p.add(24) as *mut u32, EXTENTS_PER_CALL as u32);
        }
        if unsafe { libc::ioctl(fd, FS_IOC_FIEMAP as _, p) } != 0 {
            return None;
        }
        let mapped = unsafe { std::ptr::read(p.add(20) as *const u32) } as usize;
        if mapped == 0 {
            return Some(shared);
        }
        let mut last = false;
        let mut next = start;
        for i in 0..mapped {
            let e = unsafe { p.add(FIEMAP_HDR + i * FIEMAP_EXT) };
            let logical = unsafe { std::ptr::read(e as *const u64) };
            let length = unsafe { std::ptr::read(e.add(16) as *const u64) };
            let fl = unsafe { std::ptr::read(e.add(40) as *const u32) };
            if fl & FIEMAP_EXTENT_SHARED != 0 {
                shared += length;
            }
            if fl & FIEMAP_EXTENT_LAST != 0 {
                last = true;
            }
            next = logical + length;
        }
        if last || mapped < EXTENTS_PER_CALL {
            return Some(shared);
        }
        start = next;
    }
}

/// Filesystem type name for the filesystem holding `path`.
pub fn fs_type(path: &CStr) -> Option<&'static str> {
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(path.as_ptr(), &mut s) } != 0 {
        return None;
    }
    Some(match s.f_type as i64 {
        0x9123_683E => "btrfs",
        0x5846_5342 => "xfs",
        0xEF53 => "ext4",
        0x0102_1994 => "tmpfs",
        0x794C_7630 => "overlay",
        0x6573_5546 => "fuse",
        0x2FC1_2FC1 => "zfs",
        0xF2F5_2010 => "f2fs",
        0x6969 => "nfs",
        _ => "other",
    })
}

/// Whether files on this filesystem can share extents (reflinks).
pub fn supports_sharing(path: &CStr) -> bool {
    matches!(fs_type(path), Some("btrfs" | "xfs"))
}
