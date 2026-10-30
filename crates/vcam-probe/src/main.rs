//! Test helper: opens a camera by friendly name (prefix) through Media Foundation, like
//! the Windows Camera app or new Teams do, reads frames and prints the average colour
//! of the last one as "R G B". Lists all cameras when the name isn't found.
//!
//!     vcam-probe "iPhone Webcam" [frames]
//!
//! Exit codes: 0 ok, 2 camera not found, 3 could not read frames, 1 other errors.

#[cfg(windows)]
fn main() {
    let code = match probe::run() {
        Ok(()) => 0,
        Err((code, message)) => {
            eprintln!("{message}");
            code
        }
    };
    std::process::exit(code);
}

#[cfg(not(windows))]
fn main() {
    eprintln!("vcam-probe only works on Windows");
    std::process::exit(1);
}

#[cfg(windows)]
mod probe {
    use windows::core::PWSTR;
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};

    type Failure = (i32, String);

    const STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

    fn fail<E: std::fmt::Display>(code: i32, what: &str) -> impl FnOnce(E) -> Failure + '_ {
        move |e| (code, format!("{what} failed: {e}"))
    }

    // SAFETY (for the unsafe blocks below unless they say otherwise): Media Foundation
    // calls on live objects we hold, with valid arguments.

    pub fn run() -> Result<(), Failure> {
        let mut args = std::env::args().skip(1);
        let wanted = args.next().ok_or((1, "usage: vcam-probe <camera name> [frames]".to_string()))?;
        let frames: usize = args.next().and_then(|n| n.parse().ok()).unwrap_or(30);
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.map_err(fail(1, "MFStartup"))?;

        let activate = find_camera(&wanted)?;
        let source: IMFMediaSource = unsafe { activate.ActivateObject() }.map_err(fail(3, "ActivateObject"))?;
        let reader = create_rgb32_reader(&source)?;
        let average = read_average(&reader, frames)?;
        println!("{:.1} {:.1} {:.1}", average[0], average[1], average[2]);
        let _ = unsafe { source.Shutdown() };
        // Release every Media Foundation object before shutting it down.
        drop((reader, source, activate));
        let _ = unsafe { MFShutdown() };
        Ok(())
    }

    fn new_attributes() -> Result<IMFAttributes, Failure> {
        let mut attributes = None;
        unsafe { MFCreateAttributes(&mut attributes, 1) }.map_err(fail(1, "MFCreateAttributes"))?;
        attributes.ok_or((1, "MFCreateAttributes returned nothing".into()))
    }

    /// List the video capture devices and return the first whose name starts with `wanted`.
    fn find_camera(wanted: &str) -> Result<IMFActivate, Failure> {
        let query = new_attributes()?;
        let video_capture = &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID;
        unsafe { query.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, video_capture) }.map_err(fail(1, "SetGUID"))?;
        let mut devices: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        unsafe { MFEnumDeviceSources(&query, &mut devices, &mut count) }.map_err(fail(1, "MFEnumDeviceSources"))?;
        println!("Media Foundation cameras: {count}");
        if devices.is_null() {
            return Err((2, format!("camera '{wanted}' not found")));
        }
        // SAFETY: MFEnumDeviceSources returned an array of `count` entries, which we own
        // and free below; taking each entry out leaves nothing to release twice.
        let list = unsafe { std::slice::from_raw_parts_mut(devices, count as usize) };
        let mut found = None;
        for device in list.iter_mut() {
            let Some(activate) = device.take() else { continue };
            let Some(text) = friendly_name(&activate) else { continue };
            println!("  {text}");
            // Windows appends " (Windows Virtual Camera)" to virtual cameras' names.
            if found.is_none() && text.starts_with(wanted) {
                found = Some(activate);
            }
        }
        // SAFETY: the array came from MFEnumDeviceSources and is freed once.
        unsafe { CoTaskMemFree(Some(devices as _)) };
        found.ok_or((2, format!("camera '{wanted}' not found")))
    }

    fn friendly_name(activate: &IMFActivate) -> Option<String> {
        let mut name = PWSTR::null();
        let mut len = 0u32;
        unsafe { activate.GetAllocatedString(&MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, &mut name, &mut len) }.ok()?;
        // SAFETY: a NUL-terminated string allocated for us, freed once after reading.
        unsafe {
            let text = name.to_string().unwrap_or_default();
            CoTaskMemFree(Some(name.0 as _));
            Some(text)
        }
    }

    /// A source reader for the camera's first video stream, delivering RGB32.
    fn create_rgb32_reader(source: &IMFMediaSource) -> Result<IMFSourceReader, Failure> {
        let options = new_attributes()?;
        // Lets us ask for RGB32 whatever the camera's native format.
        unsafe { options.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1) }.map_err(fail(1, "SetUINT32"))?;
        let reader = unsafe { MFCreateSourceReaderFromMediaSource(source, &options) }
            .map_err(fail(3, "MFCreateSourceReaderFromMediaSource"))?;
        let rgb = unsafe { MFCreateMediaType() }.map_err(fail(1, "MFCreateMediaType"))?;
        unsafe { rgb.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video) }.map_err(fail(1, "SetGUID"))?;
        unsafe { rgb.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32) }.map_err(fail(1, "SetGUID"))?;
        unsafe { reader.SetCurrentMediaType(STREAM, None, &rgb) }.map_err(fail(3, "SetCurrentMediaType(RGB32)"))?;
        let actual = unsafe { reader.GetCurrentMediaType(STREAM) }.map_err(fail(3, "GetCurrentMediaType"))?;
        let size = unsafe { actual.GetUINT64(&MF_MT_FRAME_SIZE) }.unwrap_or(0);
        println!("reading {}x{} RGB32", size >> 32, size as u32);
        Ok(reader)
    }

    /// Read `frames` frames and return the average colour (R, G, B) of the last one.
    fn read_average(reader: &IMFSourceReader, frames: usize) -> Result<[f64; 3], Failure> {
        let mut got = 0;
        let mut average = [0f64; 3];
        for _ in 0..frames * 4 {
            if got == frames {
                break;
            }
            let mut flags = 0u32;
            let mut sample = None;
            unsafe { reader.ReadSample(STREAM, 0, None, Some(&mut flags), None, Some(&mut sample)) }
                .map_err(fail(3, "ReadSample"))?;
            let Some(sample) = sample else { continue };
            got += 1;
            if got < frames {
                continue; // average the last frame only: earlier ones may predate the stream
            }
            average = average_colour(&sample)?;
        }
        if got < frames {
            return Err((3, format!("only {got} frames read")));
        }
        Ok(average)
    }

    fn average_colour(sample: &IMFSample) -> Result<[f64; 3], Failure> {
        let buffer = unsafe { sample.ConvertToContiguousBuffer() }.map_err(fail(3, "ConvertToContiguousBuffer"))?;
        let mut data = std::ptr::null_mut();
        let mut len = 0u32;
        unsafe { buffer.Lock(&mut data, None, Some(&mut len)) }.map_err(fail(3, "Lock"))?;
        // SAFETY: Lock gives `len` readable bytes at `data` until Unlock.
        let pixels = unsafe { std::slice::from_raw_parts(data, len as usize) };
        let mut sums = [0f64; 3];
        for px in pixels.chunks_exact(4) {
            sums[0] += px[2] as f64;
            sums[1] += px[1] as f64;
            sums[2] += px[0] as f64;
        }
        let n = (pixels.len() / 4).max(1) as f64;
        let _ = unsafe { buffer.Unlock() };
        Ok(sums.map(|s| s / n))
    }
}
