//! Reads a folder's entries with their sizes as fast as the system allows,
//! for `leaf storage`. On macOS one `getattrlistbulk` call returns the name,
//! type, device, change time, inode, link count and size on disk of a whole
//! batch of entries, with no `lstat` per file. Elsewhere `read_dir` gives
//! the type for free and `fstatat` on the open folder gives the rest.

use std::ffi::OsString;
use std::io;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    /// Symbolic links, sockets, devices: never followed or counted.
    Other,
}

/// One entry of a folder.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: OsString,
    pub kind: Kind,
    /// Bytes allocated on disk.
    pub bytes: u64,
    /// Seconds since 1970.
    pub mtime: i64,
    pub dev: u64,
    pub ino: u64,
    pub nlink: u32,
}

/// Every entry of `dir`, without `.` and `..`.
pub fn read(dir: &Path) -> io::Result<Vec<Entry>> {
    imp::read(dir)
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{Entry, Kind};
    use std::cell::RefCell;
    use std::ffi::{CString, OsStr, OsString};
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    /// Room for a few hundred entries per call; reused per thread.
    const BUFFER: usize = 256 * 1024;
    const VREG: u32 = 1;
    const VDIR: u32 = 2;

    thread_local! {
        static SCRATCH: RefCell<Vec<u8>> = RefCell::new(vec![0; BUFFER]);
    }

    /// Closes the folder however reading it ends.
    struct Fd(libc::c_int);

    impl Drop for Fd {
        fn drop(&mut self) {
            // SAFETY: the descriptor was opened by `read` and is closed once.
            unsafe { libc::close(self.0) };
        }
    }

    pub fn read(dir: &Path) -> io::Result<Vec<Entry>> {
        let c = CString::new(dir.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path has a NUL byte"))?;
        // SAFETY: a valid NUL-terminated path; the result is checked.
        let fd = unsafe {
            libc::open(
                c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = Fd(fd);
        let mut list = libc::attrlist {
            bitmapcount: libc::ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: libc::ATTR_CMN_RETURNED_ATTRS
                | libc::ATTR_CMN_NAME
                | libc::ATTR_CMN_DEVID
                | libc::ATTR_CMN_OBJTYPE
                | libc::ATTR_CMN_MODTIME
                | libc::ATTR_CMN_FILEID,
            volattr: 0,
            dirattr: 0,
            fileattr: libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_ALLOCSIZE,
            forkattr: 0,
        };
        SCRATCH.with(|scratch| {
            let mut buf = scratch.borrow_mut();
            let mut out = Vec::new();
            loop {
                // SAFETY: `buf` is BUFFER bytes long and lives for the call.
                let count = unsafe {
                    libc::getattrlistbulk(
                        fd.0,
                        (&mut list as *mut libc::attrlist).cast(),
                        buf.as_mut_ptr().cast(),
                        buf.len(),
                        u64::from(libc::FSOPT_PACK_INVAL_ATTRS),
                    )
                };
                if count < 0 {
                    return Err(io::Error::last_os_error());
                }
                if count == 0 {
                    return Ok(out);
                }
                let mut at = 0usize;
                for _ in 0..count {
                    let Some(entry) = parse(&buf, at) else {
                        return Ok(out);
                    };
                    at += entry.1;
                    out.extend(entry.0);
                }
            }
        })
    }

    fn get<T: Copy>(buf: &[u8], at: usize) -> Option<T> {
        let end = at.checked_add(std::mem::size_of::<T>())?;
        let bytes = buf.get(at..end)?;
        // SAFETY: `bytes` holds exactly size_of::<T>() bytes of a plain-old-data T.
        Some(unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<T>()) })
    }

    /// The entry at `at` and its length. The attributes come packed in the
    /// order of their bits, invalid ones as placeholders, so offsets are fixed.
    fn parse(buf: &[u8], at: usize) -> Option<(Option<Entry>, usize)> {
        let length = get::<u32>(buf, at)? as usize;
        if length == 0 {
            return None;
        }
        let mut p = at + 4;
        let returned: libc::attribute_set_t = get(buf, p)?;
        p += std::mem::size_of::<libc::attribute_set_t>();
        let name_ref: libc::attrreference_t = get(buf, p)?;
        let name_at = p.checked_add_signed(name_ref.attr_dataoffset as isize)?;
        p += std::mem::size_of::<libc::attrreference_t>();
        let dev: libc::dev_t = get(buf, p)?;
        p += std::mem::size_of::<libc::dev_t>();
        let objtype: u32 = get(buf, p)?;
        p += 4;
        let mtime: libc::timespec = get(buf, p)?;
        p += std::mem::size_of::<libc::timespec>();
        let ino: u64 = get(buf, p)?;
        p += 8;
        let nlink: u32 = get(buf, p)?;
        p += 4;
        let alloc: i64 = get(buf, p)?;

        let name_len = (name_ref.attr_length as usize).saturating_sub(1);
        let name = buf.get(name_at..name_at + name_len)?;
        let entry = (name != b"." && name != b"..").then(|| {
            let kind = match objtype {
                VREG => Kind::File,
                VDIR => Kind::Dir,
                _ => Kind::Other,
            };
            let has_size = returned.fileattr & libc::ATTR_FILE_ALLOCSIZE != 0;
            Entry {
                name: OsString::from(OsStr::from_bytes(name)),
                kind,
                bytes: if kind == Kind::File && has_size {
                    alloc.max(0) as u64
                } else {
                    0
                },
                mtime: mtime.tv_sec,
                dev: dev as u64,
                ino,
                nlink: if returned.fileattr & libc::ATTR_FILE_LINKCOUNT != 0 {
                    nlink
                } else {
                    1
                },
            }
        });
        Some((entry, length))
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::{Entry, Kind};
    use std::io;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    pub fn read(dir: &Path) -> io::Result<Vec<Entry>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let Ok(entry) = entry else { continue };
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let kind = if kind.is_file() {
                Kind::File
            } else if kind.is_dir() {
                Kind::Dir
            } else {
                Kind::Other
            };
            if kind == Kind::Other {
                continue;
            }
            // fstatat on the open folder: no path lookup from the root.
            let Ok(meta) = entry.metadata() else { continue };
            out.push(Entry {
                name: entry.file_name(),
                kind,
                bytes: if kind == Kind::File {
                    meta.blocks() * 512
                } else {
                    0
                },
                mtime: meta.mtime(),
                dev: meta.dev(),
                ino: meta.ino(),
                nlink: meta.nlink() as u32,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn entries_match_what_lstat_says() {
        let dir = std::env::temp_dir().join(format!("leaf-dirscan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a file"), vec![1u8; 70_000]).unwrap();
        std::os::unix::fs::symlink(dir.join("sub"), dir.join("link")).unwrap();
        let mut entries = read(&dir).unwrap();
        entries.retain(|e| e.kind != Kind::Other);
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(entries.len(), 2, "{entries:?}");
        let file = &entries[0];
        let meta = std::fs::symlink_metadata(dir.join("a file")).unwrap();
        assert_eq!(file.name, "a file");
        assert_eq!(file.kind, Kind::File);
        assert_eq!(file.bytes, meta.blocks() * 512);
        assert_eq!(file.dev, meta.dev());
        assert_eq!(file.ino, meta.ino());
        assert_eq!(file.mtime, meta.mtime());
        assert_eq!(file.nlink, 1);
        assert_eq!(entries[1].kind, Kind::Dir);
        assert!(read(&dir.join("missing")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_big_folder_reads_across_batches() {
        let dir = std::env::temp_dir().join(format!("leaf-dirscan-big-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..3000 {
            std::fs::write(dir.join(format!("file-with-a-long-name-{i:05}")), b"x").unwrap();
        }
        assert_eq!(read(&dir).unwrap().len(), 3000);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
