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
    let status = Command::new("regsvr32").arg("/s").args(args).arg(dll).status().context("running regsvr32")?;
    if !status.success() {
        bail!("regsvr32 {args:?} {} failed ({status})", dll.display());
    }
    Ok(())
}

fn netsh(args: &[&str]) -> Result<()> {
    let output =
        Command::new("netsh").args(["advfirewall", "firewall"]).args(args).output().context("running netsh")?;
    if !output.status.success() {
        bail!("netsh {args:?} failed: {}", String::from_utf8_lossy(&output.stdout).trim());
    }
    Ok(())
}

/// Re-run this executable elevated with the same arguments and wait for it.
fn relaunch_elevated() -> Result<i32> {
    let exe = std::env::current_exe()?;
    let args: Vec<String> = std::env::args().skip(1).map(|a| format!("\"{a}\"")).collect();
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
    let exe_dir = std::env::current_exe()?.parent().map(Path::to_path_buf).unwrap_or_default();
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
    if !unsafe { IsUserAnAdmin() }.as_bool() {
        println!("Requesting administrator rights...");
        return relaunch_elevated();
    }
    let build = windows_build();
    if build < 22000 {
        bail!("iPhone Webcam needs Windows 11 (this is build {build})");
    }
    let source = find_dll(dll)?;
    let dir = install_dir().join("vcam");
    std::fs::create_dir_all(&dir)?;
    let target = dir.join(DLL_NAME);
    if target.exists() {
        let _ = regsvr32(&["/u"], &target);
    }
    copy_replacing(&source, &target)?;
    regsvr32(&[], &target)?;
    println!("Registered the virtual camera ({}).", target.display());
    remove_legacy();

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
    println!("\nDone. Start iPhone Webcam; the camera appears in video apps while it runs.");
    Ok(0)
}

pub fn uninstall() -> Result<i32> {
    if !unsafe { IsUserAnAdmin() }.as_bool() {
        println!("Requesting administrator rights...");
        return relaunch_elevated();
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

fn windows_build() -> u32 {
    // The current build number is in the registry; it's what winver shows.
    let output = Command::new("reg")
        .args(["query", r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion", "/v", "CurrentBuildNumber"])
        .output();
    output
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.split_whitespace().last().and_then(|n| n.parse().ok()))
        .unwrap_or(0)
}
