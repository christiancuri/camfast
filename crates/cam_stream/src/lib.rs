//! Live camera pipeline: RTSP (retina, TCP) → VideoToolbox hardware decode → latest-frame slot.
//!
//! The UI only sees [`StreamManager`] (start/stop/pause streams) and [`StreamShared`]
//! (latest `'420f'` CVPixelBuffer + status + stats, plus a coalesced "changed" signal).

mod decoder;
mod manager;
mod nal;
mod policy;
mod rtsp;
mod shared;
mod timing;

pub use decoder::{PIXEL_FORMAT_420F, fourcc_str, is_iosurface_backed};
pub use manager::{ProbeInfo, StreamManager};
pub use shared::{StatsSnapshot, StreamShared, StreamStatus};
