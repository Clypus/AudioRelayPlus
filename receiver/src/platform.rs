//! Small OS helpers: child processes without console flashes, opening a
//! folder in the file manager, and the Windows-only firewall / autostart
//! switches.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A `Command` that never flashes a console window on Windows. The GUI build
/// has no console, so every console tool (adb, netsh, pactl...) would
/// otherwise pop up a black window for a moment.
pub fn quiet_command<S: AsRef<OsStr>>(program: S) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Folder of the running executable (portable layout: everything sits next to it).
pub fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(Path::to_path_buf)
}

/// Opens `path` in Explorer / the desktop's file manager (doesn't wait).
pub fn open_folder(path: &Path) -> std::io::Result<()> {
    let program = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let mut child = quiet_command(program).arg(path).spawn()?;
    // reap it, or it lingers as a zombie until we exit
    std::thread::spawn(move || child.wait());
    Ok(())
}

#[cfg(windows)]
pub mod windows {
    //! Windows Firewall rule (elevated through UAC) and "start with Windows".

    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use anyhow::{anyhow, Result};
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
    };
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

    use super::quiet_command;
    use crate::t;

    pub const FIREWALL_RULE: &str = "AudioRelayPlus";
    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const RUN_VALUE: &str = "AudioRelayPlus";

    fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
        s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
    }

    /// True if our inbound rule exists (reading rules needs no admin rights).
    pub fn firewall_rule_present() -> bool {
        quiet_command("netsh")
            .args(["advfirewall", "firewall", "show", "rule"])
            .arg(format!("name={FIREWALL_RULE}"))
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Allows `exe` and UDP `port` through Windows Firewall on every network
    /// profile. Shows the UAC prompt and blocks until netsh is done.
    ///
    /// Rules Windows created for the exe from its first-run prompt are removed
    /// first: if "Public" was left unticked there, a block rule exists for the
    /// exe and block rules beat allow rules.
    pub fn fix_firewall(exe: &Path, port: u16) -> Result<()> {
        let exe = exe.display();
        let rule = FIREWALL_RULE;
        let script = format!(
            "netsh advfirewall firewall delete rule name=all program=\"{exe}\" >nul 2>&1 & \
             netsh advfirewall firewall delete rule name=\"{rule}\" >nul 2>&1 & \
             netsh advfirewall firewall add rule name=\"{rule}\" dir=in action=allow program=\"{exe}\" enable=yes profile=any >nul && \
             netsh advfirewall firewall add rule name=\"{rule}\" dir=in action=allow protocol=UDP localport={port} profile=any >nul"
        );
        // /s: cmd strips exactly the outer quotes and runs the rest verbatim
        run_elevated("cmd.exe", &format!("/s /c \"{script}\""))
    }

    fn run_elevated(file: &str, params: &str) -> Result<()> {
        let verb = wide("runas");
        let file = wide(file);
        let params = wide(params);
        unsafe {
            let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
            info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
            info.lpVerb = verb.as_ptr();
            info.lpFile = file.as_ptr();
            info.lpParameters = params.as_ptr();
            info.nShow = SW_HIDE;
            if ShellExecuteExW(&mut info) == 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(ERROR_CANCELLED as i32) {
                    return Err(anyhow!(t!("the administrator prompt was declined", "yönetici izni verilmedi")));
                }
                return Err(err.into());
            }
            if info.hProcess.is_null() {
                return Err(anyhow!("no process handle"));
            }
            WaitForSingleObject(info.hProcess, 60_000);
            let mut code = 1u32;
            GetExitCodeProcess(info.hProcess, &mut code);
            CloseHandle(info.hProcess);
            if code == 0 {
                Ok(())
            } else {
                Err(anyhow!(t!("netsh failed (exit code {code})", "netsh başarısız (çıkış kodu {code})")))
            }
        }
    }

    fn autostart_command(exe: &Path) -> String {
        format!("\"{}\" --minimized", exe.display())
    }

    /// The command line registered to run at login, if any.
    fn autostart_value() -> Option<String> {
        let key = wide(RUN_KEY);
        let value = wide(RUN_VALUE);
        unsafe {
            let mut size = 0u32;
            let rc = RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            );
            if rc != ERROR_SUCCESS || size < 2 {
                return None;
            }
            let mut buf = vec![0u16; (size as usize).div_ceil(2)];
            let rc = RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                buf.as_mut_ptr().cast(),
                &mut size,
            );
            if rc != ERROR_SUCCESS {
                return None;
            }
            let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            Some(String::from_utf16_lossy(&buf[..len]))
        }
    }

    pub fn autostart_enabled() -> bool {
        autostart_value().is_some()
    }

    /// If autostart is on but points at another copy (the folder was moved),
    /// re-point it at this exe.
    pub fn refresh_autostart(exe: &Path) {
        if let Some(v) = autostart_value() {
            if v != autostart_command(exe) {
                let _ = set_autostart(exe, true);
            }
        }
    }

    pub fn set_autostart(exe: &Path, on: bool) -> Result<()> {
        let key = wide(RUN_KEY);
        let value = wide(RUN_VALUE);
        let rc = unsafe {
            if on {
                let data = wide(autostart_command(exe));
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    key.as_ptr(),
                    value.as_ptr(),
                    REG_SZ,
                    data.as_ptr().cast(),
                    (data.len() * 2) as u32,
                )
            } else {
                match RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr()) {
                    ERROR_FILE_NOT_FOUND => ERROR_SUCCESS,
                    rc => rc,
                }
            }
        };
        if rc == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(std::io::Error::from_raw_os_error(rc as i32).into())
        }
    }
}
