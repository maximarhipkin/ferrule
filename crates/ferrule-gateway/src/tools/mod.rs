//! Tools the gateway gives a chat's agent (M39: `send_file`; M51:
//! `generate_image` / `generate_video` and the video poller, M51.4 `speak`; M56.6:
//! `ask_user`, `react`, `edit_message`; M65: the vision helper and
//! `watch_video`).

pub mod chat;
pub mod look;
pub mod media;
pub mod send_file;
pub mod watch;

pub use chat::{chat_tools, AskUserTool, EditMessageTool, Questions, ReactTool};
pub use media::{
    GenerateImageTool, GenerateVideoTool, MediaLimits, MediaPoller, MediaShared, MediaSpend,
    SpeakTool,
};
pub use send_file::{FileOut, SendFileTool};
pub use watch::{Ffmpeg, WatchVideoTool};
