//! Shared pieces of the image and volume tests.
#![allow(dead_code)]

pub mod compare;
pub mod live;
pub mod testkit;
pub mod watchdog;
pub mod wofrec;

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(windows)]
pub mod truth;
#[cfg(windows)]
pub mod windows;
