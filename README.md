# iPhone Webcam

Use your iPhone as a webcam on a Windows PC. Nothing is installed on the phone: it
opens a web page served by the PC, and the camera stream shows up on the PC as a
camera called **iPhone Webcam** that Zoom, Teams, Discord, OBS, browsers and other
apps can use.

```
 iPhone (Safari)                                  Windows PC
┌──────────────────┐  WebRTC video (H.264, UDP)  ┌───────────────────────────┐
│ camera           │ ──────────────────────────▶ │ iphone_webcam (Python)    │
│ screen kept awake│  signalling over HTTPS      │  decode ─▶ virtual camera │ ─▶ Zoom / Teams / …
└──────────────────┘                             │  "iPhone Webcam" (DShow)  │
                                                 └───────────────────────────┘
```

The phone sends its camera as a WebRTC video stream: H.264 encoded in hardware on the
iPhone, with congestion control that adapts to the Wi-Fi. If WebRTC can't connect,
usually because a firewall blocks UDP, the page automatically falls back to sending
JPEG frames over a WebSocket, and to HTTPS uploads if WebSockets are refused too. The
phone's status bar shows which one is in use.

## Setup (once)

1. **Install the virtual camera.** Double-click `install_driver.bat` and accept the
   administrator prompt.
   - **Windows 11:** it registers this project's own camera (`iPhoneWebcamVCam.dll`,
     built from `vcam/`). The camera uses Windows' built-in virtual camera support
     (`MFCreateVirtualCamera`), so there's no third-party or kernel driver. It exists
     while the app runs, and every app can use it, including the Windows Camera app and
     new Teams. Windows lists it as *iPhone Webcam (Windows Virtual Camera)*.
   - **Windows 10** (or `-Legacy`): it registers a DirectShow camera named *iPhone
     Webcam* using the open-source [Unity Capture](https://github.com/schellingb/UnityCapture)
     filter (MIT licensed; downloaded at a pinned commit and hash-checked).

   It also opens TCP port 8443 in Windows Firewall, plus UDP for `iPhoneWebcam.exe` (WebRTC
   video). `uninstall_driver.bat` removes everything again. The DLL ships next to
   `install_driver.bat` in the GitHub Actions build. In a source checkout, build it first
   with `scripts\build_vcam.ps1` (needs Visual Studio 2022 Build Tools with C++ and CMake).
2. **Start the app.** Either
   - double-click `run.bat`. It uses [uv](https://docs.astral.sh/uv/), installing it if
     needed, and uv fetches Python and the dependencies by itself on the first run, or
   - run `dist\iPhoneWebcam.exe` (from the GitHub Actions build artifact, or build it
     yourself with `scripts\build_exe.ps1`).

## Daily use

1. Start the app. A window shows a QR code, the address, and a live preview.
2. With the iPhone on the **same Wi-Fi**, scan the QR code with the Camera app. It
   opens Safari. The link contains a pairing key, so only phones that scanned the code
   can send video to the PC. The key is kept between runs, so a bookmark on
   the phone keeps working.
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
`%APPDATA%\iPhoneWebcam`.

## Options

```
iPhoneWebcam.exe [--port 8443] [--width 1280 --height 720] [--fps 30]
                 [--bitrate 6] [--quality 0.75] [--mode fit|fill]
                 [--backend unitycapture|obs|none]
                 [--device NAME] [--host 0.0.0.0] [--data-dir DIR] [--no-gui] [-v]
```

* `--bitrate` caps the WebRTC video in Mbit/s. `--quality` is the JPEG quality used
  only by the fallback transport.
* `--mode fit` (default) letterboxes a portrait phone picture into the 16:9 camera.
  `--mode fill` crops it instead.
* `--backend obs` uses OBS Studio's virtual camera instead of the bundled driver. By
  default the app tries *iPhone Webcam* first and falls back to any available virtual
  camera.
* If you change `--port`, also run `scripts\install_driver.ps1 -Port <n>` so the
  firewall rule matches.

## Limitations

* **Keep Safari in the foreground.** iOS stops camera access for web pages when they're
  in the background or the phone is locked. Leave the page open and use *Dim*. The
  stream resumes automatically when you come back to the page.
* On Windows 10 the camera is a DirectShow device. Desktop apps (Zoom, Teams, Discord,
  OBS, Skype) and Chrome/Edge/Firefox can use it, but apps that only use Media
  Foundation, such as the Windows *Camera* app, won't list it. The Windows 11 camera
  doesn't have this gap.
* The Windows 11 camera supports 640×360, 1280×720 and 1920×1080 at 30 fps, on x64 PCs.
* WebRTC is capped at 6 Mbit/s (`--bitrate`). It starts around 2.5 Mbit/s and adjusts
  to the network from there. The JPEG fallback uses more bandwidth for the same
  quality. There's no audio, so use your PC's microphone.

## Troubleshooting

* **The phone can't open the page (it times out).**
  - Check that both devices are on the same network. Guest Wi-Fi often isolates devices
    from each other.
  - Check that `install_driver.bat` ran, since it adds the firewall rule.
  - If you once clicked *Cancel* on the Windows Firewall prompt for Python or
    `iPhoneWebcam.exe`, Windows created a *block* rule, and block rules win over allow
    rules. Remove it under *Windows Defender Firewall → Allow an app through firewall*.
* **"Pairing key rejected".** You opened an old link or typed the address by hand. Scan
  the QR code again. Deleting `%APPDATA%\iPhoneWebcam\pairing-key.txt` makes a new key,
  which invalidates old links.
* **The phone shows "Streaming" instead of "Streaming (WebRTC)".** WebRTC couldn't
  connect and the JPEG fallback is in use. WebRTC needs UDP between the devices. Allow
  Python or `iPhoneWebcam.exe` through Windows Firewall (the prompt on first start), and
  avoid networks that block device-to-device traffic.
* **Picture freezes after switching apps on the phone.** iOS pauses the camera in the
  background. Switch back to Safari; the stream resumes on its own.
* **Video app shows the "Waiting for iPhone..." image.** The camera works and no phone
  is streaming. Tap *Start camera* on the phone.
* **The window says "Virtual camera unavailable".** Run `install_driver.bat`, or
  start the app with `--backend obs` if OBS Studio is installed.
* **"MFCreateVirtualCamera failed: HRESULT 0x80070005".** Windows' camera privacy
  settings block it. Turn on Settings → Privacy & security → Camera → *Camera access*
  and *Let desktop apps access your camera*.
* **Diagnosing the Windows 11 camera.** Create the DWORD
  `HKLM\SOFTWARE\iPhoneWebcam\VCamLog` = 1 and the string `VCamLogDir` pointing to a
  folder everyone can write to. Every process using the camera then writes a log there,
  including Windows' camera service.

## Development

The project is managed with [uv](https://docs.astral.sh/uv/):

```
uv sync                                   # create .venv with runtime + dev dependencies
uv run playwright install chromium        # browser for the end-to-end tests
uv run pytest                             # unit tests + headless-Chromium end-to-end tests
uv run iphone-webcam --backend none --no-gui   # run without a virtual camera
```

`.github/workflows/windows.yml` runs two jobs on Windows:
- **Main job:** runs the tests, installs Unity Capture, streams a solid colour over
  WebRTC and then the JPEG fallback (`tests/windows_smoke.py`), and checks the camera
  shows that colour through DirectShow. It also builds `iPhoneWebcam.exe` and the
  Windows 11 camera DLL as a downloadable artifact.
- **Windows 11 camera job:** builds and installs the camera and does the same check
  through Media Foundation, using `vcam_probe.exe`.

Code layout:

| Path | What it does |
| --- | --- |
| `iphone_webcam/static/` | The phone page: camera capture, wake lock, WebRTC sender, JPEG fallbacks |
| `iphone_webcam/server.py` | aiohttp HTTPS server: page, `/offer` WebRTC signalling, `/ws` and `/frame` fallbacks, `/ca.crt` |
| `iphone_webcam/webrtc.py` | aiortc receiver: one peer connection per phone, start-bitrate hint |
| `iphone_webcam/camera.py` | Frame conversion, letterboxing, and the fixed-rate virtual camera output thread |
| `iphone_webcam/certs.py` | Local CA and per-IP server certificate generation |
| `iphone_webcam/gui.py` | Tk status window with QR code and preview |
| `iphone_webcam/netutil.py` | Picks the LAN address shown in the QR code |
| `iphone_webcam/app.py` | Command line, pairing key, startup and shutdown |
| `iphone_webcam/mfvcam.py` | Creates the Windows 11 camera and hands it frames (ctypes) |
| `vcam/src/vcam.cpp` | Windows 11 camera: Media Foundation media source run by the Frame Server service |
| `vcam/probe/probe.cpp` | Test tool that reads a camera through Media Foundation |
| `scripts/install_driver.ps1` | Registers or removes the virtual camera and firewall rules |
