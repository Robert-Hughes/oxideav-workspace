//! Split-responsibility output traits.
//!
//! The old `OutputDriver` trait handled audio *and* video in one object,
//! which forced every driver (SDL2, winit) to own both halves. The
//! `--vo` / `--ao` CLI flags need the opposite: any video engine
//! composed with any audio engine. We split into:
//!
//! - [`VideoEngine`]: presents frames and pumps window-input events.
//! - [`AudioEngine`]: consumes audio frames and owns the master clock.
//!
//! A [`Composite`] struct carries an `Option<Box<dyn VideoEngine>>` and
//! an `Option<Box<dyn AudioEngine>>`, and implements the player's
//! original [`OutputDriver`] trait on top of them. That way the player
//! core (and every call-site that already speaks `OutputDriver`) keeps
//! working unchanged — the composition happens at `build_driver` time.

use std::time::Duration;

use oxideav_core::{
    AudioFrame, ChannelLayout, CodecParameters, Error, Frame, FrameLease, Result, VideoFrame,
};

use crate::driver::{OutputDriver, OverlayState, PlayerEvent};

/// Per-frame video presentation + window-input pump. Implementations:
/// `SdlVideoEngine`, `WinitVideoEngine`, `NullVideoEngine` (used when
/// `--vo none`).
pub trait VideoEngine: Send {
    fn present(&mut self, frame: &VideoFrame) -> Result<()>;

    /// Present a decoded-video lease. Heap-backed CPU frames are borrowed
    /// directly; arena/hardware representations materialise here only when the
    /// concrete video engine does not override this method.
    fn present_lease(&mut self, frame: &FrameLease) -> Result<()> {
        match frame.as_frame() {
            Some(Frame::Video(video)) => return self.present(video),
            Some(_) => {
                return Err(Error::invalid(
                    "oxideplay: video engine received a non-video frame lease",
                ))
            }
            None => {}
        }
        match frame.materialize()? {
            Frame::Video(video) => self.present(&video),
            _ => Err(Error::invalid(
                "oxideplay: video engine materialised a non-video frame",
            )),
        }
    }
    /// Drain any queued user-input events (keyboard, close button).
    /// Audio-only engines return an empty Vec.
    fn poll_events(&mut self) -> Vec<PlayerEvent> {
        Vec::new()
    }
    /// The video path gets a chance to react to pause, e.g. by
    /// pausing a GPU render timer. Most just don't care.
    fn set_paused(&mut self, _paused: bool) {}
    /// One-line human-readable description — driver name, GPU / render
    /// backend, initial surface size, pixel format, etc. Printed by
    /// `oxideplay` at startup so users can confirm they got the
    /// backend they expected.
    fn info(&self) -> String {
        "unknown".into()
    }
    /// Push the latest player state for the on-screen overlay UI to
    /// render. Called every engine tick. Default is a no-op — only
    /// the winit (egui) engine implements it.
    fn set_overlay_state(&mut self, _state: OverlayState) {}

    /// Tell the video engine the source's stream-level shape (pixel
    /// format + width + height + time_base) once at stream open.
    /// Engines used to read these off each `VideoFrame`; the slim
    /// moved them onto `CodecParameters` so the engine pulls them
    /// from there and pushes them in here. Default no-op so engines
    /// without a video path compile unchanged.
    fn set_source_video_params(&mut self, _params: &CodecParameters) {}

    /// True for engines that have no real-time deadline (e.g.
    /// `--vo hash`). The player's main loop skips A/V pacing +
    /// stale-frame trim when this is true, so every decoded frame
    /// is presented in order at max throughput. Default false:
    /// real renderers want pts-paced presentation.
    fn drains_immediately(&self) -> bool {
        false
    }
}

/// Audio output + master-clock owner. Implementations: `SdlAudioEngine`,
/// `SysAudioEngine`, `NullAudioEngine` (used when `--ao none`).
pub trait AudioEngine: Send {
    fn queue(&mut self, frame: &AudioFrame) -> Result<()>;

    /// Current position of the master clock — typically
    /// `samples_played / sample_rate`.
    fn master_clock_pos(&self) -> Duration;

    fn set_paused(&mut self, paused: bool);
    fn set_volume(&mut self, vol: f32);

    /// Approximate samples still queued to the device. Used by the
    /// player to throttle the decoder.
    fn audio_queue_len_samples(&self) -> u64 {
        0
    }
    /// How many samples (per channel) can still be queued before the
    /// backend starts dropping. `u64::MAX` means "no soft cap" (engines
    /// that block or grow on demand). The player consults this as its
    /// audio-side back-pressure signal: if the headroom drops below a
    /// threshold, it stops pulling new audio frames from the decode
    /// worker and lets the downstream channels fill, which eventually
    /// blocks the decoder and then the demuxer.
    fn audio_headroom_samples(&self) -> u64 {
        u64::MAX
    }
    /// Output-side latency reported by the backend, if available.
    /// See `oxideav_sysaudio::Stream::latency` — over Bluetooth /
    /// network sinks this matters for A/V sync compensation.
    #[allow(dead_code)] // consumed once A/V-sync compensation lands in the sync layer
    fn latency(&self) -> Option<Duration> {
        None
    }
    /// One-line human-readable description — driver name, device
    /// sample rate / channels / format, and a note on how the
    /// backend measures `latency()` (end-to-end vs. driver-queue vs.
    /// software-estimate). Printed by `oxideplay` at startup.
    fn info(&self) -> String {
        "unknown".into()
    }
    /// Tell the engine the *source*'s authoritative speaker layout (as
    /// surfaced by `CodecParameters::resolved_layout`). Audio engines
    /// that do surround-aware downmix (sysaudio) consult this to pick
    /// the right matrix; the SDL2 / null engines no-op. Default impl
    /// is a no-op so existing engines compile unchanged.
    fn set_source_layout(&mut self, _layout: Option<ChannelLayout>) {}

    /// Tell the engine the source's stream-level audio shape (sample
    /// format + sample rate + channel count + layout) once at stream
    /// open. Engines used to read these off each `AudioFrame`; the
    /// slim moved them onto `CodecParameters` so the engine pulls
    /// them from there and pushes them in here. Default no-op so
    /// engines without an audio path (or that only need
    /// `set_source_layout`) compile unchanged.
    fn set_source_audio_params(&mut self, _params: &CodecParameters) {}
}

/// Combines an optional video engine with an optional audio engine into
/// the player's original [`OutputDriver`] trait. `--vo none` → `None`
/// for the video slot (present is a no-op, poll_events returns empty);
/// `--ao none` → `None` for audio (clock ticks from a wall-clock
/// fallback).
pub struct Composite {
    pub video: Option<Box<dyn VideoEngine>>,
    pub audio: Option<Box<dyn AudioEngine>>,
    /// Fallback wall-clock start used when `audio` is None. Set to
    /// `None` while paused so elapsed doesn't keep accumulating.
    wall_start: Option<std::time::Instant>,
    wall_accum: Duration,
}

impl Composite {
    pub fn new(video: Option<Box<dyn VideoEngine>>, audio: Option<Box<dyn AudioEngine>>) -> Self {
        Self {
            video,
            audio,
            wall_start: Some(std::time::Instant::now()),
            wall_accum: Duration::ZERO,
        }
    }
}

impl OutputDriver for Composite {
    fn present_video(&mut self, frame: &VideoFrame) -> Result<()> {
        match self.video.as_mut() {
            Some(v) => v.present(frame),
            None => Ok(()),
        }
    }

    fn present_video_lease(&mut self, frame: &FrameLease) -> Result<()> {
        match self.video.as_mut() {
            Some(v) => v.present_lease(frame),
            None => Ok(()),
        }
    }

    fn queue_audio(&mut self, frame: &AudioFrame) -> Result<()> {
        match self.audio.as_mut() {
            Some(a) => a.queue(frame),
            None => Ok(()),
        }
    }

    fn poll_events(&mut self) -> Vec<PlayerEvent> {
        match self.video.as_mut() {
            Some(v) => v.poll_events(),
            None => Vec::new(),
        }
    }

    fn master_clock_pos(&self) -> Duration {
        if let Some(a) = self.audio.as_ref() {
            return a.master_clock_pos();
        }
        // No audio output → walk wall-clock time. Accurate enough for
        // video-only playback; the decoder pacing doesn't need sample
        // precision.
        match self.wall_start {
            Some(t) => self.wall_accum + t.elapsed(),
            None => self.wall_accum,
        }
    }

    fn set_paused(&mut self, paused: bool) {
        if let Some(a) = self.audio.as_mut() {
            a.set_paused(paused);
        }
        if let Some(v) = self.video.as_mut() {
            v.set_paused(paused);
        }
        // Freeze / unfreeze the wall-clock fallback regardless of
        // whether the audio engine exists — it's only consulted when
        // audio is absent but should behave consistently.
        if paused {
            if let Some(t) = self.wall_start.take() {
                self.wall_accum += t.elapsed();
            }
        } else if self.wall_start.is_none() {
            self.wall_start = Some(std::time::Instant::now());
        }
    }

    fn set_volume(&mut self, vol: f32) {
        if let Some(a) = self.audio.as_mut() {
            a.set_volume(vol);
        }
    }

    fn audio_queue_len_samples(&self) -> u64 {
        self.audio
            .as_ref()
            .map(|a| a.audio_queue_len_samples())
            .unwrap_or(0)
    }

    fn audio_headroom_samples(&self) -> u64 {
        self.audio
            .as_ref()
            .map(|a| a.audio_headroom_samples())
            .unwrap_or(u64::MAX)
    }

    fn engine_info(&self) -> (Option<String>, Option<String>) {
        (
            self.video.as_ref().map(|v| v.info()),
            self.audio.as_ref().map(|a| a.info()),
        )
    }

    fn set_overlay_state(&mut self, state: OverlayState) {
        if let Some(v) = self.video.as_mut() {
            v.set_overlay_state(state);
        }
    }

    fn set_source_layout(&mut self, layout: Option<ChannelLayout>) {
        if let Some(a) = self.audio.as_mut() {
            a.set_source_layout(layout);
        }
    }

    fn set_source_audio_params(&mut self, params: &CodecParameters) {
        if let Some(a) = self.audio.as_mut() {
            a.set_source_audio_params(params);
        }
    }

    fn set_source_video_params(&mut self, params: &CodecParameters) {
        if let Some(v) = self.video.as_mut() {
            v.set_source_video_params(params);
        }
    }

    fn video_drains_immediately(&self) -> bool {
        self.video
            .as_ref()
            .map(|v| v.drains_immediately())
            .unwrap_or(false)
    }
}

// `--vo none` and `--ao none` are handled by passing `None` for the
// respective slot in `Composite`; no stub engine needed.

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use oxideav_core::{Frame, VideoPlane};

    struct LeaseAwareVideoEngine {
        lease_called: Arc<AtomicBool>,
        legacy_called: Arc<AtomicBool>,
    }

    impl VideoEngine for LeaseAwareVideoEngine {
        fn present(&mut self, _frame: &VideoFrame) -> Result<()> {
            self.legacy_called.store(true, Ordering::SeqCst);
            Ok(())
        }

        fn present_lease(&mut self, frame: &FrameLease) -> Result<()> {
            assert!(matches!(frame.as_frame(), Some(Frame::Video(_))));
            self.lease_called.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn composite_forwards_video_lease_without_using_legacy_present() {
        let lease_called = Arc::new(AtomicBool::new(false));
        let legacy_called = Arc::new(AtomicBool::new(false));
        let engine = LeaseAwareVideoEngine {
            lease_called: Arc::clone(&lease_called),
            legacy_called: Arc::clone(&legacy_called),
        };
        let mut composite = Composite::new(Some(Box::new(engine)), None);
        let lease = FrameLease::from_frame(Frame::Video(VideoFrame {
            pts: Some(1),
            planes: vec![VideoPlane {
                stride: 1,
                data: vec![0],
            }],
        }));

        composite
            .present_video_lease(&lease)
            .expect("lease-aware video presentation");

        assert!(lease_called.load(Ordering::SeqCst));
        assert!(!legacy_called.load(Ordering::SeqCst));
    }

    struct BorrowCheckingVideoEngine {
        expected_ptr: usize,
        saw_same_buffer: Arc<AtomicBool>,
    }

    impl VideoEngine for BorrowCheckingVideoEngine {
        fn present(&mut self, frame: &VideoFrame) -> Result<()> {
            let ptr = frame
                .planes
                .first()
                .expect("test video plane")
                .data
                .as_ptr() as usize;
            self.saw_same_buffer
                .store(ptr == self.expected_ptr, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn default_video_engine_adapter_borrows_owned_cpu_frame_without_copy() {
        let lease = FrameLease::from_frame(Frame::Video(VideoFrame {
            pts: Some(2),
            planes: vec![VideoPlane {
                stride: 2,
                data: vec![1, 2, 3, 4],
            }],
        }));
        let expected_ptr = match lease.as_frame() {
            Some(Frame::Video(video)) => video.planes[0].data.as_ptr() as usize,
            _ => panic!("expected owned video lease"),
        };
        let saw_same_buffer = Arc::new(AtomicBool::new(false));
        let engine = BorrowCheckingVideoEngine {
            expected_ptr,
            saw_same_buffer: Arc::clone(&saw_same_buffer),
        };
        let mut composite = Composite::new(Some(Box::new(engine)), None);

        composite
            .present_video_lease(&lease)
            .expect("borrowed CPU presentation");

        assert!(saw_same_buffer.load(Ordering::SeqCst));
    }
}
