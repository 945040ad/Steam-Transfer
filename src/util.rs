use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn no_window(c: &mut Command) -> &mut Command {
    c.creation_flags(CREATE_NO_WINDOW)
}

pub fn size(b: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", units[i])
    } else if v >= 10.0 {
        format!("{v:.1} {}", units[i])
    } else {
        format!("{v:.2} {}", units[i])
    }
}

pub fn time(d: Duration) -> String {
    let s = d.as_secs();
    let (h, m, x) = (s / 3600, s % 3600 / 60, s % 60);
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {x:02}s")
    } else {
        format!("{x}s")
    }
}

pub fn link(bps: u64) -> String {
    if bps >= 1_000_000_000 {
        let g = bps as f64 / 1e9;
        if (g - g.round()).abs() < 0.05 { format!("{:.0} Gbps", g) } else { format!("{g:.1} Gbps") }
    } else {
        format!("{} Mbps", bps / 1_000_000)
    }
}

/// Drive root like "E:\" for any path on a lettered drive.
pub fn drive_of(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    let b = s.as_bytes();
    if b.len() >= 2 && b[1] == b':' { PathBuf::from(format!("{}:\\", &s[..1].to_ascii_uppercase())) } else { p.to_path_buf() }
}

pub fn drive_gone(p: &Path) -> bool {
    !drive_of(p).is_dir()
}

pub fn free_space(p: &Path) -> u64 {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = drive_of(p).as_os_str().encode_wide().chain(Some(0)).collect();
    let mut free: u64 = 0;
    unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut());
    }
    free
}

/// .NET-compatible timestamps (100 ns ticks since 0001-01-01 UTC) so the C# and Rust versions understand each other.
const UNIX_EPOCH_TICKS: i64 = 621_355_968_000_000_000;

pub fn to_ticks(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => UNIX_EPOCH_TICKS + (d.as_nanos() / 100) as i64,
        Err(e) => UNIX_EPOCH_TICKS - (e.duration().as_nanos() / 100) as i64,
    }
}

pub fn from_ticks(ticks: i64) -> SystemTime {
    let rel = ticks - UNIX_EPOCH_TICKS;
    if rel >= 0 { UNIX_EPOCH + Duration::from_nanos(rel as u64 * 100) } else { UNIX_EPOCH - Duration::from_nanos((-rel) as u64 * 100) }
}

pub fn machine_name() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "PC".into())
}

pub fn local_app_data() -> PathBuf {
    PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ticks_roundtrip() {
        let now = SystemTime::now();
        let back = from_ticks(to_ticks(now));
        let diff = now.duration_since(back).unwrap_or_else(|e| e.duration());
        assert!(diff < Duration::from_micros(1));
        assert_eq!(to_ticks(UNIX_EPOCH), 621_355_968_000_000_000);
    }
    #[test]
    fn formats() {
        assert_eq!(size(1536), "1.50 KB");
        assert_eq!(link(1_000_000_000), "1 Gbps");
        assert_eq!(link(2_500_000_000), "2.5 Gbps");
        assert_eq!(drive_of(Path::new(r"e:\games\x")), PathBuf::from(r"E:\"));
    }
}
