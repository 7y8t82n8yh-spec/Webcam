//! iPhone Webcam's Windows 11 virtual camera.
//!
//! This DLL is a COM server providing a Media Foundation media source. The app creates
//! a camera with `MFCreateVirtualCamera` pointing at our CLSID; the Windows Camera
//! Frame Server service then loads this DLL and asks it for frames, which come from
//! the shared section the app writes (see `protocol::frame_section`).

#![allow(non_snake_case)] // COM method names

pub mod convert;

#[cfg(windows)]
mod imp;
