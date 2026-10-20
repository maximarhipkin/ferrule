//! A deterministic backend for tests: fixed image bytes, a video job whose
//! polls count down before completing, every call remembered.

use crate::{ImageOut, ImageReq, MediaBackend, MediaError, VideoJob, VideoReq, VideoState};
use std::path::Path;
use std::sync::Mutex;

pub struct FakeMedia {
    /// What `generate_image` returns (a real 1×1 PNG by default).
    pub image: ImageOut,
    /// Polls before `Done` (then `Done` forever).
    pub polls_left: Mutex<u32>,
    /// When set, videos end `Failed` with it after the countdown.
    pub fail: Option<String>,
    pub images: Mutex<Vec<ImageReq>>,
    pub videos: Mutex<Vec<VideoReq>>,
    next_id: Mutex<u32>,
}

/// A real 1×1 transparent PNG.
pub const PIXEL: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x62, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
    0x42, 0x60, 0x82,
];

impl Default for FakeMedia {
    fn default() -> Self {
        Self {
            image: ImageOut {
                bytes: PIXEL.to_vec(),
                format: "png".into(),
            },
            polls_left: Mutex::new(0),
            fail: None,
            images: Mutex::default(),
            videos: Mutex::default(),
            next_id: Mutex::new(0),
        }
    }
}

impl FakeMedia {
    pub fn slow(polls: u32) -> Self {
        let f = Self::default();
        *f.polls_left.lock().unwrap() = polls;
        f
    }

    pub fn failing(reason: &str) -> Self {
        Self {
            fail: Some(reason.into()),
            ..Self::default()
        }
    }
}

#[async_trait::async_trait]
impl MediaBackend for FakeMedia {
    async fn generate_image(&self, req: ImageReq) -> Result<ImageOut, MediaError> {
        self.images.lock().unwrap().push(req);
        Ok(self.image.clone())
    }

    async fn submit_video(&self, req: VideoReq) -> Result<VideoJob, MediaError> {
        self.videos.lock().unwrap().push(req);
        let mut next = self.next_id.lock().unwrap();
        *next += 1;
        Ok(VideoJob {
            id: format!("fake-{}", *next),
        })
    }

    async fn poll_video(&self, _job: &str) -> Result<VideoState, MediaError> {
        let mut left = self.polls_left.lock().unwrap();
        if *left > 0 {
            *left -= 1;
            return Ok(VideoState::Pending);
        }
        if let Some(reason) = &self.fail {
            return Ok(VideoState::Failed {
                reason: reason.clone(),
            });
        }
        Ok(VideoState::Done {
            url: "https://cdn.example/fake.mp4".into(),
        })
    }

    async fn download(&self, _url: &str, dest: &Path) -> Result<(), MediaError> {
        // A minimal MP4 header (ftyp box), so the sniff check passes.
        let bytes: &[u8] = b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00mp42isom";
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| MediaError::BadAnswer(format!("{}: {e}", parent.display())))?;
        }
        std::fs::write(dest, bytes)
            .map_err(|e| MediaError::BadAnswer(format!("{}: {e}", dest.display())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_fake_counts_down_then_completes() {
        let f = FakeMedia::slow(2);
        let job = f.submit_video(VideoReq::default()).await.unwrap();
        assert_eq!(job.id, "fake-1");
        assert_eq!(f.poll_video(&job.id).await.unwrap(), VideoState::Pending);
        assert_eq!(f.poll_video(&job.id).await.unwrap(), VideoState::Pending);
        assert!(matches!(
            f.poll_video(&job.id).await.unwrap(),
            VideoState::Done { .. }
        ));
        assert_eq!(f.videos.lock().unwrap().len(), 1);

        let f = FakeMedia::failing("the provider ran out of GPUs");
        let job = f.submit_video(VideoReq::default()).await.unwrap();
        assert!(matches!(
            f.poll_video(&job.id).await.unwrap(),
            VideoState::Failed { reason } if reason.contains("GPUs")
        ));

        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("media/out.mp4");
        f.download("https://x", &dest).await.unwrap();
        assert_eq!(
            crate::openrouter::sniff(&std::fs::read(&dest).unwrap()),
            Some("mp4")
        );
    }
}
