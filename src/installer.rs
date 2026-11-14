//! One-click per-user install (no admin except the single firewall prompt), uninstall,
//! single-instance lock and small Win32 helpers.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{CreateMutexW, INFINITE, WaitForSingleObject};
use windows_sys::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows_sys::Win32::UI::WindowsAndMessaging::{IDYES, MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK, MB_YESNO, MessageBoxW, SW_HIDE};
use winreg::RegKey;
use winreg::enums::HKEY_CURRENT_USER;

use crate::net::firewall;
use crate::util::{self, no_window};

pub const APP_NAME: &str = "Steam Game Transfer";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\SteamGameTransfer";

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

pub fn install_dir() -> PathBuf {
    util::local_app_data().join("Programs").join("SteamTransfer")
}
pub fn installed_exe() -> PathBuf {
    install_dir().join("SteamTransfer.exe")
}
pub fn current_exe() -> PathBuf {
    std::env::current_exe().unwrap_or_default()
}
pub fn is_running_installed() -> bool {
    same_path(&current_exe(), &installed_exe())
}
fn same_path(a: &Path, b: &Path) -> bool {
    let n = |p: &Path| std::path::absolute(p).unwrap_or(p.to_path_buf()).to_string_lossy().to_lowercase();
    n(a) == n(b)
}
fn start_menu_link() -> PathBuf {
    PathBuf::from(std::env::var("APPDATA").unwrap_or_default()).join(r"Microsoft\Windows\Start Menu\Programs").join(format!("{APP_NAME}.lnk"))
}
fn desktop_link() -> PathBuf {
    PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default()).join("Desktop").join(format!("{APP_NAME}.lnk"))
}

/// Copies the exe to %LocalAppData%\Programs, adds shortcuts, an "Installed apps" entry and the firewall rule.
pub fn install() -> std::io::Result<()> {
    std::fs::create_dir_all(install_dir())?;
    if !is_running_installed() {
        std::fs::copy(current_exe(), installed_exe())?;
    }
    let exe = installed_exe();
    create_shortcut(&start_menu_link(), &exe);
    create_shortcut(&desktop_link(), &exe);

    let (key, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(UNINSTALL_KEY)?;
    let exe_s = exe.to_string_lossy().to_string();
    key.set_value("DisplayName", &APP_NAME)?;
    key.set_value("DisplayVersion", &VERSION)?;
    key.set_value("Publisher", &APP_NAME)?;
    key.set_value("DisplayIcon", &exe_s)?;
    key.set_value("InstallLocation", &install_dir().to_string_lossy().to_string())?;
    key.set_value("UninstallString", &format!("\"{exe_s}\" --uninstall"))?;
    key.set_value("NoModify", &1u32)?;
    key.set_value("NoRepair", &1u32)?;
    key.set_value("EstimatedSize", &((std::fs::metadata(&exe).map(|m| m.len()).unwrap_or(0) / 1024) as u32))?;

    if !firewall::rule_exists() {
        firewall::add_rules();
    }
    Ok(())
}

pub fn launch_installed() {
    let _ = Command::new(installed_exe()).arg("--after-install").spawn();
}

pub fn uninstall() {
    if !ask_yes_no("Remove Steam Game Transfer from this PC?\nYour games are not touched.") {
        return;
    }
    let _ = std::fs::remove_file(start_menu_link());
    let _ = std::fs::remove_file(desktop_link());
    let _ = RegKey::predef(HKEY_CURRENT_USER).delete_subkey_all(UNINSTALL_KEY);
    firewall::remove_rules();
    // The running exe can't delete itself; let cmd do it a moment after we exit.
    let script = format!(
        "timeout /t 2 /nobreak >nul & rmdir /s /q \"{}\" & rmdir /s /q \"{}\"",
        install_dir().display(),
        util::local_app_data().join("SteamTransfer").display()
    );
    let _ = no_window(Command::new("cmd.exe").args(["/c", &script])).spawn();
    message("Steam Game Transfer was removed.");
}

/// What `ensure_app_copy` found or did.
#[derive(Debug, PartialEq)]
pub enum AppCopy {
    /// An identical copy is already there (or the app is running from that very file).
    Present,
    /// There was none (deleted or moved away): a fresh copy was put there.
    Copied,
    /// There was a damaged, partial or older copy: replaced.
    Updated,
    /// A newer version of the app is already there: left alone.
    NewerThere,
}

/// Makes sure a working copy of this app is at `folder\SteamTransfer.exe`, so whoever gets the drive next can run it
/// straight from there — even if the copy was deleted, cut/moved off the drive, cut short or is an older version.
/// The copy is written to a temporary file first and renamed into place, so pulling the drive out mid-copy never
/// leaves a broken exe behind.
pub fn ensure_app_copy(folder: &Path) -> std::io::Result<AppCopy> {
    let me = current_exe();
    let dest = folder.join("SteamTransfer.exe");
    if same_path(&dest, &me) {
        return Ok(AppCopy::Present);
    }
    let existed = dest.is_file();
    if existed {
        let same = std::fs::metadata(&dest)?.len() == std::fs::metadata(&me)?.len() && std::fs::read(&dest)? == std::fs::read(&me)?;
        if same {
            return Ok(AppCopy::Present);
        }
        if let (Some(theirs), Some(mine)) = (exe_version(&dest), exe_version(&me)) {
            if theirs > mine {
                return Ok(AppCopy::NewerThere);
            }
        }
    }
    std::fs::create_dir_all(folder)?;
    let tmp = folder.join("SteamTransfer.exe.partial");
    std::fs::copy(&me, &tmp)?;
    if let Err(e) = std::fs::rename(&tmp, &dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e); // e.g. someone is running the copy on the drive right now
    }
    Ok(if existed { AppCopy::Updated } else { AppCopy::Copied })
}

/// File version from an exe's version resource, e.g. (2, 1, 0, 0).
pub fn exe_version(path: &Path) -> Option<(u16, u16, u16, u16)> {
    use windows_sys::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VS_FIXEDFILEINFO, VerQueryValueW};
    let w = wide(&path.to_string_lossy());
    unsafe {
        let size = GetFileVersionInfoSizeW(w.as_ptr(), std::ptr::null_mut());
        if size == 0 {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        if GetFileVersionInfoW(w.as_ptr(), 0, size, buf.as_mut_ptr().cast()) == 0 {
            return None;
        }
        let root = wide("\\");
        let mut info: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut len = 0u32;
        if VerQueryValueW(buf.as_ptr().cast(), root.as_ptr(), &mut info, &mut len) == 0 || info.is_null() || (len as usize) < std::mem::size_of::<VS_FIXEDFILEINFO>() {
            return None;
        }
        let f = &*(info as *const VS_FIXEDFILEINFO);
        Some(((f.dwFileVersionMS >> 16) as u16, f.dwFileVersionMS as u16, (f.dwFileVersionLS >> 16) as u16, f.dwFileVersionLS as u16))
    }
}

fn create_shortcut(lnk: &Path, target: &Path) {
    let q = |p: &Path| p.to_string_lossy().replace('\'', "''");
    let ps = format!(
        "$s=(New-Object -ComObject WScript.Shell).CreateShortcut('{}');$s.TargetPath='{}';$s.WorkingDirectory='{}';$s.IconLocation='{},0';$s.Description='Copy Steam games between PCs';$s.Save()",
        q(lnk),
        q(target),
        q(target.parent().unwrap_or(target)),
        q(target)
    );
    let _ = no_window(Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", &ps])).status();
}

// ---------------- Win32 helpers ----------------

/// Runs a program as administrator (UAC prompt) and waits for it. False if the user said no.
pub fn run_elevated(file: &str, params: &str) -> bool {
    let (verb, file, params) = (wide("runas"), wide(file), wide(params));
    unsafe {
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.lpParameters = params.as_ptr();
        info.nShow = SW_HIDE;
        if ShellExecuteExW(&mut info) == 0 {
            return false;
        }
        if !info.hProcess.is_null() {
            WaitForSingleObject(info.hProcess, INFINITE);
            CloseHandle(info.hProcess);
        }
        true
    }
}

pub fn ask_yes_no(text: &str) -> bool {
    let (t, c) = (wide(text), wide(APP_NAME));
    unsafe { MessageBoxW(std::ptr::null_mut(), t.as_ptr(), c.as_ptr(), MB_YESNO | MB_ICONQUESTION) == IDYES }
}

pub fn message(text: &str) {
    let (t, c) = (wide(text), wide(APP_NAME));
    unsafe {
        MessageBoxW(std::ptr::null_mut(), t.as_ptr(), c.as_ptr(), MB_OK | MB_ICONINFORMATION);
    }
}

/// Only one copy at a time (they would fight over the network ports).
pub struct SingleInstance(HANDLE);

impl SingleInstance {
    /// `wait`: the previous copy is closing (after install); wait up to 15 s for it.
    pub fn acquire(wait: bool) -> Option<SingleInstance> {
        let name = wide(r"Local\SteamGameTransfer");
        unsafe {
            let h = CreateMutexW(std::ptr::null(), 1, name.as_ptr());
            if h.is_null() {
                return None;
            }
            if GetLastError() != ERROR_ALREADY_EXISTS {
                return Some(SingleInstance(h));
            }
            if wait {
                let r = WaitForSingleObject(h, 15_000);
                if r == WAIT_OBJECT_0 || r == WAIT_ABANDONED {
                    return Some(SingleInstance(h));
                }
            }
            CloseHandle(h);
            None
        }
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
