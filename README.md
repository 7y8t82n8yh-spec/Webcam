# iPhone Webcam

Use your iPhone as a webcam on a Windows 11 or Linux PC. Nothing is installed on the
phone: it opens a web page served by the PC, and the camera stream shows up on the PC
as a camera called **iPhone Webcam** that Zoom, Teams, Discord, OBS, browsers, the
Windows Camera app and other apps can use.

```
 iPhone (Safari)                                  Windows 11 PC
┌──────────────────┐  WebRTC video (H.264, UDP)  ┌──────────────────────────────┐
│ page (Rust→wasm) │ ──────────────────────────▶ │ iphone-webcam.exe            │
│ camera           │  signalling over HTTPS      │  OpenH264 decoder            │
│ screen kept awake│                             │  ─▶ shared memory            │
└──────────────────┘                             └──────────────┬───────────────┘
                                                                ▼
                                   Windows camera service loads iPhoneWebcamVCam.dll
                                   ─▶ "iPhone Webcam" ─▶ Zoom / Teams / …
```

The phone sends its camera as a WebRTC video stream: H.264 encoded in hardware on the
iPhone, with congestion control that adapts to the Wi-Fi. The PC decodes it with
Cisco's OpenH264, built into the app. On Linux the camera is a
[v4l2loopback](https://github.com/umlaeute/v4l2loopback) device instead of the
Windows 11 camera shown above. If WebRTC can't connect, usually because a firewall
blocks UDP, the page falls back to sending JPEG frames over a WebSocket, and to HTTPS uploads
if WebSockets are refused too. The phone's status bar shows which one is in use.

Everything is written in Rust: the PC app, the camera DLL, and the phone page, which is
compiled to WebAssembly.

## Setup on Windows 11 (once)

1. Download the `iPhoneWebcam-windows11` artifact from the latest GitHub Actions run
   (or build it, see *Development*), and keep `iphone-webcam.exe` and
   `iPhoneWebcamVCam.dll` in the same folder.
2. Run `iphone-webcam.exe install` and accept the administrator prompt. It
   - copies the camera DLL to `C:\Program Files\iPhoneWebcam\vcam` and registers it.
     The camera uses Windows 11's built-in virtual camera support
     (`MFCreateVirtualCamera`), so there's no third-party or kernel driver;
   - opens TCP port 8443 in Windows Firewall, plus UDP for `iphone-webcam.exe` (WebRTC
     video).

   `iphone-webcam.exe uninstall` removes all of it again.

## Setup on Linux (once)

1. Install the v4l2loopback kernel module from your distribution:
   `sudo apt install v4l2loopback-dkms` (Debian/Ubuntu),
   `sudo dnf install v4l2loopback` (Fedora, from RPM Fusion) or
   `sudo pacman -S v4l2loopback-dkms` (Arch).
2. Download the `iPhoneWebcam-linux-x64` artifact from the latest GitHub Actions run
   (or build it, see *Development*) and run `iphone-webcam install`. It asks for your
   password through sudo, then
   - configures v4l2loopback to load at boot with one device named *iPhone Webcam*
     (`/etc/modprobe.d/iphone-webcam.conf`, `/etc/modules-load.d/iphone-webcam.conf`)
     and loads it now. If another program, such as OBS, already uses v4l2loopback, the
     app uses that device until the next reboot;
   - opens TCP port 8443 in ufw or firewalld, if one of them is active.

   `iphone-webcam uninstall` removes all of it again. With Secure Boot on, the
   distribution's DKMS setup has to sign the module, or it won't load.

## Daily use

1. Start `iphone-webcam` (`iphone-webcam.exe` on Windows). A window shows a QR code,
   the address, and a live preview. On Windows the camera exists while the app runs.
2. With the iPhone on the **same Wi-Fi**, scan the QR code with the Camera app. It
   opens Safari. The link contains a pairing key, so only phones that scanned the code
   can send video to the PC. The key is kept between runs, so a bookmark on the phone
   keeps working.
3. The first time, Safari warns that the connection isn't private, because the PC uses
   its own certificate. Tap **Show Details → visit this website → Visit Website**.
4. Tap **Start camera** and allow camera access.
5. Pick **iPhone Webcam** as the camera in your video app.

The phone page has buttons for switching the front and back cameras, choosing a
resolution, mirroring the preview, and **Dim**, which blacks out the screen to save
battery while it keeps streaming. The screen is kept awake with the Screen Wake Lock
API. Older iOS versions fall back to a silent looping video, the same approach
NoSleep.js uses.

**Stop the certificate warning for good (optional):** on the phone page tap *Install
the local certificate*. Then go to Settings → General → VPN & Device Management and
install the profile, and enable it under Settings → General → About → Certificate
Trust Settings. The certificate is generated on your PC and stored in
`%APPDATA%\iPhoneWebcam` (`~/.config/iphone-webcam` on Linux). It can only vouch for local network addresses.

## Options

```
iphone-webcam.exe [--port 8443] [--width 1280 --height 720] [--fps 30]
                  [--bitrate 6] [--quality 0.75] [--mode fit|fill]
                  [--camera-name NAME] [--host 0.0.0.0] [--data-dir DIR]
                  [--no-camera] [--no-gui] [-v]
                  [install [--dll PATH] | uninstall]
```

* `--bitrate` caps the WebRTC video in Mbit/s. `--quality` is the JPEG quality used
  only by the fallback transports.
* `--mode fit` (default) letterboxes a portrait phone picture into the 16:9 camera.
  `--mode fill` crops it instead.
* If you change `--port`, run `iphone-webcam.exe --port <n> install` again so the
  firewall rule matches.

## Limitations

* **Windows 11** (build 22000 or later, x64) or **Linux** with v4l2loopback. Windows 10
  and macOS aren't supported.
* **Keep Safari in the foreground.** iOS stops camera access for web pages when they're
  in the background or the phone is locked. Leave the page open and use *Dim*. The
  stream resumes automatically when you come back to the page.
* The Windows camera offers 640×360, 1280×720 and 1920×1080 at 30 fps. On Linux the
  camera has the app's `--width`/`--height` (1280×720 by default) in YUYV.
* WebRTC is capped at 6 Mbit/s (`--bitrate`). It starts around 2.5 Mbit/s and adjusts
  to the network from there. The JPEG fallback uses more bandwidth for the same
  quality. There's no audio, so use your PC's microphone.

## Troubleshooting

* **The phone can't open the page (it times out).**
  - Check that both devices are on the same network. Guest Wi-Fi often isolates devices
    from each other.
  - Check that `iphone-webcam.exe install` ran, since it adds the firewall rules.
  - If you once clicked *Cancel* on a Windows Firewall prompt for `iphone-webcam.exe`,
    Windows created a *block* rule, and block rules win over allow rules. Remove it
    under *Windows Defender Firewall → Allow an app through firewall*.
* **Linux: the window says no v4l2loopback device was found.** Run `iphone-webcam
  install`, and check `sudo dmesg` if the module doesn't load.
* **"Pairing key rejected".** You opened an old link or typed the address by hand. Scan
  the QR code again. Deleting `%APPDATA%\iPhoneWebcam\pairing-key.txt` makes a new key,
  which invalidates old links.
* **The phone shows "Streaming" instead of "Streaming (WebRTC)".** WebRTC couldn't
  connect and the JPEG fallback is in use. WebRTC needs UDP between the devices: run
  the install again, and avoid networks that block device-to-device traffic.
* **Picture freezes after switching apps on the phone.** iOS pauses the camera in the
  background. Switch back to Safari; the stream resumes on its own.
* **Video app shows the "Waiting for iPhone…" image.** The camera works and no phone
  is streaming. Tap *Start camera* on the phone.
* **The window says the virtual camera is unavailable.** Run `iphone-webcam.exe install`.
  If it mentions access being denied, Windows' camera privacy settings block it: turn
  on Settings → Privacy & security → Camera → *Camera access* and *Let desktop apps
  access your camera*.
* **Diagnosing the camera.** Create the DWORD `HKLM\SOFTWARE\iPhoneWebcam\VCamLog` = 1
  and the string `VCamLogDir` pointing to a folder everyone can write to. Every process
  using the camera, including Windows' camera service, then writes a log there.
  `vcam-probe.exe "iPhone Webcam"` reads frames from the camera the way video apps do
  and prints their average colour.

## Development

Needs Rust (stable), the `wasm32-unknown-unknown` target and `wasm-bindgen-cli` at the
version in `Cargo.lock`:

```
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.129 --locked
cargo xtask web                          # build the phone page into crates/app/web-dist
cargo build --release                    # iphone-webcam.exe, iPhoneWebcamVCam.dll, vcam-probe.exe
cargo test --workspace
```

On Linux, `cargo build --release -p iphone-webcam` builds the app; the camera DLL and
the probe are Windows-only. The tests include a fake phone that streams H.264 over
WebRTC, and the real phone page in headless Chromium with a fake camera (set `CHROMIUM_PATH` if it isn't found).
`cargo run -p iphone-webcam -- --no-camera` runs the server without a camera.

CI runs everything on Windows 11 (`.github/workflows/windows.yml`) and Ubuntu
(`.github/workflows/linux.yml`). Both also install the camera, stream a solid colour
from the fake phone, and check that an ordinary client sees that colour: `vcam-probe.exe`
through Media Foundation, or ffmpeg from the v4l2loopback device. They upload the built
programs as artifacts.

| Path | What it does |
| --- | --- |
| `crates/web` | The phone page in Rust/WebAssembly: camera, wake lock, WebRTC sender, JPEG fallbacks |
| `crates/app/src/server.rs` | HTTPS server: page, `/offer` WebRTC signalling, `/ws` and `/frame` fallbacks, `/ca.crt` |
| `crates/app/src/webrtc.rs` | WebRTC receiver ([str0m](https://github.com/algesten/str0m)) and decoder thread |
| `crates/app/src/decode.rs` | H.264 decoding (OpenH264) |
| `crates/app/src/hub.rs` | Latest frame, colour conversion, letterboxing, the "waiting" image |
| `crates/app/src/vcam_client.rs` | Creates the Windows 11 camera and hands it frames through shared memory |
| `crates/app/src/v4l2.rs` | Linux camera: writes frames to a v4l2loopback device |
| `crates/app/src/certs.rs` | Local CA (restricted to private addresses) and server certificates |
| `crates/app/src/gui.rs` | Status window with QR code and preview ([egui](https://github.com/emilk/egui)) |
| `crates/app/src/install.rs`, `install_linux.rs` | `install` / `uninstall`: camera setup and firewall rules |
| `crates/vcam` | The camera DLL: a Media Foundation media source run by Windows' camera service |
| `crates/vcam-probe` | Test tool that reads a camera through Media Foundation |
| `crates/protocol` | Messages and the shared-memory layout used by more than one crate |
| `xtask` | `cargo xtask web` builds the phone page |
