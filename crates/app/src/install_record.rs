//! What `install` set up, saved where it can't be lost with the program: in the registry
//! on Windows, in /etc on Linux. `uninstall` and `reinstall` read it to undo exactly
//! that, whichever version installed it.
//!
//! Later versions may add fields, which older ones ignore. Versions from before the
//! record existed left none: then the settings given on the command line (or the
//! defaults) are used instead.

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallRecord {
    /// Version of the app that installed.
    pub version: String,
    /// The HTTPS port opened in the firewall.
    pub port: u16,
    /// Name of the camera (Linux names the loopback device after it).
    pub camera_name: String,
}

impl InstallRecord {
    /// A record of this version installing with `port` and `camera_name`.
    pub fn new(port: u16, camera_name: &str) -> Self {
        Self { version: env!("CARGO_PKG_VERSION").to_string(), port, camera_name: camera_name.to_string() }
    }
}

fn parse(json: &str) -> Option<InstallRecord> {
    serde_json::from_str(json).ok()
}

#[cfg(windows)]
mod store {
    use anyhow::Result;
    use windows_registry::LOCAL_MACHINE;

    const KEY: &str = r"SOFTWARE\iPhoneWebcam";
    const VALUE: &str = "Installed";

    pub fn read() -> Option<String> {
        LOCAL_MACHINE.open(KEY).ok()?.get_string(VALUE).ok()
    }

    pub fn write(json: &str) -> Result<()> {
        Ok(LOCAL_MACHINE.create(KEY)?.set_string(VALUE, json)?)
    }

    pub fn remove() {
        // `open` alone gives read access only.
        if let Ok(key) = LOCAL_MACHINE.options().write().open(KEY) {
            let _ = key.remove_value(VALUE);
        }
    }
}

#[cfg(target_os = "linux")]
mod store {
    use anyhow::{Context, Result};

    const DIR: &str = "/etc/iphone-webcam";
    const FILE: &str = "/etc/iphone-webcam/installed.json";

    pub fn read() -> Option<String> {
        std::fs::read_to_string(FILE).ok()
    }

    pub fn write(json: &str) -> Result<()> {
        std::fs::create_dir_all(DIR).context(DIR)?;
        std::fs::write(FILE, json).context(FILE)
    }

    pub fn remove() {
        let _ = std::fs::remove_dir_all(DIR);
    }
}

/// The record of the current installation, if there is one.
pub fn load() -> Option<InstallRecord> {
    parse(&store::read()?)
}

pub fn save(record: &InstallRecord) -> Result<()> {
    store::write(&serde_json::to_string_pretty(record)?)
}

pub fn remove() {
    store::remove();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_from_other_versions_read() {
        let record = InstallRecord::new(8443, "iPhone Webcam");
        assert_eq!(parse(&serde_json::to_string(&record).unwrap()), Some(record.clone()));
        // A later version's record, with a field this one doesn't know.
        let newer = r#"{"version":"9.0.0","port":9000,"cameraName":"Cam","dllDir":"C:\\x"}"#;
        let newer = parse(newer).unwrap();
        assert_eq!((newer.version.as_str(), newer.port, newer.camera_name.as_str()), ("9.0.0", 9000, "Cam"));
        assert_eq!(parse("not json"), None);
    }
}
