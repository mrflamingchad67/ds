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
    let raw = path.as_os_str().to_string_lossy().into_owned();
    let trimmed = raw.trim();
    let untrimmed = trimmed.trim_end_matches(['\\', '/']);

    if untrimmed.is_empty() {
        return PathBuf::from(if trimmed.contains('\\') { "\\" } else { "/" });
    }

    let bytes = untrimmed.as_bytes();

    // Bare drive letter, e.g. `C` or `c:` -> `C:\`
    if bytes.len() == 1 && bytes[0].is_ascii_alphabetic() {
        return PathBuf::from(format!("{}:\\", untrimmed.to_ascii_uppercase()));
    }
    if bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        let letter = bytes[0].to_ascii_uppercase() as char;
        return PathBuf::from(format!("{letter}:\\"));
    }

    // `C:\some\dir` or `C:/some/dir` -> the drive root.
    let mut chars = untrimmed.chars();
    if let (Some(letter), Some(':'), Some(sep)) = (chars.next(), chars.next(), chars.next())
        && letter.is_ascii_alphabetic()
        && (sep == '\\' || sep == '/')
    {
        return PathBuf::from(format!("{}:\\", letter.to_ascii_uppercase()));
    }

    PathBuf::from(untrimmed)
}

/// Normalise a scan target without collapsing it to its volume.
///
/// `C:` becomes `C:\`, but `C:\Users` stays `C:\Users`, because a filesystem scan
/// must walk exactly what the user asked for.
pub fn scan_target(path: &Path) -> PathBuf {
    let raw = path.as_os_str().to_string_lossy().into_owned();
    let trimmed = raw.trim();

    if trimmed.is_empty() {
        return PathBuf::from("/");
    }

    let untrimmed = trimmed.trim_end_matches(['\\', '/']);
    if untrimmed.is_empty() {
        return PathBuf::from(if trimmed.contains('\\') { "\\" } else { "/" });
    }

    let bytes = untrimmed.as_bytes();

    if bytes.len() == 1 && bytes[0].is_ascii_alphabetic() {
        return PathBuf::from(format!("{}:\\", untrimmed.to_ascii_uppercase()));
    }
    if bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        let letter = bytes[0].to_ascii_uppercase() as char;
        return PathBuf::from(format!("{letter}:\\"));
    }

    // Keep the user's path intact so a subdirectory scan stays a subdirectory.
    PathBuf::from(untrimmed)
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

    pub fn usage(root: &Path) -> Result<DiskUsage, String> {
        let c_path = std::ffi::CString::new(root.as_os_str().as_bytes())
            .map_err(|_| "path contains an interior nul byte".to_string())?;

        // SAFETY: `c_path` is a valid NUL-terminated string that outlives
        // the call, and `stat` is fully initialised on success.
        unsafe {
            let mut stat: libc::statvfs = std::mem::zeroed();
            if libc::statvfs(c_path.as_ptr(), &mut stat) != 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }

            let fragment = stat.f_frsize as u64;
            let total = stat.f_blocks as u64 * fragment;
            let available = stat.f_bavail as u64 * fragment;

            Ok(DiskUsage { available, total })
        }
    }

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

    /// Read mount points from `/proc/mounts` where available, otherwise fall
    /// back to well-known roots.
    fn candidates() -> Vec<PathBuf> {
        if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
            let mut roots: Vec<PathBuf> = Vec::new();
            for line in mounts.lines() {
                let Some((device, mount)) = line.split_whitespace().next_chunk::<2>() else {
                    continue;
                };
                if !device.starts_with('/') {
                    continue;
                }
                let root = PathBuf::from(mount);
                if !roots.contains(&root) {
                    roots.push(root);
                }
            }
            if !roots.is_empty() {
                roots.sort();
                return roots;
            }
        }

        ["/", "/home", "/mnt", "/media", "/run/media"]
            .iter()
            .map(PathBuf::from)
            .collect()
    }

    /// Split a whitespace-delimited pair, used by the `/proc/mounts` reader.
    trait NextChunk: Iterator {
        fn next_chunk<const N: usize>(self) -> Option<[Self::Item; N]>;
    }

    impl<I: Iterator> NextChunk for I {
        fn next_chunk<const N: usize>(mut self) -> Option<[Self::Item; N]> {
            let mut out = Vec::with_capacity(N);
            for _ in 0..N {
                out.push(self.next()?);
            }
            out.try_into().ok()
        }
    }
}

/// List all drives on the current platform.
pub fn list() -> Vec<Drive> {
    platform::list_drives()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn volume_root_expands_bare_drive_letter() {
        assert_eq!(volume_root(Path::new("C:")), PathBuf::from("C:\\"));
        assert_eq!(volume_root(Path::new("c:")), PathBuf::from("C:\\"));
        assert_eq!(volume_root(Path::new("d")), PathBuf::from("D:\\"));
    }

    #[test]
    fn volume_root_reduces_drive_path_to_its_root() {
        assert_eq!(volume_root(Path::new("C:\\Users")), PathBuf::from("C:\\"));
        assert_eq!(volume_root(Path::new("C:/Users/me")), PathBuf::from("C:\\"));
    }

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

    #[test]
    fn scan_target_expands_a_bare_drive_letter() {
        assert_eq!(scan_target(Path::new("C:")), PathBuf::from("C:\\"));
        assert_eq!(scan_target(Path::new("d")), PathBuf::from("D:\\"));
        assert_eq!(scan_target(Path::new("D:\\")), PathBuf::from("D:\\"));
    }

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
