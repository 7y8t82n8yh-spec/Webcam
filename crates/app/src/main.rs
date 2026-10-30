//! iPhone Webcam: use an iPhone as a webcam on a Windows 11 or Linux PC.

use std::net::IpAddr;
use std::path::PathBuf;

use anyhow::Result;
use clap::parser::ValueSource;
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};

use iphone_webcam::app::{App, Settings};
use iphone_webcam::hub::FitMode;

#[derive(Parser)]
#[command(name = "iphone-webcam", version, about = "Use an iPhone as a webcam for this PC")]
struct Args {
    #[command(subcommand)]
    command: Option<Cmd>,
    /// HTTPS port the phone connects to.
    #[arg(long, default_value_t = 8443)]
    port: u16,
    /// Address to listen on.
    #[arg(long, default_value = "0.0.0.0")]
    host: IpAddr,
    /// Port of the admin page, on 127.0.0.1 (only this PC can open it).
    #[arg(long, default_value_t = 8444)]
    admin_port: u16,
    /// Virtual camera width.
    #[arg(long, default_value_t = 1280)]
    width: u32,
    /// Virtual camera height.
    #[arg(long, default_value_t = 720)]
    height: u32,
    /// Virtual camera frame rate.
    #[arg(long, default_value_t = 30)]
    fps: u32,
    /// Maximum WebRTC video bitrate in Mbit/s.
    #[arg(long, default_value_t = 6.0)]
    bitrate: f64,
    /// JPEG quality for the fallback transport, 0.1-1.0.
    #[arg(long, default_value_t = 0.75)]
    quality: f64,
    /// fit: letterbox the phone picture; fill: crop it to fill the frame.
    #[arg(long, value_enum, default_value_t = FitMode::Fit)]
    mode: FitMode,
    /// Name of the virtual camera.
    #[arg(long, default_value = "iPhone Webcam")]
    camera_name: String,
    /// Don't create a virtual camera (testing).
    #[arg(long)]
    no_camera: bool,
    /// Where certificates and the pairing key are stored.
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Don't open the admin page in the browser at startup.
    #[arg(long, alias = "no-gui")]
    no_browser: bool,
    #[arg(short, long)]
    verbose: bool,
    /// Set on the copy that install/uninstall start with administrator rights: its
    /// console window closes on exit, so it waits for Enter after an error.
    #[arg(long, hide = true, global = true)]
    elevated: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Set up the virtual camera and open the firewall, replacing any earlier
    /// installation (asks for administrator rights).
    Install {
        /// Windows: path to iPhoneWebcamVCam.dll (default: next to this program).
        #[arg(long)]
        dll: Option<PathBuf>,
    },
    /// Remove what the last install set up, whichever version it was.
    Uninstall,
    /// Uninstall, then install this version, keeping the port and camera name of the
    /// last install unless given here. For updating to a new version.
    Reinstall {
        /// Windows: path to iPhoneWebcamVCam.dll (default: next to this program).
        #[arg(long)]
        dll: Option<PathBuf>,
    },
}

impl Cmd {
    fn name(&self) -> &'static str {
        match self {
            Cmd::Install { .. } => "install",
            Cmd::Uninstall => "uninstall",
            Cmd::Reinstall { .. } => "reinstall",
        }
    }

    fn dll(&self) -> Option<&PathBuf> {
        match self {
            Cmd::Install { dll } | Cmd::Reinstall { dll } => dll.as_ref(),
            Cmd::Uninstall => None,
        }
    }
}

/// The settings given on the command line, as opposed to defaults.
struct Given {
    port: Option<u16>,
    camera_name: Option<String>,
}

fn default_data_dir() -> PathBuf {
    if let Some(appdata) = std::env::var_os("APPDATA") {
        return PathBuf::from(appdata).join("iPhoneWebcam");
    }
    let config = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => {
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
            home.join(".config")
        }
    };
    config.join("iphone-webcam")
}

fn print_banner(app: &App) {
    println!("\niPhone Webcam {}", env!("CARGO_PKG_VERSION"));
    println!("Virtual camera: {}", app.camera_status.lock().unwrap());
    println!("\nOn your iPhone (same Wi-Fi), open in Safari:");
    for url in &app.urls {
        println!("    {url}");
    }
    if let Ok(code) = qrcode::QrCode::new(app.urls[0].as_bytes()) {
        let art = code
            .render::<qrcode::render::unicode::Dense1x2>()
            .dark_color(qrcode::render::unicode::Dense1x2::Light)
            .light_color(qrcode::render::unicode::Dense1x2::Dark)
            .build();
        println!("\n{art}");
    }
    println!("Safari warns about the certificate the first time: tap 'Show Details' > 'visit this website'.");
    println!("\nAdmin page (QR code, preview, rotation): {}", app.admin_url);
    println!("Close this window or press Ctrl+C to quit.\n");
}

fn main() -> Result<()> {
    let matches = Args::command().get_matches();
    let mut args = Args::from_arg_matches(&matches)?;
    let typed = |id| matches.value_source(id) == Some(ValueSource::CommandLine);
    let given = Given {
        port: typed("port").then_some(args.port),
        camera_name: typed("camera_name").then(|| args.camera_name.clone()),
    };
    let filter = if args.verbose { "debug,str0m=info" } else { "info,str0m=warn" };
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| filter.into());
    tracing_subscriber::fmt().with_env_filter(env_filter).init();
    anyhow::ensure!(args.width % 2 == 0 && args.height % 2 == 0, "width and height must be even");

    if let Some(command) = args.command.take() {
        let result = run_install_command(command, &args, &given);
        if let (true, Err(err)) = (args.elevated, &result) {
            // The elevated console window closes on exit; keep the error readable.
            eprintln!("Error: {err:#}\n\nPress Enter to close this window.");
            let _ = std::io::stdin().read_line(&mut String::new());
        }
        std::process::exit(result?);
    }

    let settings = Settings {
        host: args.host,
        port: args.port,
        admin_port: args.admin_port,
        width: args.width,
        height: args.height,
        fps: args.fps,
        quality: args.quality,
        bitrate_mbps: args.bitrate,
        mode: args.mode,
        camera: !args.no_camera,
        camera_name: args.camera_name,
        data_dir: args.data_dir.unwrap_or_else(default_data_dir),
    };
    let mut app = App::start(settings)?;
    print_banner(&app);
    if !args.no_browser {
        open_in_browser(&app.admin_url);
    }

    let (tx, rx) = std::sync::mpsc::channel();
    ctrlc_channel(tx);
    let _ = rx.recv();
    app.stop();
    Ok(())
}

fn open_in_browser(url: &str) {
    let mut command = if cfg!(windows) {
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "start", ""]);
        command
    } else {
        std::process::Command::new("xdg-open")
    };
    if let Err(err) = command.arg(url).spawn() {
        tracing::warn!("could not open the browser ({err}); open {url} yourself");
    }
}

/// `install`, `uninstall` or `reinstall`; returns the exit code.
#[cfg(any(windows, target_os = "linux"))]
fn run_install_command(command: Cmd, args: &Args, given: &Given) -> Result<i32> {
    use iphone_webcam::install;
    use iphone_webcam::install_record::{self, InstallRecord};

    if !install::is_admin() {
        return install::run_as_admin(&admin_args(&command, given)?);
    }
    let installed = install_record::load();
    if let Some(record) = &installed {
        println!("Installed: version {} (port {}, camera \"{}\").", record.version, record.port, record.camera_name);
        if record.version != env!("CARGO_PKG_VERSION") {
            println!("This is version {}.", env!("CARGO_PKG_VERSION"));
        }
    }
    // Remove what was installed: going by the record, else (installed by a version
    // from before records, or by none) by the settings given. A plain install without
    // a record just installs over whatever is there, as it always did.
    if installed.is_some() || !matches!(command, Cmd::Install { .. }) {
        let previous = installed.clone().unwrap_or_else(|| InstallRecord::new(args.port, &args.camera_name));
        install::uninstall(previous.port, &previous.camera_name)?;
        install_record::remove();
    }

    let (port, camera_name) = match (&command, &installed) {
        (Cmd::Uninstall, _) => return Ok(0),
        // Keep the last install's settings, unless new ones are given.
        (Cmd::Reinstall { .. }, Some(record)) => {
            (given.port.unwrap_or(record.port), given.camera_name.clone().unwrap_or_else(|| record.camera_name.clone()))
        }
        _ => (args.port, args.camera_name.clone()),
    };
    install::install(port, &camera_name, command.dll().cloned())?;
    install_record::save(&InstallRecord::new(port, &camera_name))?;
    println!("\nDone. Start iphone-webcam; the camera \"{camera_name}\" appears in video apps while it runs.");
    Ok(0)
}

/// The arguments for running `command` again with administrator rights. That copy may
/// start in another folder (System32 on Windows), so the DLL path is made absolute.
#[cfg(any(windows, target_os = "linux"))]
fn admin_args(command: &Cmd, given: &Given) -> Result<Vec<String>> {
    let mut args = vec!["--elevated".to_string()];
    if let Some(port) = given.port {
        args.extend(["--port".into(), port.to_string()]);
    }
    if let Some(name) = &given.camera_name {
        args.extend(["--camera-name".into(), name.clone()]);
    }
    args.push(command.name().into());
    if let Some(dll) = command.dll() {
        args.extend(["--dll".into(), std::path::absolute(dll)?.display().to_string()]);
    }
    Ok(args)
}

#[cfg(not(any(windows, target_os = "linux")))]
fn run_install_command(_: Cmd, _: &Args, _: &Given) -> Result<i32> {
    anyhow::bail!("install/uninstall/reinstall are for Windows 11 and Linux")
}

/// Ctrl+C (and console close) -> message on `tx`.
fn ctrlc_channel(tx: std::sync::mpsc::Sender<()>) {
    std::thread::spawn(move || wait_for_ctrlc(tx));
}

fn wait_for_ctrlc(tx: std::sync::mpsc::Sender<()>) {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let _ = runtime.block_on(tokio::signal::ctrl_c());
    let _ = tx.send(());
}
