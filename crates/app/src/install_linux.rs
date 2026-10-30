//! `install` / `uninstall` on Linux: set up the v4l2loopback kernel module with a device
//! named after the camera, now and at every boot, and open the HTTPS port in ufw or
//! firewalld when one of them is active. Both need root (`is_admin`, `run_as_admin`);
//! `main` gets it first.

use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

const MODPROBE_CONF: &str = "/etc/modprobe.d/iphone-webcam.conf";
const MODULES_LOAD: &str = "/etc/modules-load.d/iphone-webcam.conf";

fn run(program: &str, args: &[&str]) -> bool {
    Command::new(program).args(args).output().is_ok_and(|o| o.status.success())
}

fn output(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// Whether we run as root: `/proc/self` belongs to the process's effective user.
pub fn is_admin() -> bool {
    std::fs::metadata("/proc/self").is_ok_and(|proc_self| proc_self.uid() == 0)
}

/// Run this program again with `args` through sudo and return its exit code.
pub fn run_as_admin(args: &[String]) -> Result<i32> {
    println!("Administrator rights are needed; running with sudo...");
    let exe = std::env::current_exe()?;
    let status = Command::new("sudo").arg(exe).args(args).status().context("running sudo")?;
    Ok(status.code().unwrap_or(1))
}

fn ours_loaded(name: &str) -> bool {
    crate::v4l2::loopback_devices().iter().any(|device| device.label == name)
}

fn ufw_active() -> bool {
    output("ufw", &["status"]).contains("Status: active")
}

fn firewalld_running() -> bool {
    output("firewall-cmd", &["--state"]).trim() == "running"
}

/// `dll` is only used on Windows.
pub fn install(port: u16, name: &str, _dll: Option<std::path::PathBuf>) -> Result<()> {
    if !run("modinfo", &["v4l2loopback"]) {
        bail!(
            "the v4l2loopback kernel module isn't installed. Install it first, e.g.\n  \
             Debian/Ubuntu: sudo apt install v4l2loopback-dkms\n  \
             Fedora:        sudo dnf install v4l2loopback   (from RPM Fusion)\n  \
             Arch:          sudo pacman -S v4l2loopback-dkms\n\
             then run this again."
        );
    }
    anyhow::ensure!(!name.contains('"'), "camera name can't contain quotes");
    std::fs::write(MODPROBE_CONF, format!("options v4l2loopback devices=1 exclusive_caps=1 card_label=\"{name}\"\n"))
        .with_context(|| MODPROBE_CONF)?;
    std::fs::write(MODULES_LOAD, "v4l2loopback\n").with_context(|| MODULES_LOAD)?;
    println!("v4l2loopback will load at boot with a camera named \"{name}\".");

    if !ours_loaded(name) {
        // Reload to pick up the options; that fails if another program uses the module.
        if Path::new("/sys/module/v4l2loopback").exists() && !run("modprobe", &["-r", "v4l2loopback"]) {
            println!(
                "v4l2loopback is already loaded and in use, so its options can't change now. \
                 Until the next reboot iPhone Webcam uses its first device."
            );
        } else if !run("modprobe", &["v4l2loopback"]) {
            bail!("`modprobe v4l2loopback` failed. With Secure Boot on, the module may need to be signed (see dmesg).");
        }
    }
    match crate::v4l2::loopback_devices().first() {
        Some(device) => println!("Camera device: {} ({}).", device.path.display(), device.label),
        None => println!("No loopback device appeared yet; check `dmesg`."),
    }

    if ufw_active() {
        run("ufw", &["allow", &format!("{port}/tcp"), "comment", "iPhone Webcam"]);
        println!("ufw: allowed TCP {port}.");
    }
    if firewalld_running() {
        run("firewall-cmd", &["--permanent", &format!("--add-port={port}/tcp")]);
        run("firewall-cmd", &["--reload"]);
        println!("firewalld: allowed TCP {port}.");
    }
    Ok(())
}

pub fn uninstall(port: u16, name: &str) -> Result<()> {
    let _ = std::fs::remove_file(MODPROBE_CONF);
    let _ = std::fs::remove_file(MODULES_LOAD);
    // Only unload the module if it's ours; other programs (OBS) may use it as well.
    if ours_loaded(name) && !run("modprobe", &["-r", "v4l2loopback"]) {
        println!("v4l2loopback is in use; it goes away at the next reboot.");
    }
    if ufw_active() {
        run("ufw", &["delete", "allow", &format!("{port}/tcp")]);
    }
    if firewalld_running() {
        run("firewall-cmd", &["--permanent", &format!("--remove-port={port}/tcp")]);
        run("firewall-cmd", &["--reload"]);
    }
    println!("Removed the virtual camera setup and the firewall rules.");
    Ok(())
}
