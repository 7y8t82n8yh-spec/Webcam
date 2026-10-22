//! `iphone-webcam install` / `uninstall`: register the Windows 11 camera's media
//! source and open the firewall. Needs administrator rights; asks for them (UAC) and
//! re-runs itself elevated when started without.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND};
use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
use windows::Win32::UI::Shell::{IsUserAnAdmin, ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

const DLL_NAME: &str = "iPhoneWebcamVCam.dll";
const TCP_RULE: &str = "iPhone Webcam";
const UDP_RULE: &str = "iPhone Webcam (WebRTC video)";

fn install_dir() -> PathBuf {
    let program_files = std::env::var_os("ProgramFiles").unwrap_or_else(|| r"C:\Program Files".into());
    // Admin-only, so the DLL (loaded by the camera service) can't be swapped by others.
    PathBuf::from(program_files).join("iPhoneWebcam")
}

fn regsvr32(args: &[&str], dll: &Path) -> Result<()> {
    let mut command = Command::new("regsvr32");
    command.arg("/s").args(args).arg(dll);
    let status = command.status().context("running regsvr32")?;
    if !status.success() {
        bail!("regsvr32 {args:?} {} failed ({status})", dll.display());
    }
    Ok(())
}

fn netsh(args: &[&str]) -> Result<()> {
    let mut command = Command::new("netsh");
    command.args(["advfirewall", "firewall"]).args(args);
    let output = command.output().context("running netsh")?;
    if !output.status.success() {
        bail!("netsh {args:?} failed: {}", String::from_utf8_lossy(&output.stdout).trim());
    }
    Ok(())
}

/// Quote one argument for the command line, the way the C runtime parses it back.
fn quote(arg: &str) -> String {
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            // Backslashes before a quote are doubled, and the quote itself escaped.
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    // Same before the closing quote.
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

/// This process's arguments, quoted, with `--dll` replaced by the absolute `dll` and
/// `--pause-on-error` added.
fn elevated_args(dll: Option<&Path>) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    let mut skip_next = false;
    for arg in std::env::args().skip(1) {
        if std::mem::take(&mut skip_next) {
            continue;
        }
        if arg == "--dll" {
            skip_next = true;
        } else if !arg.starts_with("--dll=") {
            args.push(quote(&arg));
        }
    }
    if let Some(dll) = dll {
        args.push("--dll".into());
        args.push(quote(&dll.to_string_lossy()));
    }
    args.push("--pause-on-error".into());
    args
}

/// Re-run this executable elevated and wait for it. The elevated process starts in
/// System32 and its console closes when it exits, so paths are made absolute and it
/// is asked to pause on errors.
fn relaunch_elevated(dll: Option<&Path>) -> Result<i32> {
    let exe = std::env::current_exe()?;
    let args = elevated_args(dll);
    let (file, params) = (HSTRING::from(exe.as_os_str()), HSTRING::from(args.join(" ")));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        hwnd: HWND::default(),
        lpVerb: w!("runas"),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe {
        ShellExecuteExW(&mut info).context("administrator rights were not granted")?;
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 1u32;
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
        Ok(code as i32)
    }
}

/// Replace `target` with `source`; a DLL loaded by the camera service can't be
/// overwritten, but it can be renamed out of the way.
fn copy_replacing(source: &Path, target: &Path) -> Result<()> {
    if target.exists() && std::fs::remove_file(target).is_err() {
        let aside = target.with_extension(format!("{}.old", rand::random::<u32>()));
        std::fs::rename(target, aside).context("moving the old DLL aside")?;
    }
    std::fs::copy(source, target).with_context(|| format!("copying {}", source.display()))?;
    Ok(())
}

fn find_dll(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    let exe = std::env::current_exe()?;
    let exe_dir = exe.parent().map(Path::to_path_buf).unwrap_or_default();
    for candidate in [exe_dir.join(DLL_NAME), exe_dir.join("..").join(DLL_NAME)] {
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    bail!("{DLL_NAME} not found next to {}; pass --dll <path>", exe_dir.display())
}

/// Remove what the earlier (Python) version installed: the Unity Capture filter,
/// which would otherwise show up as a second "iPhone Webcam".
fn remove_legacy() {
    let dir = install_dir().join("driver");
    for dll in ["UnityCaptureFilter64.dll", "UnityCaptureFilter32.dll"] {
        let path = dir.join(dll);
        if path.exists() {
            let _ = regsvr32(&["/u"], &path);
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}

pub fn install(port: u16, dll: Option<PathBuf>) -> Result<i32> {
    let dll = dll.map(|p| std::path::absolute(&p).unwrap_or(p));
    if !unsafe { IsUserAnAdmin() }.as_bool() {
        println!("Requesting administrator rights...");
        return relaunch_elevated(dll.as_deref());
    }
    let build = windows_build();
    if build < 22000 {
        bail!("iPhone Webcam needs Windows 11 (this is build {build})");
    }
    let source = find_dll(dll)?;
    let target = register_dll(&source)?;
    println!("Registered the virtual camera ({}).", target.display());
    // The camera service never unloads the DLL it has; restart it so an upgrade takes
    // effect now. It starts again by itself when a camera is next used.
    let stop = Command::new("sc").args(["stop", "FrameServer"]).output();
    if stop.is_ok_and(|o| o.status.success()) {
        println!("Restarted the Windows camera service.");
    }
    remove_legacy();
    open_firewall(port)?;
    println!("\nDone. Start iPhone Webcam; the camera appears in video apps while it runs.");
    Ok(0)
}

/// Copy the DLL into the install folder (replacing an earlier one) and register it.
fn register_dll(source: &Path) -> Result<PathBuf> {
    let dir = install_dir().join("vcam");
    std::fs::create_dir_all(&dir)?;
    let target = dir.join(DLL_NAME);
    if target.exists() {
        let _ = regsvr32(&["/u"], &target);
    }
    copy_replacing(source, &target)?;
    regsvr32(&[], &target)?;
    Ok(target)
}

/// Allow the TCP port and this program's UDP (WebRTC) traffic in, replacing earlier rules.
fn open_firewall(port: u16) -> Result<()> {
    let _ = netsh(&["delete", "rule", &format!("name={TCP_RULE}")]);
    netsh(&[
        "add",
        "rule",
        &format!("name={TCP_RULE}"),
        "dir=in",
        "action=allow",
        "protocol=TCP",
        &format!("localport={port}"),
        "profile=any",
    ])?;
    let exe = std::env::current_exe()?;
    let _ = netsh(&["delete", "rule", &format!("name={UDP_RULE}")]);
    netsh(&[
        "add",
        "rule",
        &format!("name={UDP_RULE}"),
        "dir=in",
        "action=allow",
        "protocol=UDP",
        &format!("program={}", exe.display()),
        "profile=any",
    ])?;
    println!("Firewall: allowed TCP {port} and UDP for {}.", exe.display());
    Ok(())
}

pub fn uninstall() -> Result<i32> {
    if !unsafe { IsUserAnAdmin() }.as_bool() {
        println!("Requesting administrator rights...");
        return relaunch_elevated(None);
    }
    let target = install_dir().join("vcam").join(DLL_NAME);
    if target.exists() {
        let _ = regsvr32(&["/u"], &target);
    }
    let _ = std::fs::remove_dir_all(install_dir().join("vcam")); // may linger while loaded
    remove_legacy();
    let _ = netsh(&["delete", "rule", &format!("name={TCP_RULE}")]);
    let _ = netsh(&["delete", "rule", &format!("name={UDP_RULE}")]);
    println!("Virtual camera and firewall rules removed.");
    Ok(0)
}

#[cfg(test)]
mod tests {
    #[test]
    fn quoting_survives_backslashes_and_quotes() {
        assert_eq!(super::quote(r"C:\dir\"), r#""C:\dir\\""#);
        assert_eq!(super::quote(r#"a"b"#), r#""a\"b""#);
        assert_eq!(super::quote(r"a\b"), r#""a\b""#);
        assert_eq!(super::quote(r#"a\"b"#), r#""a\\\"b""#);
    }
}

fn windows_build() -> u32 {
    // The current build number is in the registry; it's what winver shows.
    let output = Command::new("reg")
        .args(["query", r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion", "/v", "CurrentBuildNumber"])
        .output();
    let Ok(output) = output else { return 0 };
    let Ok(text) = String::from_utf8(output.stdout) else { return 0 };
    let build = text.split_whitespace().last();
    build.and_then(|n| n.parse().ok()).unwrap_or(0)
}
