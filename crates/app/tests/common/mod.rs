//! Helpers shared by the integration tests, one module per concept:
//!
//! - [`test_app`]: the app running on localhost, and talking to it over HTTPS.
//! - [`fake_phone`]: a phone that streams H.264 over WebRTC, like the phone page does.
//! - [`images`]: solid-colour test pictures and colour comparisons.

// Each test binary uses only some of these helpers.
#![allow(dead_code)]

pub mod fake_phone;
pub mod images;
pub mod test_app;
