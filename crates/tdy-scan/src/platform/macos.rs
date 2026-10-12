//! macOS reader: `getattrlistbulk(2)` returns name, type and sizes for many
//! entries per kernel crossing. With `FSOPT_ATTR_CMN_EXTENDED` the same call
//! can also vend `ATTR_CMNEXT_PRIVATESIZE` (bytes not shared with a clone or
//! snapshot) — when the volume supports it.
//!
//! Buffer layout per entry (see getattrlist(2)): u32 length, then attributes
//! in the order the man page describes them — RETURNED_ATTRS first, ERROR
//! next, then remaining common attrs in bit order, then dir attrs, file attrs,
//! and finally the extended common (forkattr) group. Each attribute is
//! 4-byte aligned, so 8-byte values are read unaligned.

use crate::entry::{EntryInfo, Kind, ReadOptions, flags};
use std::ffi::CStr;
use std::io;

const BULK_BUF: usize = 256 * 1024;

// Constants from <sys/attr.h>; spelled out so the parser below is self-describing.
const ATTR_BIT_MAP_COUNT: u16 = 5;
const ATTR_CMN_NAME: u32 = 0x0000_0001;
const ATTR_CMN_DEVID: u32 = 0x0000_0002;
const ATTR_CMN_OBJTYPE: u32 = 0x0000_0008;
const ATTR_CMN_MODTIME: u32 = 0x0000_0400;
const ATTR_CMN_FLAGS: u32 = 0x0004_0000;
const ATTR_CMN_FILEID: u32 = 0x0200_0000;
const ATTR_CMN_ERROR: u32 = 0x2000_0000;
const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000;
const ATTR_FILE_LINKCOUNT: u32 = 0x0000_0001;
const ATTR_FILE_TOTALSIZE: u32 = 0x0000_0002;
const ATTR_FILE_ALLOCSIZE: u32 = 0x0000_0004;
const ATTR_CMNEXT_PRIVATESIZE: u32 = 0x0000_0008;
const FSOPT_ATTR_CMN_EXTENDED: u64 = 0x0000_0020;

// fsobj_type_t values
const VREG: u32 = 1;
const VDIR: u32 = 2;
const VLNK: u32 = 5;

// chflags(2)
const UF_IMMUTABLE: u32 = 0x0000_0002;
const UF_COMPRESSED: u32 = 0x0000_0020;
const UF_HIDDEN: u32 = 0x0000_8000;
const SF_IMMUTABLE: u32 = 0x0002_0000;
const SF_DATALESS: u32 = 0x4000_0000;

#[repr(C)]
struct AttrList {
    bitmapcount: u16,
    reserved: u16,
    commonattr: u32,
    volattr: u32,
    dirattr: u32,
    fileattr: u32,
    forkattr: u32,
}

unsafe extern "C" {
    fn getattrlistbulk(dirfd: libc::c_int, alist: *mut AttrList, buf: *mut libc::c_void, size: libc::size_t, options: u64) -> libc::c_int;
}

pub struct Reader {
    buf: Vec<u64>,
    opts: ReadOptions,
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
        Reader { buf: vec![0u64; BULK_BUF / 8], opts }
    }

    pub fn read(&mut self, path: &CStr, f: &mut dyn FnMut(&EntryInfo<'_>)) -> io::Result<()> {
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let _g = FdGuard(fd);
        let mut al = AttrList {
            bitmapcount: ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: ATTR_CMN_RETURNED_ATTRS
                | ATTR_CMN_NAME
                | ATTR_CMN_ERROR
                | ATTR_CMN_DEVID
                | ATTR_CMN_OBJTYPE
                | ATTR_CMN_MODTIME
                | ATTR_CMN_FLAGS
                | ATTR_CMN_FILEID,
            volattr: 0,
            dirattr: 0,
            fileattr: ATTR_FILE_LINKCOUNT | ATTR_FILE_TOTALSIZE | ATTR_FILE_ALLOCSIZE,
            forkattr: if self.opts.private { ATTR_CMNEXT_PRIVATESIZE } else { 0 },
        };
        let options = if self.opts.private { FSOPT_ATTR_CMN_EXTENDED } else { 0 };
        let mut erange_retry = false;
        loop {
            let n = unsafe { getattrlistbulk(fd, &mut al, self.buf.as_mut_ptr() as *mut libc::c_void, BULK_BUF, options) };
            if n < 0 {
                let e = io::Error::last_os_error();
                // APFS quirk: when the previous call exactly filled the buffer the
                // next call can return ERANGE; calling again returns 0.
                if e.raw_os_error() == Some(libc::ERANGE) && !erange_retry {
                    erange_retry = true;
                    continue;
                }
                return Err(e);
            }
            if n == 0 {
                break;
            }
            erange_retry = false;
            let base = self.buf.as_ptr() as *const u8;
            let mut entry = base;
            for _ in 0..n {
                let len = unsafe { rd_u32(entry) } as usize;
                parse_entry(entry, f);
                entry = unsafe { entry.add(len) };
            }
        }
        Ok(())
    }
}

#[inline]
unsafe fn rd_u32(p: *const u8) -> u32 {
    unsafe { std::ptr::read_unaligned(p as *const u32) }
}
#[inline]
unsafe fn rd_i32(p: *const u8) -> i32 {
    unsafe { std::ptr::read_unaligned(p as *const i32) }
}
#[inline]
unsafe fn rd_u64(p: *const u8) -> u64 {
    unsafe { std::ptr::read_unaligned(p as *const u64) }
}

fn parse_entry(entry: *const u8, f: &mut dyn FnMut(&EntryInfo<'_>)) {
    unsafe {
        let mut p = entry.add(4);
        // attribute_set_t returned { common, vol, dir, file, fork }
        let r_common = rd_u32(p);
        let r_file = rd_u32(p.add(12));
        let r_fork = rd_u32(p.add(16));
        p = p.add(20);
        if r_common & ATTR_CMN_ERROR != 0 {
            let err = rd_u32(p);
            p = p.add(4);
            if err != 0 {
                return;
            }
        }
        let mut name: &[u8] = b"";
        if r_common & ATTR_CMN_NAME != 0 {
            // attrreference_t { i32 offset (from this field), u32 length incl. NUL }
            let off = rd_i32(p);
            let len = rd_u32(p.add(4)) as usize;
            let np = p.offset(off as isize);
            name = std::slice::from_raw_parts(np, len.saturating_sub(1));
            p = p.add(8);
        }
        let mut dev = 0u64;
        if r_common & ATTR_CMN_DEVID != 0 {
            dev = rd_i32(p) as u32 as u64;
            p = p.add(4);
        }
        let mut objtype = 0u32;
        if r_common & ATTR_CMN_OBJTYPE != 0 {
            objtype = rd_u32(p);
            p = p.add(4);
        }
        let mut mtime = 0i64;
        if r_common & ATTR_CMN_MODTIME != 0 {
            mtime = rd_u64(p) as i64; // struct timespec { tv_sec: i64, tv_nsec: i64 }
            p = p.add(16);
        }
        let mut bsd_flags = 0u32;
        if r_common & ATTR_CMN_FLAGS != 0 {
            bsd_flags = rd_u32(p);
            p = p.add(4);
        }
        let mut ino = 0u64;
        if r_common & ATTR_CMN_FILEID != 0 {
            ino = rd_u64(p);
            p = p.add(8);
        }
        // no dir attrs requested
        let mut nlink = 1u32;
        if r_file & ATTR_FILE_LINKCOUNT != 0 {
            nlink = rd_u32(p);
            p = p.add(4);
        }
        let mut logical = 0u64;
        if r_file & ATTR_FILE_TOTALSIZE != 0 {
            logical = rd_u64(p);
            p = p.add(8);
        }
        let mut alloc = 0u64;
        if r_file & ATTR_FILE_ALLOCSIZE != 0 {
            alloc = rd_u64(p);
            p = p.add(8);
        }
        let mut private = None;
        if r_fork & ATTR_CMNEXT_PRIVATESIZE != 0 {
            private = Some(rd_u64(p));
        }
        let kind = match objtype {
            VREG => Kind::File,
            VDIR => Kind::Dir,
            VLNK => Kind::Symlink,
            _ => Kind::Other,
        };
        let mut fl = 0u32;
        if bsd_flags & UF_COMPRESSED != 0 {
            fl |= flags::COMPRESSED;
        }
        if bsd_flags & SF_DATALESS != 0 {
            fl |= flags::DATALESS;
        }
        if bsd_flags & UF_HIDDEN != 0 {
            fl |= flags::HIDDEN;
        }
        if bsd_flags & (UF_IMMUTABLE | SF_IMMUTABLE) != 0 {
            fl |= flags::IMMUTABLE;
        }
        f(&EntryInfo { name, kind, logical, alloc, private, ino, dev, nlink, mtime, flags: fl });
    }
}

struct FdGuard(libc::c_int);
impl Drop for FdGuard {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

/// Filesystem type name for the volume holding `path` (e.g. "apfs").
pub fn fs_type(path: &CStr) -> Option<String> {
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(path.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let name = unsafe { CStr::from_ptr(s.f_fstypename.as_ptr()) };
    Some(name.to_string_lossy().into_owned())
}

pub fn supports_sharing(path: &CStr) -> bool {
    fs_type(path).is_some_and(|t| t.eq_ignore_ascii_case("apfs"))
}
