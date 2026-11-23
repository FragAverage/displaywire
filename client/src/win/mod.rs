//! The Windows client. The stream itself (network, reassembly, audio, input, transfers) is the
//! code the Mac client runs; this is what differs: video output (`video`), the stream window and
//! its menu (`app`, `menu`), gamepads (`gamepad`), the clipboard, and the list of PCs with
//! pairing and settings (`launcher`, an egui window in a process of its own, which starts one
//! `displaywire-client <address>` per connection).

pub mod app;
pub mod clipboard;
pub mod dialog;
pub mod gamepad;
pub mod hook;
pub mod launcher;
pub mod menu;
pub mod video;
