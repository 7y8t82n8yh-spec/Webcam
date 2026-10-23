//! Reads frames from the shared section (see `protocol::frame_section`).

use std::time::{Duration, Instant};

use protocol::frame_section::{self, Read};

use super::vlog;

/// After this long without a new frame the app is gone: show black, not a frozen frame.
const STALE_AFTER: Duration = Duration::from_secs(3);

pub struct FrameReader {
    section: Option<frame_section::FrameReader>,
    last_attempt: Option<Instant>,
    /// The last complete frame, kept for when the writer is busy every time we look.
    rgb: Vec<u8>,
    scratch: Vec<u8>,
    size: (u32, u32),
    sequence: u64,
    last_change: Instant,
}

impl FrameReader {
    pub fn new() -> Self {
        Self {
            section: None,
            last_attempt: None,
            rgb: Vec::new(),
            scratch: Vec::new(),
            size: (0, 0),
            sequence: 0,
            last_change: Instant::now(),
        }
    }

    /// The latest frame as (rgb, width, height), or `None` if there is none yet or the
    /// app stopped writing.
    pub fn read(&mut self) -> Option<(&[u8], u32, u32)> {
        // Throttled: this runs for every frame while the app isn't writing yet.
        if self.section.is_none() && self.last_attempt.is_none_or(|t| t.elapsed() > Duration::from_secs(1)) {
            self.last_attempt = Some(Instant::now());
            match frame_section::FrameReader::create_or_open() {
                Ok(section) => {
                    vlog!("frames: section mapped");
                    self.section = Some(section);
                }
                Err(e) => vlog!("frames: no section yet ({e})"),
            }
        }
        let section = self.section.as_ref()?;
        for _ in 0..4 {
            match section.read_into(&mut self.scratch) {
                Read::Empty => return None,
                Read::Busy => std::thread::sleep(Duration::from_millis(1)),
                Read::Frame { width, height, sequence } => {
                    if sequence != self.sequence {
                        std::mem::swap(&mut self.rgb, &mut self.scratch);
                        self.size = (width, height);
                        self.sequence = sequence;
                        self.last_change = Instant::now();
                    }
                    break;
                }
            }
        }
        if self.rgb.is_empty() || self.last_change.elapsed() > STALE_AFTER {
            return None;
        }
        Some((&self.rgb, self.size.0, self.size.1))
    }
}
