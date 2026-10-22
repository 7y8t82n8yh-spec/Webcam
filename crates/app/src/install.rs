//! `iphone-webcam install` / `uninstall` on Windows: register the camera's media source
//! (iPhoneWebcamVCam.dll) and open the firewall. Needs administrator rights; when
//! started without, it asks for them (UAC) and runs itself again, elevated.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};
use windows_registry::LOCAL_MACHINE;

const DLL_NAME: &str = "iPhoneWebcamVCam.dll";
const TCP_RULE: &str = "iPhone Webcam";
const UDP_RULE: &str = "iPhone Webcam (WebRTC video)";

pub fn install(port: u16, dll: Option<PathBuf>) -> Result<i32> {
    let dll = find_dll(dll)?;
    if !is_admin() {
        // The elevated copy starts in System32, hence the absolute DLL path.
        let args = format!("--elevated --port {port} install --dll \"{}\"", dll.display());
        return run_elevated(&args);
    }
    let build = windows_build()?;
    if build < 22000 {
        bail!("iPhone Webcam needs Windows 11 (this is build {build})");
    }

    // The camera service loads the DLL, so it goes where only administrators can write.
    let installed = install_dir().join(DLL_NAME);
    std::fs::create_dir_all(install_dir())?;
    if installed.exists() && std::fs::remove_file(&installed).is_err() {
        // The camera service still has the old one loaded: a loaded DLL can't be
        // deleted, but it can be renamed out of the way.
        let aside = installed.with_extension(format!("{}.old", rand::random::<u32>()));
        std::fs::rename(&installed, aside).context("moving the old DLL aside")?;
    }
    std::fs::copy(&dll, &installed).with_context(|| format!("copying {}", dll.display()))?;

    // What regsvr32 would do: tell COM where the class lives.
    let class = LOCAL_MACHINE.create(class_key())?;
    class.set_string("", "iPhone Webcam virtual camera")?;
    let server = class.create("InprocServer32")?;
    server.set_string("", installed.to_string_lossy())?;
    server.set_string("ThreadingModel", "Both")?;
    println!("Registered the virtual camera ({}).", installed.display());

    // The camera service never unloads a DLL; restart it so an upgrade takes effect
    // now. It starts again by itself when a camera is next used.
    if run_quietly("sc", &["stop", "FrameServer"]) {
        println!("Restarted the Windows camera service.");
    }

    // The HTTPS port, and UDP for this program (WebRTC picks its ports at random).
    let exe = std::env::current_exe()?;
    let tcp_port = format!("localport={port}");
    let program = format!("program={}", exe.display());
    let _ = netsh(&["delete", "rule", &format!("name={TCP_RULE}")]);
    let _ = netsh(&["delete", "rule", &format!("name={UDP_RULE}")]);
    let allow = ["dir=in", "action=allow", "profile=any"];
    netsh(&[&["add", "rule", &format!("name={TCP_RULE}"), "protocol=TCP", &tcp_port][..], &allow].concat())?;
    netsh(&[&["add", "rule", &format!("name={UDP_RULE}"), "protocol=UDP", &program][..], &allow].concat())?;
    println!("Firewall: allowed TCP {port} and UDP for {}.", exe.display());

    println!("\nDone. Start iPhone Webcam; the camera appears in video apps while it runs.");
    Ok(0)
}

pub fn uninstall() -> Result<i32> {
    if !is_admin() {
        return run_elevated("--elevated uninstall");
    }
    let _ = LOCAL_MACHINE.remove_tree(class_key());
    // The DLL may stay behind while the camera service has it loaded.
    let _ = std::fs::remove_dir_all(install_dir());
    let _ = netsh(&["delete", "rule", &format!("name={TCP_RULE}")]);
    let _ = netsh(&["delete", "rule", &format!("name={UDP_RULE}")]);
    println!("Virtual camera and firewall rules removed.");
    Ok(0)
}

fn install_dir() -> PathBuf {
    let program_files = std::env::var_os("ProgramFiles").unwrap_or_else(|| r"C:\Program Files".into());
    PathBuf::from(program_files).join("iPhoneWebcam").join("vcam")
}

fn class_key() -> String {
    format!(r"SOFTWARE\Classes\CLSID\{}", protocol::frame_section::VCAM_CLSID)
}

/// The DLL given on the command line, else the one next to this program.
fn find_dll(explicit: Option<PathBuf>) -> Result<PathBuf> {
    let path = match explicit {
        Some(path) => path,
        None => std::env::current_exe()?.with_file_name(DLL_NAME),
    };
    if !path.exists() {
        bail!("{} not found; pass --dll <path>", path.display());
    }
    Ok(std::path::absolute(path)?)
}

fn is_admin() -> bool {
    // SAFETY: no arguments, no pointers; it only reads this process's token.
    unsafe { windows::Win32::UI::Shell::IsUserAnAdmin() }.as_bool()
}

/// Run this program again with administrator rights (the UAC prompt) and wait for it.
fn run_elevated(args: &str) -> Result<i32> {
    println!("Requesting administrator rights...");
    let exe = std::env::current_exe()?;
    // PowerShell single-quoted strings escape ' as ''.
    let script = format!(
        "$p = Start-Process -FilePath '{}' -ArgumentList '{}' -Verb RunAs -Wait -PassThru; exit $p.ExitCode",
        exe.display().to_string().replace('\'', "''"),
        args.replace('\'', "''"),
    );
    let status = Command::new("powershell").args(["-NoProfile", "-Command", &script]).status()?;
    match status.code() {
        Some(code) => Ok(code),
        None => bail!("administrator rights were not granted"),
    }
}

/// The Windows build number, as `winver` shows it.
fn windows_build() -> Result<u32> {
    let key = LOCAL_MACHINE.open(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion")?;
    Ok(key.get_string("CurrentBuildNumber")?.parse()?)
}

fn netsh(args: &[&str]) -> Result<()> {
    let output = Command::new("netsh").args(["advfirewall", "firewall"]).args(args).output()?;
    if !output.status.success() {
        bail!("netsh {args:?} failed: {}", String::from_utf8_lossy(&output.stdout).trim());
    }
    Ok(())
}

fn run_quietly(program: &str, args: &[&str]) -> bool {
    Command::new(program).args(args).output().is_ok_and(|output| output.status.success())
}
