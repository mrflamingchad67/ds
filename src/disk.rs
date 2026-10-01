//! Disk space collection.

use std::fmt;
use std::path::{Path, PathBuf};

/// Space figures for a single filesystem, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct DiskUsage {
    /// Bytes available to an unprivileged user.
    pub available: u64,
    /// Total bytes on the filesystem.
    pub total: u64,
}

impl DiskUsage {
    /// Bytes currently consumed.
    ///
    /// Saturates instead of underflowing when the platform reports
    /// inconsistent figures.
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.available)
    }

    /// Percentage of the filesystem in use, from 0.0 to 100.0.
    ///
    /// A zero-sized filesystem reports 0.0 rather than dividing by zero.
    pub fn usage_percent(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        (self.used() as f64 / self.total as f64) * 100.0
    }
}

/// A drive letter (Windows) or mount point (Unix) and its usage.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Drive {
    /// Display name including the separator, e.g. `C:\` or `/`.
    pub name: String,
    /// Backend query path used to fetch usage figures.
    #[serde(skip)]
    pub path: PathBuf,
    /// Space figures.
    pub usage: DiskUsage,
}

impl Drive {
    /// Label suitable for display.
    pub fn label(&self) -> &str {
        &self.name
    }
}

/// Why a drive could not be inspected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveError {
    /// The drive or path that failed.
    pub target: String,
    /// Human-readable cause.
    pub message: String,
}

impl fmt::Display for DriveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.target, self.message)
    }
}

impl std::error::Error for DriveError {}

/// Fetch usage for a single path, normalising the root as needed.
///
/// Space belongs to a volume, so `C:\Users` reports the figures for `C:\`.
pub fn query(path: &Path) -> Result<DiskUsage, String> {
    let root = volume_root(path);
    platform::usage(&root)
}

/// Normalise a user-supplied drive argument into a volume root.
///
/// `C`, `c:`, `C:\`, and `C:\Users` all resolve to `C:\`, because free and total
/// space are properties of the volume rather than the directory.
pub fn volume_root(path: &Path) -> PathBuf {
    let text = trim_root(path);

    // A drive letter is a Windows concept; on Unix `/data` is already a mount
    // point and must not be folded into `/`.
    #[cfg(windows)]
    if let Some(letter) = drive_letter_of(&text) {
        return PathBuf::from(format!("{letter}:\\"));
    }

    PathBuf::from(text)
}

/// Normalise a scan target without collapsing it to its volume.
///
/// `C:` becomes `C:\`, but `C:\Users` stays `C:\Users`, because a filesystem scan
/// must walk exactly what the user asked for.
pub fn scan_target(path: &Path) -> PathBuf {
    let text = trim_root(path);

    // `C` and `C:` (and `C:\`, already trimmed to `C:`) mean the volume root.
    // Anything with a subdirectory is kept exactly as given, so
    // `C:\Users` does not silently become a whole-volume scan.
    #[cfg(windows)]
    if let Some(letter) = bare_drive_letter_of(&text) {
        return PathBuf::from(format!("{letter}:\\"));
    }

    PathBuf::from(text)
}

/// Trim surrounding space and trailing separators, defaulting to the root.
fn trim_root(path: &Path) -> String {
    let raw = path.as_os_str().to_string_lossy().trim().to_string();

    // Only the platform's own separator is trimmed: a backslash is a legal
    // character in a Unix file name, so stripping it there would corrupt paths.
    let trimmed = if cfg!(windows) {
        raw.trim_end_matches(['\\', '/'])
    } else {
        raw.trim_end_matches('/')
    };

    if trimmed.is_empty() {
        return if cfg!(windows) && raw.contains('\\') {
            "\\".to_string()
        } else {
            "/".to_string()
        };
    }

    trimmed.to_string()
}

/// The uppercase letter of a `C` or `C:` argument, with nothing after it.
#[cfg(windows)]
fn bare_drive_letter_of(text: &str) -> Option<char> {
    let bytes = text.as_bytes();

    let is_bare = bytes.len() == 1 || (bytes.len() == 2 && bytes[1] == b':');
    if !is_bare || !is_ascii_letter(text) {
        return None;
    }

    Some(bytes[0].to_ascii_uppercase() as char)
}

/// The uppercase letter of a `C`, `C:`, or `C:\...` argument, if any.
///
/// Used for volume capacity, where any path on the drive reports the drive.
#[cfg(windows)]
fn drive_letter_of(text: &str) -> Option<char> {
    if let Some(letter) = bare_drive_letter_of(text) {
        return Some(letter);
    }

    let bytes = text.as_bytes();
    let has_root = bytes.len() >= 3 && bytes[1] == b':' && matches!(bytes[2], b'\\' | b'/');

    (has_root && is_ascii_letter(text)).then(|| bytes[0].to_ascii_uppercase() as char)
}

#[cfg(windows)]
fn is_ascii_letter(text: &str) -> bool {
    text.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetLogicalDrives};
    use windows_sys::Win32::System::WindowsProgramming::{DRIVE_CDROM, DRIVE_RAMDISK};

    pub fn usage(root: &Path) -> Result<DiskUsage, String> {
        let mut wide: Vec<u16> = root.as_os_str().encode_wide().collect();
        wide.push(0);

        let mut free_to_caller: u64 = 0;
        let mut total: u64 = 0;
        let mut total_free: u64 = 0;

        // SAFETY: `wide` is a NUL-terminated buffer that outlives the call,
        // and the out-params are valid, initialised u64 slots.
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut free_to_caller as *mut u64,
                &mut total as *mut u64,
                &mut total_free as *mut u64,
            )
        };

        if ok == 0 {
            return Err(last_os_error());
        }

        Ok(DiskUsage {
            available: free_to_caller,
            total,
        })
    }

    fn last_os_error() -> String {
        std::io::Error::last_os_error().to_string()
    }

    /// Enumerate volumes that have a drive letter assigned.
    pub fn list_drives() -> Vec<Drive> {
        let mask = unsafe { GetLogicalDrives() };

        (0u8..26)
            .filter(|index| mask & (1 << index) != 0)
            .filter_map(|index| {
                let letter = (b'A' + index) as char;
                let root = PathBuf::from(format!("{letter}:\\"));

                // Optical and RAM drives report no meaningful capacity, so
                // they are skipped rather than shown as empty volumes.
                if matches!(volume_type(&root), DRIVE_CDROM | DRIVE_RAMDISK) {
                    return None;
                }

                let usage = usage(&root).ok()?;

                Some(Drive {
                    name: format!("{letter}:\\"),
                    path: root,
                    usage,
                })
            })
            .collect()
    }

    /// Read the volume-type flags for a drive root.
    ///
    /// A failure is treated as "not a special volume" so a single odd device
    /// cannot hide a real drive.
    fn volume_type(root: &Path) -> u32 {
        use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW;

        let mut wide: Vec<u16> = root.as_os_str().encode_wide().collect();
        wide.push(0);
        let mut fs_name = [0u16; 261];
        let mut serial: u32 = 0;
        let mut max_component: u32 = 0;
        let mut flags: u32 = 0;

        // SAFETY: all buffers are valid and large enough for the API's
        // documented limits, and `wide` is NUL-terminated.
        let ok = unsafe {
            GetVolumeInformationW(
                wide.as_ptr(),
                fs_name.as_mut_ptr(),
                fs_name.len() as u32,
                &mut serial as *mut u32,
                &mut max_component as *mut u32,
                &mut flags as *mut u32,
                std::ptr::null_mut(),
                0,
            )
        };

        if ok == 0 {
            return 0;
        }

        flags
    }
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    /// Total and available bytes for a mount point.
    ///
    /// The two Unix dialects disagree on both the call and the field holding
    /// the fragment size, so each variant is compiled separately. Reading the
    /// wrong struct's fields would silently produce nonsense numbers rather
    /// than failing, which is why this is not shared behind a cfg inside one
    /// function body.
    #[cfg(not(target_os = "macos"))]
    pub fn usage(root: &Path) -> Result<DiskUsage, String> {
        let c_path = std::ffi::CString::new(root.as_os_str().as_bytes())
            .map_err(|_| "path contains an interior nul byte".to_string())?;

        // SAFETY: `c_path` is a valid NUL-terminated string that outlives the
        // call, and `statvfs` is fully initialised on success.
        let stat = unsafe {
            let mut stat: libc::statvfs = std::mem::zeroed();
            if libc::statvfs(c_path.as_ptr(), &mut stat) != 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            stat
        };

        // `f_frsize` is the fragment size the block counts are expressed in.
        // `f_bavail` is what an unprivileged user may actually use, which is
        // the figure DS reports so both platforms agree on what "free" means.
        let fragment = stat.f_frsize as u64;
        Ok(DiskUsage {
            total: (stat.f_blocks as u64).saturating_mul(fragment),
            available: (stat.f_bavail as u64).saturating_mul(fragment),
        })
    }

    /// Total and available bytes for a mount point on macOS.
    ///
    /// macOS has `statfs` rather than `statvfs`, and carries the fragment size
    /// in `f_bsize`.
    ///
    /// Note that APFS volumes sharing a container all report that container's
    /// totals, so two volumes in one container legitimately show identical
    /// figures. That is the filesystem's behaviour, not a rounding artefact.
    #[cfg(target_os = "macos")]
    pub fn usage(root: &Path) -> Result<DiskUsage, String> {
        let c_path = std::ffi::CString::new(root.as_os_str().as_bytes())
            .map_err(|_| "path contains an interior nul byte".to_string())?;

        // SAFETY: as for the statvfs variant above.
        let stat = unsafe {
            let mut stat: libc::statfs = std::mem::zeroed();
            if libc::statfs(c_path.as_ptr(), &mut stat) != 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            stat
        };

        let fragment = stat.f_bsize as u64;
        Ok(DiskUsage {
            total: stat.f_blocks.saturating_mul(fragment),
            available: stat.f_bavail.saturating_mul(fragment),
        })
    }

    /// Every real mount point, labelled by its path.
    pub fn list_drives() -> Vec<Drive> {
        candidates()
            .into_iter()
            .filter_map(|root| {
                let usage = usage(&root).ok()?;
                Some(Drive {
                    name: root.to_string_lossy().into_owned(),
                    path: root,
                    usage,
                })
            })
            .collect()
    }

    /// Mount points from the platform's own facility, otherwise the
    /// conventional locations.
    pub(crate) fn candidates() -> Vec<PathBuf> {
        let mut roots = mount_points().unwrap_or_default();

        if roots.is_empty() {
            // Nothing enumerable: guess the usual roots and let `usage` discard
            // whatever turns out not to be a real mount point.
            roots = ["/", "/home", "/mnt", "/media", "/run/media"]
                .iter()
                .map(PathBuf::from)
                .collect();
        }

        roots.sort();
        roots.dedup();
        roots
    }

    /// Distinct real mount points, from `/proc/mounts`.
    ///
    /// Pseudo-filesystems are skipped: reporting `proc` or `sysfs` as a drive is
    /// noise rather than information.
    #[cfg(not(target_os = "macos"))]
    pub(crate) fn mount_points() -> Option<Vec<PathBuf>> {
        let mounts = std::fs::read_to_string("/proc/mounts").ok()?;
        let mut roots = Vec::new();

        for line in mounts.lines() {
            let mut fields = line.split_whitespace();
            let (Some(device), Some(mount)) = (fields.next(), fields.next()) else {
                continue;
            };

            if !device.starts_with('/') || is_pseudo_filesystem(mount) {
                continue;
            }

            let root = PathBuf::from(unescape_mount_point(mount));
            if !roots.contains(&root) {
                roots.push(root);
            }
        }

        Some(roots)
    }

    /// Distinct real mount points, from `getmntinfo`.
    ///
    /// macOS has no procfs, so reading `/proc/mounts` there would find nothing
    /// and DS would report only `/`, hiding every external disk. The kernel's
    /// mount table is queried directly instead.
    #[cfg(target_os = "macos")]
    pub(crate) fn mount_points() -> Option<Vec<PathBuf>> {
        // SAFETY: `getmntinfo` stores a pointer to a system-allocated table of
        // exactly `count` entries. The pointer and length are used only here,
        // before the table is released when the process exits.
        unsafe {
            let mut buffer: *mut libc::statfs = std::ptr::null_mut();
            let count = libc::getmntinfo(&mut buffer, libc::MNT_NOWAIT);

            if count <= 0 || buffer.is_null() {
                return None;
            }

            let entries = std::slice::from_raw_parts(buffer, count as usize);
            let mut roots = Vec::new();

            for entry in entries {
                // Filtered by filesystem type, not path prefix: macOS has no
                // /proc and its pseudo-filesystems live under /dev and /System.
                if is_pseudo_filesystem(&fixed_string(&entry.f_fstypename)) {
                    continue;
                }

                let name = fixed_string(&entry.f_mntonname);
                if name.is_empty() {
                    continue;
                }

                let root = PathBuf::from(name);
                if !roots.contains(&root) {
                    roots.push(root);
                }
            }

            Some(roots)
        }
    }

    /// Decode a fixed-size C string field into an owned `String`.
    ///
    /// # Safety
    ///
    /// `field` must be a NUL-terminated byte array, as the `statfs` name and
    /// filesystem-type fields are.
    #[cfg(target_os = "macos")]
    unsafe fn fixed_string(field: &[libc::c_char]) -> String {
        let bytes: Vec<u8> = field
            .iter()
            .take_while(|&&ch| ch != 0)
            .map(|&ch| ch as u8)
            .collect();

        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Mount types that are not real storage.
    #[cfg(not(target_os = "macos"))]
    pub(crate) fn is_pseudo_filesystem(mount: &str) -> bool {
        const PSEUDO: [&str; 11] = [
            "proc",
            "sysfs",
            "devtmpfs",
            "devpts",
            "tmpfs",
            "cgroup",
            "cgroup2",
            "securityfs",
            "pstore",
            "debugfs",
            "tracefs",
        ];
        PSEUDO.contains(&mount)
    }

    /// Filesystem types that are not real storage on macOS.
    ///
    /// Filtered by type rather than by path, because macOS mount points carry
    /// no indication of pseudo-ness in their name.
    #[cfg(target_os = "macos")]
    pub(crate) fn is_pseudo_filesystem(filesystem: &str) -> bool {
        const PSEUDO: [&str; 4] = ["devfs", "autofs", "procfs", "nullfs"];
        PSEUDO.contains(&filesystem)
    }

    /// Decode the octal escapes the kernel writes into `/proc/mounts`, such as
    /// `/mnt/my\x20disk` for a directory whose name contains a space.
    #[cfg(not(target_os = "macos"))]
    pub(crate) fn unescape_mount_point(mount: &str) -> String {
        let bytes = mount.as_bytes();
        let mut out = String::with_capacity(mount.len());
        let mut index = 0;

        while index < bytes.len() {
            let escaped = bytes[index] == b'\\'
                && index + 3 < bytes.len()
                && std::str::from_utf8(&bytes[index + 1..index + 4])
                    .is_ok_and(|digits| u8::from_str_radix(digits, 8).is_ok());

            if escaped {
                let digits =
                    std::str::from_utf8(&bytes[index + 1..index + 4]).expect("checked above");
                let value = u8::from_str_radix(digits, 8).expect("checked above");
                out.push(value as char);
                index += 4;
                continue;
            }

            out.push(bytes[index] as char);
            index += 1;
        }

        out
    }
}

/// List all drives on the current platform.
pub fn list() -> Vec<Drive> {
    platform::list_drives()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unix-only helpers, tested wherever they can run.
    #[cfg(unix)]
    mod unix {
        use super::platform::*;
        use std::path::Path;

        #[test]
        fn reads_the_system_root() {
            let usage = usage(Path::new("/")).expect("querying / should succeed");
            assert!(usage.total > 0, "root must have capacity");
            assert!(usage.available <= usage.total);
        }

        #[test]
        fn reports_each_mount_point_once() {
            let drives = list_drives();
            assert!(!drives.is_empty(), "expected at least one mount point");

            let names: Vec<&str> = drives.iter().map(|d| d.name.as_str()).collect();
            let mut unique = names.clone();
            unique.sort_unstable();
            unique.dedup();
            assert_eq!(names.len(), unique.len(), "mount points must be unique");

            for drive in &drives {
                assert!(drive.usage.total > 0, "{} reported no capacity", drive.name);
            }
        }

        #[test]
        fn includes_the_root_mount_point() {
            let drives = list_drives();
            assert!(
                drives.iter().any(|drive| drive.path == Path::new("/")),
                "the root filesystem must be reported: {:?}",
                drives.iter().map(|d| &d.name).collect::<Vec<_>>()
            );
        }

        #[test]
        fn a_missing_mount_point_reports_an_error() {
            assert!(usage(Path::new("/definitely-not-a-mount")).is_err());
        }
    }

    /// `/proc/mounts` parsing, which only exists on Linux and other procfs
    /// systems.
    #[cfg(all(unix, not(target_os = "macos")))]
    mod procfs {
        use super::platform::*;

        #[test]
        fn unescapes_octal_sequences_in_mount_points() {
            // The kernel escapes spaces and other specials in /proc/mounts.
            assert_eq!(unescape_mount_point("/mnt/my\\040disk"), "/mnt/my disk");
            assert_eq!(unescape_mount_point("/data\\011tab"), "/data\ttab");
        }

        #[test]
        fn leaves_plain_mount_points_untouched() {
            assert_eq!(unescape_mount_point("/"), "/");
            assert_eq!(unescape_mount_point("/home/user"), "/home/user");
            assert_eq!(unescape_mount_point("/mnt/disk"), "/mnt/disk");
        }

        #[test]
        fn leaves_stray_backslashes_alone() {
            assert_eq!(unescape_mount_point("/a\\b"), "/a\\b");
            assert_eq!(unescape_mount_point("/trailing\\"), "/trailing\\");
            assert_eq!(unescape_mount_point("/\\"), "/\\");
        }

        #[test]
        fn skips_pseudo_filesystems() {
            for pseudo in ["proc", "sysfs", "tmpfs", "cgroup2", "devpts", "debugfs"] {
                assert!(
                    is_pseudo_filesystem(pseudo),
                    "{pseudo} should be filtered out"
                );
            }
        }

        #[test]
        fn keeps_real_filesystems() {
            // Notably "/" itself is real storage, never a pseudo-filesystem.
            for real in ["", "ext4", "btrfs", "xfs", "vfat", "ntfs3", "nfs4", "zfs"] {
                assert!(!is_pseudo_filesystem(real), "{real:?} should be kept");
            }
        }

        #[test]
        fn never_reports_proc_as_a_drive() {
            let names: Vec<String> = list_drives().into_iter().map(|drive| drive.name).collect();
            assert!(
                !names.iter().any(|name| name.contains("proc")),
                "proc must not be reported as a drive: {names:?}"
            );
        }
    }

    /// macOS mount discovery and the `statfs` variant.
    #[cfg(target_os = "macos")]
    mod macos {
        use super::platform::*;
        use std::path::Path;

        #[test]
        fn skips_pseudo_filesystems_by_type() {
            for pseudo in ["devfs", "autofs", "procfs", "nullfs"] {
                assert!(
                    is_pseudo_filesystem(pseudo),
                    "{pseudo} should be filtered out"
                );
            }
        }

        #[test]
        fn keeps_real_filesystems() {
            for real in ["apfs", "hfs", "msdos", "exfat", "ntfs", "smbfs", ""] {
                assert!(!is_pseudo_filesystem(real), "{real:?} should be kept");
            }
        }

        #[test]
        fn reads_the_system_root() {
            let usage = usage(Path::new("/")).expect("statfs on / should succeed");
            assert!(usage.total > 0);
        }

        /// The regression this backend exists to prevent: macOS has no
        /// procfs, so a `/proc/mounts`-only implementation reports just `/`
        /// and hides every external disk.
        #[test]
        fn finds_more_than_the_root_mount_point() {
            let drives = list_drives();
            assert!(
                drives.len() > 1,
                "expected several mount points on macOS, got {:?}",
                drives.iter().map(|d| &d.name).collect::<Vec<_>>()
            );
        }

        #[test]
        fn reads_mount_points_without_procfs() {
            let roots = mount_points().expect("getmntinfo should enumerate mounts");
            assert!(!roots.is_empty());
            assert!(roots.iter().any(|root| root == Path::new("/")));
        }

        #[test]
        fn never_reports_devfs_as_a_drive() {
            let names: Vec<String> = list_drives().into_iter().map(|d| d.name).collect();
            assert!(
                !names.iter().any(|name| name.starts_with("/dev")),
                "devfs must not be reported as a drive: {names:?}"
            );
        }

        #[test]
        fn a_missing_mount_point_reports_an_error() {
            assert!(usage(Path::new("/definitely-not-a-mount")).is_err());
        }
    }

    fn usage(total: u64, available: u64) -> DiskUsage {
        DiskUsage { available, total }
    }

    #[test]
    fn used_is_total_minus_available() {
        assert_eq!(usage(1000, 400).used(), 600);
    }

    #[test]
    fn used_saturates_instead_of_underflowing() {
        assert_eq!(usage(100, 150).used(), 0);
    }

    #[test]
    fn usage_percent_of_half_used_filesystem() {
        assert!((usage(1000, 500).usage_percent() - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn usage_percent_of_full_filesystem_is_one_hundred() {
        assert!((usage(2000, 0).usage_percent() - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn usage_percent_of_empty_filesystem_is_zero() {
        assert_eq!(usage(0, 0).usage_percent(), 0.0);
    }

    /// Windows-specific: drive-letter expansion has no meaning elsewhere.
    #[cfg(windows)]
    #[test]
    fn volume_root_expands_bare_drive_letter() {
        assert_eq!(volume_root(Path::new("C:")), PathBuf::from("C:\\"));
        assert_eq!(volume_root(Path::new("c:")), PathBuf::from("C:\\"));
        assert_eq!(volume_root(Path::new("d")), PathBuf::from("D:\\"));
    }

    /// Windows-specific: drive-letter expansion has no meaning elsewhere.
    #[cfg(windows)]
    #[test]
    fn volume_root_reduces_drive_path_to_its_root() {
        assert_eq!(volume_root(Path::new("C:\\Users")), PathBuf::from("C:\\"));
        assert_eq!(volume_root(Path::new("C:/Users/me")), PathBuf::from("C:\\"));
    }

    /// Windows-specific: drive-letter expansion has no meaning elsewhere.
    #[cfg(windows)]
    #[test]
    fn volume_root_strips_trailing_separator() {
        assert_eq!(volume_root(Path::new("D:\\")), PathBuf::from("D:\\"));
    }

    #[test]
    fn volume_root_handles_unix_paths() {
        assert_eq!(volume_root(Path::new("/")), PathBuf::from("/"));
        assert_eq!(
            volume_root(Path::new("/home/me")),
            PathBuf::from("/home/me")
        );
        assert_eq!(
            volume_root(Path::new("/home/me/")),
            PathBuf::from("/home/me")
        );
    }

    #[test]
    fn volume_root_maps_empty_input_to_root() {
        assert_eq!(volume_root(Path::new("")), PathBuf::from("/"));
    }

    /// Windows-specific: drive-letter expansion has no meaning elsewhere.
    #[cfg(windows)]
    #[test]
    fn scan_target_expands_a_bare_drive_letter() {
        assert_eq!(scan_target(Path::new("C:")), PathBuf::from("C:\\"));
        assert_eq!(scan_target(Path::new("d")), PathBuf::from("D:\\"));
        assert_eq!(scan_target(Path::new("D:\\")), PathBuf::from("D:\\"));
    }

    /// Windows-specific: drive-letter expansion has no meaning elsewhere.
    #[cfg(windows)]
    #[test]
    fn scan_target_keeps_subdirectories() {
        // The whole point of this function: a scan must walk exactly the
        // directory the user named, not its volume.
        assert_eq!(
            scan_target(Path::new("C:\\Users")),
            PathBuf::from("C:\\Users")
        );
        assert_eq!(
            scan_target(Path::new("C:\\Users\\me\\")),
            PathBuf::from("C:\\Users\\me")
        );
        assert_eq!(
            scan_target(Path::new("C:/Users/me")),
            PathBuf::from("C:\\Users\\me")
        );
    }

    #[test]
    fn scan_target_keeps_unix_paths() {
        assert_eq!(scan_target(Path::new("/")), PathBuf::from("/"));
        assert_eq!(
            scan_target(Path::new("/home/me")),
            PathBuf::from("/home/me")
        );
        assert_eq!(
            scan_target(Path::new("/home/me/")),
            PathBuf::from("/home/me")
        );
        assert_eq!(scan_target(Path::new("")), PathBuf::from("/"));
    }

    /// Windows-specific: drive-letter expansion has no meaning elsewhere.
    #[cfg(windows)]
    #[test]
    fn scan_target_and_volume_root_differ_for_subdirectories() {
        let sub = Path::new("C:\\Users\\me");
        assert_ne!(scan_target(sub), volume_root(sub));
        assert_eq!(volume_root(sub), PathBuf::from("C:\\"));
        assert_eq!(scan_target(sub), PathBuf::from("C:\\Users\\me"));
    }

    #[test]
    fn query_reports_usable_figures_for_the_system_root() {
        let usage = query(Path::new(if cfg!(windows) { "C:\\" } else { "/" }))
            .expect("system root should be queryable");
        assert!(usage.total > 0, "total should be non-zero");
        assert!(usage.available <= usage.total);
    }

    #[test]
    fn query_errors_on_a_nonexistent_volume() {
        let bogus = if cfg!(windows) {
            "Z:\\"
        } else {
            "/definitely-not-a-mount"
        };
        assert!(query(Path::new(bogus)).is_err());
    }

    #[test]
    fn list_returns_at_least_one_drive() {
        let drives = list();
        assert!(!drives.is_empty(), "expected at least one drive");
        for drive in &drives {
            assert!(!drive.name.is_empty());
        }
    }

    #[test]
    fn drive_error_message_includes_the_target() {
        let err = DriveError {
            target: "Z:\\".to_string(),
            message: "device not ready".to_string(),
        };
        assert_eq!(err.to_string(), "Z:\\: device not ready");
    }
}
