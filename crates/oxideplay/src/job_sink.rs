//! `JobSink` implementation that forwards executor events to the
//! main-thread [`crate::engine::PlayerEngine`] via a bounded channel.
//!
//! This is the only sink oxideplay registers — both plain playback
//! (`oxideplay file.mp4`) and `--job` / `--inline` flow through the
//! same path. The executor runs on a worker thread, the engine runs
//! on the main thread, the bounded channel between them provides
//! natural pause/back-pressure.

use std::sync::mpsc::SyncSender;

use oxideav::pipeline::{BarrierKind, JobSink};
use oxideav_core::{Error, Frame, FrameLease, MediaType, Packet, Result, StreamInfo};

use crate::engine::EngineMsg;

/// Cross-thread sink: forwards every JobSink callback into a
/// `SyncSender<EngineMsg>` consumed by [`crate::engine::PlayerEngine`].
///
/// Holds no driver / non-Send state — driver ownership lives entirely
/// on the main thread inside the engine.
pub struct ChannelSink {
    tx: SyncSender<EngineMsg>,
    debug: bool,
    seen_audio: usize,
    seen_video: usize,
}

impl ChannelSink {
    pub fn new(tx: SyncSender<EngineMsg>) -> Self {
        let debug = std::env::var("OXIDEPLAY_SINK_DEBUG")
            .ok()
            .filter(|v| !v.is_empty() && v != "0")
            .is_some();
        Self {
            tx,
            debug,
            seen_audio: 0,
            seen_video: 0,
        }
    }
}

impl JobSink for ChannelSink {
    fn start(&mut self, streams: &[StreamInfo]) -> Result<()> {
        if self.debug {
            eprintln!("[sink] start: {} streams", streams.len());
        }
        self.tx
            .send(EngineMsg::Started(streams.to_vec()))
            .map_err(|_| Error::other("oxideplay: engine receiver dropped before start"))
    }

    fn write_packet(&mut self, _kind: MediaType, _pkt: &Packet) -> Result<()> {
        // The `@display` reserved sink consumes raw frames. Any
        // path that delivers packets here has been mis-configured
        // (e.g. user wrote `codec: copy` for a file output that's
        // actually pointed at the player). Fail loudly with the
        // same message the legacy `PlayerSink` used.
        Err(Error::unsupported(
            "oxideplay: @display sink needs decoded frames; \
             remove `codec` or set it to the source codec with a decoder",
        ))
    }

    fn write_frame(&mut self, kind: MediaType, frame: &Frame) -> Result<()> {
        // Legacy compatibility path. The lease-aware pipeline calls
        // `write_frame_lease` directly, so normal playback never deep-clones a
        // decoded frame here.
        self.write_frame_lease(kind, FrameLease::from_frame(frame.clone()))
    }

    fn write_frame_lease(&mut self, kind: MediaType, frame: FrameLease) -> Result<()> {
        if self.debug {
            if let Some(frame) = frame.as_frame() {
                match frame {
                    Frame::Audio(af) => {
                        self.seen_audio += 1;
                        if self.seen_audio <= 5 || self.seen_audio % 50 == 0 {
                            eprintln!(
                                "[sink] audio frame #{} samples={} pts={:?}",
                                self.seen_audio, af.samples, af.pts
                            );
                        }
                    }
                    Frame::Video(vf) => {
                        self.seen_video += 1;
                        if self.seen_video <= 5 || self.seen_video % 50 == 0 {
                            eprintln!(
                                "[sink] video frame #{} planes={} pts={:?}",
                                self.seen_video,
                                vf.planes.len(),
                                vf.pts
                            );
                        }
                    }
                    _ => {}
                }
            } else if let Some(hw) = frame.as_hardware_video() {
                self.seen_video += 1;
                if self.seen_video <= 5 || self.seen_video % 50 == 0 {
                    eprintln!(
                        "[sink] video frame #{} hardware={} {}x{} pts={:?}",
                        self.seen_video,
                        hw.backend(),
                        hw.width(),
                        hw.height(),
                        hw.pts()
                    );
                }
            } else if frame.as_arena_video().is_some() {
                self.seen_video += 1;
            }
        }
        self.tx
            .send(EngineMsg::Frame { kind, frame })
            .map_err(|_| Error::other("oxideplay: engine receiver dropped"))
    }

    fn barrier(&mut self, kind: BarrierKind) -> Result<()> {
        if self.debug {
            eprintln!("[sink] barrier: {:?}", kind);
        }
        self.tx
            .send(EngineMsg::Barrier(kind))
            .map_err(|_| Error::other("oxideplay: engine receiver dropped during barrier"))
    }

    fn finish(&mut self) -> Result<()> {
        if self.debug {
            eprintln!(
                "[sink] finish: total audio={} video={}",
                self.seen_audio, self.seen_video
            );
        }
        // Best-effort: if the engine has already exited, swallow
        // the disconnection error so the executor can wind down
        // cleanly.
        let _ = self.tx.send(EngineMsg::Finished);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::any::Any;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc};

    use oxideav_core::{
        HardwareVideoFrame, HardwareVideoFrameStorage, PixelFormat, VideoFrame, VideoPlane,
    };

    struct CountingHardwareStorage {
        materializations: Arc<AtomicUsize>,
    }

    impl HardwareVideoFrameStorage for CountingHardwareStorage {
        fn backend(&self) -> &'static str {
            "test-hardware"
        }

        fn width(&self) -> u32 {
            2
        }

        fn height(&self) -> u32 {
            2
        }

        fn pixel_format(&self) -> PixelFormat {
            PixelFormat::Yuv420P
        }

        fn pts(&self) -> Option<i64> {
            Some(9)
        }

        fn as_any(&self) -> &dyn Any {
            self
        }

        fn materialize(&self) -> Result<VideoFrame> {
            self.materializations.fetch_add(1, Ordering::SeqCst);
            Ok(VideoFrame {
                pts: Some(9),
                planes: vec![
                    VideoPlane {
                        stride: 2,
                        data: vec![16; 4],
                    },
                    VideoPlane {
                        stride: 1,
                        data: vec![128],
                    },
                    VideoPlane {
                        stride: 1,
                        data: vec![128],
                    },
                ],
            })
        }
    }

    #[test]
    fn channel_sink_preserves_owned_cpu_lease_identity() {
        let (tx, rx) = mpsc::sync_channel(1);
        let mut sink = ChannelSink::new(tx);
        let lease = FrameLease::from_frame(Frame::Video(VideoFrame {
            pts: Some(3),
            planes: vec![VideoPlane {
                stride: 2,
                data: vec![1, 2, 3, 4],
            }],
        }));
        let retained = lease.clone();

        sink.write_frame_lease(MediaType::Video, lease)
            .expect("send CPU frame lease");
        let EngineMsg::Frame {
            frame: received, ..
        } = rx.recv().expect("receive frame")
        else {
            panic!("expected frame message");
        };

        match (&retained, &received) {
            (FrameLease::Owned(before), FrameLease::Owned(after)) => {
                assert!(Arc::ptr_eq(before, after));
            }
            _ => panic!("expected heap-backed retained frame leases"),
        }
    }

    #[test]
    fn channel_sink_does_not_materialize_hardware_lease() {
        let materializations = Arc::new(AtomicUsize::new(0));
        let lease =
            FrameLease::from_hardware_video(HardwareVideoFrame::new(CountingHardwareStorage {
                materializations: Arc::clone(&materializations),
            }));
        let (tx, rx) = mpsc::sync_channel(1);
        let mut sink = ChannelSink::new(tx);

        sink.write_frame_lease(MediaType::Video, lease)
            .expect("send hardware frame lease");
        let EngineMsg::Frame { frame, .. } = rx.recv().expect("receive frame") else {
            panic!("expected frame message");
        };

        assert!(frame.is_hardware_video());
        assert_eq!(frame.pts(), Some(9));
        assert_eq!(materializations.load(Ordering::SeqCst), 0);
    }
}
