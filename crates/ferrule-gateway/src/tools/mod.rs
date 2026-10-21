//! Tools the gateway gives a chat's agent (M39: `send_file`; M51:
//! `generate_image` / `generate_video` and the video poller).

pub mod media;
pub mod send_file;

pub use media::{GenerateImageTool, GenerateVideoTool, MediaPoller, MediaShared};
pub use send_file::{FileOut, SendFileTool};
