//! The drives mounted on this computer and how full each is, for
//! `leaf storage`: the internal disk, USB sticks, external disks and
//! network shares, on macOS and Linux.

use std::ffi::CString;
use std::path::{Path, PathBuf};

/// One mounted drive.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Volume {
    /// What it's called: Macintosh HD, or a USB stick's label.
    pub name: String,
    pub mount: PathBuf,
    pub total: u64,
    pub free: u64,
    /// The computer's own disk, as opposed to one plugged in.
    pub internal: bool,
}

impl Volume {
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.free)
    }

    /// How full it is, 0..1.
    pub fn fullness(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.used() as f64 / self.total as f64
        }
    }

    /// The same drive with its sizes read again, after something was deleted.
    pub fn refreshed(&self) -> Volume {
        match space(&self.mount) {
            Some((total, free)) => Volume {
                total,
                free,
                ..self.clone()
            },
            None => self.clone(),
        }
    }
}

/// The total and free bytes of the file system `path` is on.
pub fn space(path: &Path) -> Option<(u64, u64)> {
    let c = CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: statvfs fills the zeroed struct for a valid NUL-terminated path.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let unit = stat.f_frsize as u64;
    Some((
        (stat.f_blocks as u64).saturating_mul(unit),
        (stat.f_bavail as u64).saturating_mul(unit),
    ))
}

/// Every drive worth showing: the internal disk first, then the rest by name.
pub fn list() -> Vec<Volume> {
    let mut volumes: Vec<Volume> = mounts()
        .into_iter()
        .filter_map(|(mount, internal)| {
            let (total, free) = space(&mount)?;
            (total > 0).then(|| Volume {
                name: name_of(&mount),
                mount,
                total,
                free,
                internal,
            })
        })
        .collect();
    volumes.sort_by(|a, b| {
        b.internal
            .cmp(&a.internal)
            .then(a.mount.as_os_str().len().cmp(&b.mount.as_os_str().len()))
            .then(a.name.cmp(&b.name))
    });
    volumes
}

/// The drive `path` is on: the mounted volume whose mount point is the
/// longest prefix of it.
pub fn containing(volumes: &[Volume], path: &Path) -> Option<usize> {
    volumes
        .iter()
        .enumerate()
        .filter(|(_, v)| path.starts_with(&v.mount))
        .max_by_key(|(_, v)| v.mount.as_os_str().len())
        .map(|(i, _)| i)
}

/// The name a drive shows under: its label in /Volumes on macOS, else the
/// last part of where it's mounted.
fn name_of(mount: &Path) -> String {
    if mount == Path::new("/") {
        #[cfg(target_os = "macos")]
        if let Some(name) = startup_disk_name() {
            return name;
        }
        return "System".to_owned();
    }
    mount
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| mount.display().to_string())
}

/// The startup disk's name: the entry in /Volumes that links to `/`.
#[cfg(target_os = "macos")]
fn startup_disk_name() -> Option<String> {
    std::fs::read_dir("/Volumes")
        .ok()?
        .flatten()
        .find_map(|entry| {
            let target = std::fs::read_link(entry.path()).ok()?;
            (target == Path::new("/")).then(|| entry.file_name().to_string_lossy().into_owned())
        })
}

/// Mount points of real drives, and whether each is internal.
#[cfg(target_os = "macos")]
fn mounts() -> Vec<(PathBuf, bool)> {
    use std::ffi::CStr;
    let mut list: *mut libc::statfs = std::ptr::null_mut();
    // SAFETY: getmntinfo points `list` at `count` statfs records it owns.
    let count = unsafe { libc::getmntinfo(&mut list, libc::MNT_NOWAIT) };
    if count <= 0 || list.is_null() {
        return vec![(PathBuf::from("/"), true)];
    }
    let records = unsafe { std::slice::from_raw_parts(list, count as usize) };
    records
        .iter()
        .filter_map(|record| {
            let on = unsafe { CStr::from_ptr(record.f_mntonname.as_ptr()) };
            let kind = unsafe { CStr::from_ptr(record.f_fstypename.as_ptr()) };
            let on = PathBuf::from(on.to_string_lossy().into_owned());
            let kind = kind.to_string_lossy();
            if matches!(kind.as_ref(), "devfs" | "autofs" | "nullfs" | "lifs") {
                return None;
            }
            // The system's own volumes share the startup disk's space.
            if on.starts_with("/System/Volumes") || on.starts_with("/private/var/vm") {
                return None;
            }
            if on == Path::new("/") {
                return Some((on, true));
            }
            let browsable = record.f_flags & libc::MNT_DONTBROWSE as u32 == 0;
            (on.starts_with("/Volumes") && browsable).then_some((on, false))
        })
        .collect()
}

/// Mount points of real drives, and whether each is internal.
#[cfg(not(target_os = "macos"))]
fn mounts() -> Vec<(PathBuf, bool)> {
    let table = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (device, on, kind) in table.lines().filter_map(parse_mount_line) {
        if !is_real_fs(&device, &kind) || !seen.insert(device.clone()) {
            continue;
        }
        let internal =
            !(on.starts_with("/media") || on.starts_with("/run/media") || on.starts_with("/mnt"));
        out.push((on, internal));
    }
    if out.is_empty() {
        out.push((PathBuf::from("/"), true));
    }
    out
}

/// A /proc/mounts line: the device, where it's mounted and its type, with
/// the kernel's octal escapes for spaces undone.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn parse_mount_line(line: &str) -> Option<(String, PathBuf, String)> {
    let mut parts = line.split_whitespace();
    let device = parts.next()?.to_owned();
    let on = parts.next()?;
    let kind = parts.next()?.to_owned();
    let on = on
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\134", "\\");
    Some((device, PathBuf::from(on), kind))
}

/// Whether a mount is a drive with files on it, not a kernel file system.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn is_real_fs(device: &str, kind: &str) -> bool {
    let network = matches!(
        kind,
        "nfs" | "nfs4" | "cifs" | "smb3" | "sshfs" | "fuse.sshfs"
    );
    let virtual_fs = matches!(
        kind,
        "proc"
            | "sysfs"
            | "tmpfs"
            | "devtmpfs"
            | "devpts"
            | "cgroup"
            | "cgroup2"
            | "overlay"
            | "squashfs"
            | "securityfs"
            | "debugfs"
            | "tracefs"
            | "pstore"
            | "bpf"
            | "mqueue"
            | "hugetlbfs"
            | "configfs"
            | "fusectl"
            | "autofs"
            | "ramfs"
            | "efivarfs"
            | "binfmt_misc"
            | "nsfs"
            | "fuse.portal"
            | "fuse.gvfsd-fuse"
    );
    !virtual_fs && (device.starts_with("/dev/") || network)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_lines_parse_with_spaces() {
        assert_eq!(
            parse_mount_line("/dev/sdb1 /media/a/My\\040Drive vfat rw 0 0"),
            Some((
                "/dev/sdb1".to_owned(),
                PathBuf::from("/media/a/My Drive"),
                "vfat".to_owned()
            ))
        );
        assert!(is_real_fs("/dev/sda2", "ext4"));
        assert!(!is_real_fs("tmpfs", "tmpfs"));
        assert!(!is_real_fs("/dev/loop0", "squashfs"));
    }

    #[test]
    fn the_longest_mount_owns_a_path() {
        let v = |mount: &str| Volume {
            name: mount.to_owned(),
            mount: PathBuf::from(mount),
            total: 10,
            free: 4,
            internal: true,
        };
        let volumes = [v("/"), v("/home"), v("/media/usb")];
        assert_eq!(containing(&volumes, Path::new("/home/a/x")), Some(1));
        assert_eq!(containing(&volumes, Path::new("/opt")), Some(0));
        assert_eq!(containing(&volumes, Path::new("/media/usb/f")), Some(2));
        assert_eq!(volumes[0].used(), 6);
        assert!((volumes[0].fullness() - 0.6).abs() < 1e-9);
    }

    #[test]
    fn the_root_has_space() {
        let (total, free) = space(Path::new("/")).unwrap();
        assert!(total > 0 && free <= total);
        assert!(!list().is_empty());
    }
}
