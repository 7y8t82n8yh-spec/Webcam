//! iPhone Webcam: use an iPhone as a webcam on a Windows 11 PC.

use std::net::IpAddr;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

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
    /// Console only, no window.
    #[arg(long)]
    no_gui: bool,
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Register the virtual camera and open the firewall (asks for administrator rights).
    Install {
        /// Path to iPhoneWebcamVCam.dll (default: next to this program).
        #[arg(long)]
        dll: Option<PathBuf>,
    },
    /// Remove the virtual camera and firewall rules.
    Uninstall,
}

fn default_data_dir() -> PathBuf {
    match std::env::var_os("APPDATA") {
        Some(appdata) => PathBuf::from(appdata).join("iPhoneWebcam"),
        None => std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(".iphone-webcam"),
    }
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
    println!("Close the window or press Ctrl+C to quit.\n");
}

fn main() -> Result<()> {
    let args = Args::parse();
    let filter = if args.verbose { "debug,str0m=info" } else { "info,str0m=warn" };
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| filter.into()))
        .init();
    anyhow::ensure!(args.width % 2 == 0 && args.height % 2 == 0, "width and height must be even");

    match args.command {
        #[cfg(windows)]
        Some(Cmd::Install { dll }) => std::process::exit(iphone_webcam::install::install(args.port, dll)?),
        #[cfg(windows)]
        Some(Cmd::Uninstall) => std::process::exit(iphone_webcam::install::uninstall()?),
        #[cfg(not(windows))]
        Some(_) => anyhow::bail!("install/uninstall are for Windows 11"),
        None => {}
    }

    let settings = Settings {
        host: args.host,
        port: args.port,
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
    if !args.no_gui {
        return iphone_webcam::gui::run(app);
    }

    let (tx, rx) = std::sync::mpsc::channel();
    ctrlc_channel(tx);
    let _ = rx.recv();
    app.stop();
    Ok(())
}

/// Ctrl+C (and console close) -> message on `tx`.
fn ctrlc_channel(tx: std::sync::mpsc::Sender<()>) {
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let _ = tokio::signal::ctrl_c().await;
        });
        let _ = tx.send(());
    });
}
