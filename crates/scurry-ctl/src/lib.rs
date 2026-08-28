//! The scurry controller.
//!
//! Deliberately thin. The layout lives on the dongle, so this captures local
//! input, forwards raw motion, and follows the dongle's focus announcements to
//! decide when to stop feeding its own machine. All configuration is stored on
//! the dongle; nothing persists here.

pub mod capture;
pub mod config;
pub mod transport;
