//! Windows integration: start with Windows, firewall rule, single instance,
//! opening folders. All no-ops (or errors) on other platforms.

use std::path::Path;

use anyhow::{Result, bail};

/// UDP ports the receiver sends to (NTP timing requests, retransmit
/// requests). Fixed so one firewall rule can allow exactly these.
pub const TIMING_PORT: u16 = 50610;
pub const CONTROL_PORT: u16 = 50611;

pub fn local_ports() -> airplay::session::LocalPorts {
    airplay::session::LocalPorts {
        timing: TIMING_PORT,
        control: CONTROL_PORT,
    }
}

/// Name of our inbound firewall rule.
///
/// Port-based on purpose: an equivalent program-based rule (for python.exe,
/// verified as the socket owner) silently failed to apply during development,
/// while address/port rules worked immediately.
pub const FIREWALL_RULE: &str = "simonaircast (AirPlay timing)";

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg(windows)]
const RUN_VALUE: &str = "simonaircast";

#[cfg(windows)]
fn netsh_path() -> std::path::PathBuf {
    std::env::var_os("WINDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"))
        .join("System32")
        .join("netsh.exe")
}

/// Whether we're registered to start at login.
pub fn autostart_enabled() -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Registry::{
            HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegGetValueW,
        };
        let (key, value) = (wide(RUN_KEY), wide(RUN_VALUE));
        let mut size = 0u32;
        // SAFETY: valid NUL-terminated strings; null data pointer only queries the size.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        };
        status == 0
    }
    #[cfg(not(windows))]
    false
}

/// Adds or removes `exe` from `HKCU\...\Run`.
pub fn set_autostart(enabled: bool, exe: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Registry::{
            HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW,
        };
        let (key, value) = (wide(RUN_KEY), wide(RUN_VALUE));
        // SAFETY: valid NUL-terminated strings; data length is in bytes incl. NUL.
        let status = unsafe {
            if enabled {
                let command = wide(&format!("\"{}\"", exe.display()));
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    key.as_ptr(),
                    value.as_ptr(),
                    REG_SZ,
                    command.as_ptr().cast(),
                    (command.len() * 2) as u32,
                )
            } else {
                RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr())
            }
        };
        // ERROR_FILE_NOT_FOUND when deleting a value that isn't there is fine.
        if status != 0 && !(status == 2 && !enabled) {
            bail!("updating the Run registry key failed (error {status})");
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (enabled, exe);
        bail!("start with Windows is only supported on Windows")
    }
}

/// Program rules: one per installed exe, so Windows never shows its
/// "allow access?" dialog the first time each one opens a socket.
const PROGRAM_RULES: [(&str, &str); 2] = [
    ("simonaircast", "simonaircast-tray.exe"),
    ("simonaircast command-line tool", "simonaircast.exe"),
];
/// Names Windows gave auto-created rules for earlier per-user installs.
const LEGACY_RULES: [&str; 2] = ["simonaircast.exe", "simonaircast-tray.exe"];

/// Whether our port rule exists. Reads the firewall policy through COM (no
/// admin needed, no helper process).
pub fn firewall_rule_exists() -> bool {
    #[cfg(windows)]
    {
        use windows::Win32::NetworkManagement::WindowsFirewall::{INetFwPolicy2, NetFwPolicy2};
        use windows::Win32::System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
        };
        use windows::core::BSTR;
        // SAFETY: COM calls on this thread; CoInitializeEx is idempotent (an
        // already-initialised apartment just returns S_FALSE/RPC_E_CHANGED_MODE).
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let Ok(policy) =
                CoCreateInstance::<_, INetFwPolicy2>(&NetFwPolicy2, None, CLSCTX_INPROC_SERVER)
            else {
                return false;
            };
            policy
                .Rules()
                .and_then(|rules| rules.Item(&BSTR::from(FIREWALL_RULE)))
                .is_ok()
        }
    }
    #[cfg(not(windows))]
    true
}

/// Creates (or recreates) all inbound rules for an install in `app_dir`.
/// Must run elevated — the installer does this once via `setup-firewall`.
pub fn setup_firewall(app_dir: &Path) -> Result<()> {
    remove_firewall();
    let mut failures = Vec::new();
    let port_rule = format!(
        "name={FIREWALL_RULE}|dir=in|action=allow|protocol=UDP|localport={TIMING_PORT}-{CONTROL_PORT}|remoteip=localsubnet|profile=any"
    );
    if netsh_add(&port_rule) != 0 {
        failures.push(FIREWALL_RULE.to_string());
    }
    for (name, exe) in PROGRAM_RULES {
        let program = app_dir.join(exe);
        let rule = format!(
            "name={name}|dir=in|action=allow|program={}|remoteip=localsubnet|profile=any",
            program.display()
        );
        if netsh_add(&rule) != 0 {
            failures.push(name.to_string());
        }
    }
    if !failures.is_empty() {
        bail!(
            "could not add firewall rule(s) {} (needs administrator rights)",
            failures.join(", ")
        );
    }
    Ok(())
}

/// Deletes our rules and legacy auto-created ones (best effort, needs admin).
pub fn remove_firewall() {
    let names = std::iter::once(FIREWALL_RULE)
        .chain(PROGRAM_RULES.iter().map(|(name, _)| *name))
        .chain(LEGACY_RULES);
    for name in names {
        netsh(&[
            "advfirewall",
            "firewall",
            "delete",
            "rule",
            &format!("name={name}"),
        ]);
    }
}

/// `netsh advfirewall firewall add rule` with `|`-separated key=value fields
/// (values may contain spaces; each field is passed as one argument).
fn netsh_add(fields: &str) -> i32 {
    let mut args = vec!["advfirewall", "firewall", "add", "rule"];
    args.extend(fields.split('|'));
    netsh(&args)
}

/// Runs netsh without a console window; returns its exit code (-1 on failure).
fn netsh(args: &[&str]) -> i32 {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        std::process::Command::new(netsh_path())
            .args(args)
            .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()
            .and_then(|s| s.code())
            .unwrap_or(-1)
    }
    #[cfg(not(windows))]
    {
        let _ = args;
        -1
    }
}

/// For the tray's "Repair firewall rule…": runs `simonaircast.exe
/// setup-firewall` from the install folder elevated (one UAC prompt, only
/// because the user asked). Returns whether the rule exists afterwards.
pub fn repair_firewall_elevated() -> Result<bool> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{INFINITE, WaitForSingleObject};
        use windows_sys::Win32::UI::Shell::{
            SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
        };
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

        let cli = std::env::current_exe()?.with_file_name("simonaircast.exe");
        let (verb, file, params) = (
            wide("runas"),
            wide(&cli.display().to_string()),
            wide("setup-firewall"),
        );
        // SAFETY: zeroed is a valid initial state for this plain C struct.
        let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.lpParameters = params.as_ptr();
        info.nShow = SW_HIDE;
        // SAFETY: `info` and the strings it points to outlive the call.
        if unsafe { ShellExecuteExW(&mut info) } == 0 {
            return Ok(false); // most likely the UAC prompt was declined
        }
        if !info.hProcess.is_null() {
            // SAFETY: process handle from ShellExecuteExW; closed once.
            unsafe {
                WaitForSingleObject(info.hProcess, INFINITE);
                CloseHandle(info.hProcess);
            }
        }
        Ok(firewall_rule_exists())
    }
    #[cfg(not(windows))]
    Ok(true)
}

/// Opens a folder in Explorer.
pub fn open_folder(path: &Path) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let (verb, target) = (wide("open"), wide(&path.display().to_string()));
        // SAFETY: valid NUL-terminated strings; no window handle needed.
        unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                verb.as_ptr(),
                target.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            );
        }
    }
    #[cfg(not(windows))]
    let _ = path;
}

/// Holds a named mutex for the life of the process; `None` if another
/// instance already holds it.
pub struct SingleInstance(#[cfg(windows)] windows_sys::Win32::Foundation::HANDLE);

impl SingleInstance {
    pub fn acquire(name: &str) -> Option<Self> {
        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
            use windows_sys::Win32::System::Threading::CreateMutexW;
            let name = wide(&format!("Local\\{name}"));
            // SAFETY: valid NUL-terminated name; default security attributes.
            let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
            // SAFETY: reads the thread's last error right after the call.
            if handle.is_null() || unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                return None;
            }
            Some(Self(handle))
        }
        #[cfg(not(windows))]
        {
            let _ = name;
            Some(Self())
        }
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        #[cfg(windows)]
        // SAFETY: handle from CreateMutexW, closed once.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::netsh_path;

    #[test]
    fn netsh_is_resolved_from_the_windows_system_directory() {
        assert!(
            netsh_path()
                .to_string_lossy()
                .ends_with(r"System32\netsh.exe")
        );
    }
}
