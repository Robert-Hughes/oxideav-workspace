# Local OxideAV changes

This file records the locally developed OxideAV framework work that has not yet
necessarily landed in upstream releases. It is intentionally consumer-agnostic:
applications should depend on OxideAV library crates and public contracts rather
than on the `oxideplay` reference player.

## Local integration model

During co-development, consumers can keep ordinary versioned OxideAV dependencies
and use Cargo `[patch.crates-io]` overrides for the selected local workspace. Patch
the complete OxideAV dependency graph in use so Cargo cannot load both local and
crates.io copies of core traits/types; mixed copies create incompatible Rust type
identities.

## Confirmed local platform stack

- GhostBSD/FreeBSD with NVIDIA GeForce GTX 1080, driver 580.173.02.
- wgpu/Vulkan presentation works through the `oxideplay` reference consumer.
- FreeBSD OSS audio output is implemented in `oxideav-sysaudio`.
- VDPAU is enabled on FreeBSD and H.264 has a streaming `Decoder` factory.
- HLS VOD source support exists in `oxideav-hls`.
- Native `oxideav-aac` is fast enough for real-time playback without Symphonia.

Root reference-player integration checkpoint:

```text
bc39487 feat(oxideplay): support native HLS playback on FreeBSD
```

Local automated media/audio tests should use muted/null/hash audio output unless
sound is explicitly required by the test.

## Runtime diagnostics through the log facade

Components:

```text
ebf6072 Use log facade for HLS diagnostics
1139495 Use log facade for VDPAU diagnostics
4952a10 Use log facade for pipeline diagnostics
695feef Use log facade for H264 diagnostics
```

Library runtime diagnostics on embedding applications' playback paths now use Rust's
log facade instead of writing directly to stderr. This lets an embedding
application install one logger and route OxideAV lifecycle, warning and
explicitly enabled H.264 trace messages to the same destination as its own
diagnostics. Test-only skip output and standalone CLI output remain process
stdout/stderr.

## FreeBSD OSS audio

Component commits:

```text
5195ab8 feat(sysaudio): support OSS output on FreeBSD
a988019 fix(sysaudio): report OSS output queue latency
```

The backend uses FreeBSD's native OSS `/dev/dsp*` ABI and FreeBSD ioctl request
encoding rather than reusing Linux constants. It negotiates S16_LE, channels and
sample rate, then runs the normal worker/callback model. libc is runtime-loaded,
so this adds no compile-time audio-library dependency.

`Stream::latency()` now queries `SNDCTL_DSP_GETODELAY` for the live number of
queued output bytes and converts that S16_LE queue depth to a duration. OSS
implementations that reject the ioctl retain the previous one-period estimate as
a compatibility fallback. The FreeBSD hardware smoke test exercises the real
ioctl path with silence.

## HLS VOD source

Components:

```text
147e0b6 feat(http): add bounded whole-resource GET helper
798d0be feat(hls): add lazy MPEG-TS VOD source
5e0775e feat(hls): expose master playlist variants
b0234ab feat(source): add seekable packet sources
96e3e49 feat(pipeline): seek packet sources
3ef1eab feat(hls): seek VOD segments by media time
df5c63c feat(hls): prefetch next VOD segment
7f0acec feat(meta): wire HLS and FreeBSD VDPAU
```

V1 intentionally supports a narrow, useful shape:

- HTTP(S) master/media playlists;
- completed playlists (`#EXT-X-ENDLIST`);
- one fixed rendition at a time (default maximum height 720);
- whole MPEG-TS segments;
- media-relative VOD seeking via `#EXTINF` segment timing plus MPEG-TS access-point seeking;
- no encryption, byte ranges, `#EXT-X-MAP`/fMP4, discontinuities, nested
  masters, live reload or ABR.

The source registers as `hls+http://` / `hls+https://` and opens as a
`PacketSource`, not as one virtual concatenated byte stream. It retains each
media segment's resolved URL, cumulative `#EXTINF` start time and duration, and
owns one ordinary MPEG-TS demuxer for the active segment. Sequential playback
also maintains exactly one HLS-local prepared successor. As soon as a segment is
installed, a background worker opens the next segment's HTTP source and primes
its MPEG-TS demuxer through the first packet. At current-segment EOF the prepared
demuxer and retained first packet are installed directly, avoiding a synchronous
HEAD/GET/demux startup round trip at the media boundary.

The readahead is deliberately bounded to one successor and is independent of the
decoded-frame/audio-buffer backpressure policy. A preparation error is retained
and surfaced only if and when that successor becomes current. A successful seek
invalidates the old successor slot and starts preparation of the segment after
the actual landed segment; an already-running stale worker may finish its one
HTTP preparation after its receiver is dropped, but its result cannot become
current. MPEG-TS parsing itself remains HLS-agnostic.

`5e0775e` adds an application-facing `inspect_hls()` discovery API. One bounded
GET of a master returns every non-I-frame variant with an absolute media-playlist
URL and the useful `#EXT-X-STREAM-INF` / linked video-rendition metadata, plus
the index selected by the existing fixed-rendition policy. A player can therefore
enumerate qualities from the master and then open the chosen media URL directly:
one GET for the master, one GET for the chosen media playlist, with no duplicate
master fetch. This remains fixed-rendition discovery rather than ABR.

### Why manifests use bounded GET

Twitch Usher signed playlist URLs accepted GET but returned 404 to HEAD during
validation. `oxideav_http::fetch_bytes()` therefore exists for small metadata
resources: a normal GET with a hard byte cap and the HTTP driver's existing
redirect/content-encoding policy. HLS manifests use it rather than the seekable
HEAD/range segment-media path.

### Packet-source seeking

`b0234ab` adds an optional `PacketSource::seek_to(stream_index, pts)` method. The
default is `Error::Unsupported`, so sequential packet sources remain source- and
behaviour-compatible. A successful implementation must mutate its internal read
position so the next `next_packet()` continues from the landing point, and returns
the actual landed timestamp in the selected stream's time base.

`96e3e49` teaches the staged pipeline to handle that method exactly like demuxer
seeking. `ExecutorHandle::seek_with_generation()` still supplies the public control
surface; a successful packet-source seek produces `SeekFlush { generation,
landed_pts, time_base }`, while a rejected seek produces `SeekRejected`. Existing
worker barrier handling resets decoder/filter state only on `SeekFlush`. The same
change also preserves/rescales source `StreamInfo.duration` into sink-facing output
metadata instead of discarding it.

### HLS VOD seek model

`3ef1eab` replaces byte-length indexing with playlist-time indexing. At media-playlist
open, HLS builds cumulative starts directly from `#EXTINF` and exposes the summed
duration on its streams. It opens the first MPEG-TS segment, reads only enough initial
packets to learn trustworthy stream start PTS values, caches those packets for normal
replay, and records the minimum first PTS as the transport-clock origin.

A seek receives the normal raw stream PTS from the pipeline. HLS subtracts its transport
origin to obtain media-relative seconds, binary-searches the cumulative `#EXTINF` table
to choose the target segment, opens only that segment, and delegates the final decode-safe
landing to the existing MPEG-TS `seek_to()` implementation. For A/V renditions the inner
seek targets the video stream so the result is a valid video access point even when the
caller addressed an audio route. If nominal `#EXTINF` timing selects a segment whose
first access point lies after the requested raw PTS, HLS retries the preceding segment so
the public nearest-at-or-before contract is preserved. The selected MPEG-TS demuxer is
then installed as current state, so subsequent `next_packet()` calls continue from the
new position.

This avoids the old failure mode where generic MPEG-TS byte bisection needed an end
position for a virtual stream composed of thousands of lazily sized HTTP objects. An
HLS seek now selects a segment in-memory without probing intervening segment lengths;
only the chosen segment (or, for the boundary fallback, its predecessor) needs to be
opened.

Known HLS follow-ups: discontinuities, byte ranges, fMP4/MAP, encryption, live reload,
ABR and more sophisticated cross-rendition switching.

## H.264 packetisation and shared picture state

Components:

```text
1041a0f feat(h264): add shared streaming frontends
8d2a3e5 refactor(h264): share picture state with software decoder
a1a2463 feat(vdpau): add FreeBSD H.264 streaming decoder
2869584 fix(vdpau): reject synthetic H264 gap references
```

### Problem discovered on Twitch

MPEG-TS demuxing naturally emits PES-derived `Packet`s. Those boundaries are not
H.264 NAL/access-unit boundaries: a Twitch PES can begin with continuation bytes
from the preceding NAL/picture and contain the AUD that begins the next picture.
The generic pure-Rust decoder and the first VDPAU adapter treated packets too
locally, while NVIDIA NVDEC's `cuvidParser` already owns incremental stream
parsing internally.

### Deliberately opt-in architecture

Do **not** impose one mandatory H.264 preprocessing pipeline on every backend.
The reusable services are independently opt-in:

```text
A. raw byte packets
   → backend owns incremental parsing
   → e.g. NVDEC / cuvidParser

B. complete Annex-B access units
   → AnnexBAccessUnitAssembler reconstructs across PES/Packet boundaries

C. parsed/derived pictures
   → H264PictureFrontend owns POC + reference DPB + MMCO + frame_num gaps
```

`AnnexBAccessUnitAssembler` preserves the timestamp belonging to the PES in
which an AUD-delimited access unit begins, appends leading continuation bytes
to the preceding AU, and can emit multiple AUs from one packet without
inventing timestamps for later units. AUD-free packet-aligned Annex-B retains a
fallback path; AUD is not mandatory in H.264, so this helper is intentionally
not sold as a universal syntax-driven packetiser.

`H264PictureFrontend` is transactional: prepare derives/simulates POC, DPB,
MMCO and gap effects; `commit()` advances shared state only after the backend
successfully reconstructs the picture. Opaque DPB keys let software map
references to sample buffers and hardware map them to surfaces.

The software decoder keeps its rich slice/pixel path (PAFF/MBAFF, data
partitioning, separate colour planes, etc.) and calls
`prepare_parsed_picture()` to share only cross-picture state. VDPAU opts into
both AU reconstruction and the picture frontend. NVDEC can opt into neither.

VDPAU cannot materialise H.264 §8.2.5.2 synthetic non-existing references as
real video surfaces; it therefore returns `Unsupported` for that case so the
registry can fall back to software.

### Real-stream regression evidence

Forced-software MPEG-TS path after the refactor:

```text
Big Buck Bunny: 600 frames, decode_errors=0, hash dfa78f766b71b073
Twitch segment:  600 frames, decode_errors=0, hash 96969d9b7fcdd1ce
```

The Twitch hash matches the corresponding pre-refactor output. VDPAU's exact
600-frame streaming integration test also remains green. The complete
`oxideav-h264` suite and VDPAU suite passed after the migration.

Current VDPAU limitations are explicit rather than silently approximated:
progressive frame pictures, 8-bit 4:2:0, supported profiles/resolutions, and no
untranslated custom scaling matrices or synthetic gap references. VDPAU now
publishes retainable hardware-surface leases without CPU readback in the decoder
or pipeline. Direct `VdpVideoSurface` → wgpu/Vulkan sampling is still a future
renderer-interop optimisation; legacy renderers explicitly materialise I420 only
at their final presentation boundary.

## Retainable decoded-frame leases

Components:

```text
c6e6f02 feat(frame): add retainable decoded frame leases
4c7099a feat(pipeline): propagate decoded frame leases
bf7e468 feat(vdpau): expose reusable hardware frame leases
a8c1c96 test(vdpau): pin surface lease reuse lifetime
9c2f497 feat(player): retain decoded frame leases through presentation
46f8433 feat(arena): add mutable pooled video frame builder
94b6372 refactor(h264): store decoded pictures in compact arenas
fda3143 feat(h264): share pooled frames between DPB and output
d8ca4c2 refactor(h264): keep software output arena-backed
94de8f2 fix(h264): block on pooled picture pressure
4fe137c feat(core): add cancellable arena waits
e47b459 fix(pipeline): cancel blocked decoder waits
3f2ce28 fix(h264): avoid arena wait deadlocks
d6a6dda test(player): expose arena-backed frame transport
f11d632 feat(vdpau): expose bootstrap interop handle
e60b8cf feat(player): bridge VDPAU frames into Vulkan
3512397 perf(player): pipeline VDPAU GPU bridge
e9f8acd fix(player): mark imported bridge memory dedicated
```

`oxideav-core` now has one ownership-carrying decoded-frame contract for ordinary
CPU frames, pooled CPU arenas and hardware video surfaces:

```text
FrameLease
  ├─ Owned(Arc<Frame>)
  ├─ ArenaVideo(arena::sync::Frame)
  └─ HardwareVideo(HardwareVideoFrame)
```

The backing storage is immutable while leased. Cloning a lease retains the same
storage rather than copying decoded media. Producers may recycle pooled storage
only after decoder-internal references and every consumer lease have been
released. An asynchronous GPU consumer must retain its lease until submitted GPU
work has finished reading the surface.

`Decoder::receive_frame_lease()` is additive. Existing decoders default to moving
their legacy `Frame` behind an `Arc`, so the pipeline/player can queue it without
a deep frame clone. A decoder with native arena or hardware output can override
the method and publish that storage directly. The default deliberately does not
route through `receive_arena_frame()`: that older compatibility API is video-only
and some current implementations first copy a legacy frame into an arena, which
would make it a worse default than retaining the already-owned `Frame`.

The serial and staged pipeline executors now carry `FrameLease` through direct
playback routes. `JobSink::write_frame_lease()` and `Sink::write_frame_lease()`
have compatibility adapters for older sinks. Filters and encoders still consume
the legacy `Frame` API, so a lease materialises exactly when it crosses one of
those boundaries; a direct player route does not materialise it.

### Independent per-track sink back-pressure

The staged executor now supports an opt-in independent-track output contract for
player-style consumers. `JobSink` remains the whole-output owner, so mux/file sinks
continue to receive all streams through the historical aggregate callbacks. After
`JobSink::start()`, a sink may instead return one `TrackSink` per primary pipeline
track from `open_track_sinks()`.

A returned `TrackSink` is moved into that track's terminal worker. The terminal stage
therefore calls the sink synchronously itself: direct decode routes are
`decoder -> TrackSink`, the last filter owns the TrackSink when filters are present,
and an encoder owns it on encoded routes. There is no final per-track decoded-output
queue or central mux worker merely to hand an already-final result to an independent
TrackSink. Genuine processing boundaries retain their bounded queues, including
demux/source -> decoder packets, decoder -> filter frames, filter -> encoder frames,
and frame-source fan-out.

This gives each track its own back-pressure domain. A blocked video TrackSink stalls the
video terminal worker and eventually its compressed packet route, but it does not
immediately prevent an audio TrackSink from making progress. Because a shared physical
demuxer still feeds bounded per-track packet queues, prolonged pressure on one track can
eventually stop the common source; this intentionally bounds memory rather than allowing
a sibling track to run arbitrarily far ahead.

`StreamUpdate` and `BarrierKind` events use the same per-track terminal path as media,
so their order relative to that track's frames/packets is preserved without defining any
cross-track ordering. PTS remains the authoritative A/V timeline. Blocking TrackSinks
receive the executor's shared `CancellationToken` when they are created and must make
their waits cancellation-aware so sibling failure or executor abort cannot strand a
blocked terminal worker.

Independent mode currently requires one sink-visible stream per primary pipeline track;
multi-port filter extras continue to use the aggregate JobSink path and are rejected if a
consumer requests independent TrackSinks for such an output. Serial execution also keeps
the aggregate JobSink contract. TrackSink callback failures are recorded at the terminal
boundary as `FailureStage::Sink` with the owning track index before cancellation
propagates through the worker, so a decoder/filter/encoder does not get blamed for a sink
failure. Regression coverage proves independent sibling progress, cancellation of a
blocked TrackSink, and the sink-stage failure attribution.

`7b9a06d` also exposes `Executor::with_codec_preferences()`. The job executor now
threads the same `CodecPreferences` used by the lower-level pipeline into every
decode and encode stage in both serial and staged execution. A single runtime may
therefore keep software and hardware implementations registered while a consumer
explicitly requires hardware, excludes hardware, or biases named implementations.
The default remains unconstrained selection, preserving existing callers. The
`oxideav-pipeline` library suite passes all 78 tests and Clippy with warnings denied
after the change.

`2fe9a28` fixes sink-facing start-time metadata. `build_output_streams()` previously
stamped every primary stream with `start_time: Some(0)` even when the source reported
no known start yet. That is wrong for streaming/demuxed sources such as MPEG-TS, where
the first extended PTS is discovered only as PES packets flow; downstream A/V clocks
then mistake transport timestamps for media-relative timestamps. Track runtimes now
retain the selected source `start_time`: an unknown start stays `None`, while a known
start is rescaled into the output time base. Regressions cover both cases.

The VDPAU H.264 decoder now owns a bounded reusable `VdpVideoSurface` pool. The
H.264 reference-picture map and downstream presentation queue retain independent
clones of the same surface lease. A surface returns to the free pool only after
both codec and application references disappear. The hardware frame also retains
the VDPAU device/X11-display context, so queued surfaces remain valid after the
decoder itself has moved on. `VdpauVideoFrameStorage::materialize()` is the
explicit I420 readback fallback.

### VDPAU renderer interop

`GL_NV_vdpau_interop` is available in the installed NVIDIA OpenGL stack and can
register a `VdpVideoSurface` as GPU-resident GL textures. No corresponding
VDPAU-to-Vulkan import is exposed directly by VDPAU. `f11d632` therefore retains
and exposes the original driver-supplied `VdpGetProcAddress` alongside the raw
VDPAU device/surface handles so a presentation backend can initialise NVIDIA GL
interop without shortening the frame lease lifetime.

A direct wgpu-GLES presentation experiment was attempted with wgpu 29 on
GhostBSD/NVIDIA/X11. Stock wgpu rejected the GL adapter as `not compatible with
provided surface`; supplying the X11 display handle explicitly did not change
that result. A temporary diagnostic change to wgpu-hal which relaxed its Unix
EGL `NATIVE_RENDERABLE` presentation gate progressed further, but
`request_device` then failed with `Parent device is lost`. The diagnostic patch
and GLES backend-selection edits were fully reverted. The supported player path
therefore remains wgpu/Vulkan.

The Vulkan-preserving bridge is now proven and implemented in the reference
player (`e60b8cf`). On NVIDIA 580.173.02, `GL_NV_vdpau_interop2` exposes the
progressive H.264 decoder's 4:2:0 surface as a full-frame Y texture plus a
half-resolution interleaved UV texture. A private GLX context runs YUV->RGBA on
the GPU into a Vulkan-owned `RGBA8` image whose device memory is exported as an
opaque FD and imported into GL through `GL_EXT_memory_object_fd`.
`GL_NV_draw_vulkan_image` signals an ordinary raw Vulkan semaphore after the GL
write, so this works with wgpu's existing Vulkan device without requiring
`VK_KHR_external_semaphore_fd`, a wgpu fork, or a second Vulkan device.

The bridge deliberately keeps the externally-written Vulkan image outside wgpu's
resource tracker. After GL conversion, a raw Vulkan image copy moves it into a
normal wgpu-owned RGBA texture, and the ordinary wgpu/egui pass samples that
texture. This means two GPU operations per frame (YUV->RGBA render, then Vulkan
image copy), but **no VDPAU readback to CPU and no CPU upload to wgpu**. It is
therefore a zero-CPU-copy path, not yet literal zero-copy.

`e9f8acd` fixes a correctness issue in that import path: because Vulkan allocates
its shared image with `VkMemoryDedicatedAllocateInfo`, the GL memory object must be
marked `GL_DEDICATED_MEMORY_OBJECT_EXT` before `glImportMemoryFdEXT`. Without that
flag NVIDIA accepted the import but framebuffer writes were not visible, producing
a black video window despite successful decode and submission. A fresh visual
capture after the fix showed the Big Buck Bunny title card correctly through the
async bridge.

`3512397` removes the original steady-state CPU synchronization. The player now
owns four independent bridge slots; each slot retains its own `HardwareVideoFrame`
lease, VDPAU/GL registration, Vulkan external-memory staging image, semaphore,
command buffer/fence and wgpu output texture. GL queues `glSignalVkSemaphoreNV`
after the YUV->RGBA render and only calls non-blocking `glFlush`. Vulkan waits on
that semaphore on-GPU, submits the image copy, and the following wgpu draw is
naturally ordered on the same Vulkan queue. A later `vkGetFenceStatus` poll retires
the slot and releases the retained hardware lease only after the dependent copy
has completed. There is no `glFinish()` and no `vkWaitForFences()` in playback.
The initial GENERAL-layout transition is also asynchronous: Vulkan signals a
`vk_ready` semaphore and the first GL command stream enqueues
`glWaitVkSemaphoreNV`. If all four slots are genuinely still in flight, the
renderer drops that video frame rather than blocking or falling back through CPU
materialisation. `queue_wait_idle()` remains only in bridge teardown.

The isolated transport proofs verified GL-written values through Vulkan and then
an actual `VdpVideoSurface` through GLX into Vulkan. The integrated muted
1280x720 H.264 fixture completed all 96 frames as `HardwareVideo` leases with four
async slots active, wgpu still reporting the GTX 1080 Vulkan backend, no slot
saturation and no CPU materialisation fallback. The reference player test suite is
66/66 green, including regressions for first-ready-slot selection and the
all-slots-busy non-blocking policy.

### Corrected async bridge performance

Performance was re-measured only after `e9f8acd` made the shared GL/Vulkan image
visibly correct. Earlier measurements taken while the bridge rendered black are
not authoritative and should be ignored.

The corrected comparison used the same local 10.03 s, 1280x720, 60 fps H.264
transport-stream segment for both paths (600 frames, NVIDIA GTX 1080, VDPAU
decode, wgpu/Vulkan output, audio disabled), with five paced playback runs per
path. The baseline kept VDPAU hardware decode but deliberately declined the GPU
bridge, forcing `HardwareVideoFrame::materialize()` plus the existing CPU YUV
upload path.

```text
path                         real     user     sys      total CPU   CPU/wall   peak RSS
VDPAU + CPU readback         10.566s  7.294s   0.402s   7.696s      72.8%      235.9 MiB
async GLX/Vulkan bridge      10.630s  2.118s   0.634s   2.752s      25.9%      239.1 MiB
```

The async bridge therefore used **64.2% less total process CPU time** and **71.0%
less user CPU time**, reducing CPU cost from about 12.83 ms/frame to 4.59
ms/frame. Wall time is intentionally unchanged because playback is clock-paced.
Peak process RSS increased by about 3.2 MiB (1.4%), consistent with retaining
several in-flight bridge resources rather than synchronously recycling one
presentation buffer.

At 1280x720 YUV420, one decoded frame is 1,382,400 bytes. Over 600 frames the old
path therefore transfers about 829.4 MB GPU->CPU and another 829.4 MB CPU->GPU
before counting CPU-side NV12 deinterleave, tight-plane copies, or any internal
wgpu staging. The async bridge removes that host/device round trip; it still
performs the GL YUV->RGBA render plus one GPU-local Vulkan image copy, so this
remains zero-CPU-copy rather than literal zero-copy.

`oxideplay` now carries leases across `ChannelSink`, `EngineMsg`, its timed video
queue, `OutputDriver`, and `VideoEngine`. Ordinary `Owned` CPU video frames are
borrowed directly by legacy video engines with the original pixel-buffer address;
no copy is introduced at presentation. Arena/hardware frames reach the concrete
video-engine boundary unchanged; a renderer that is not lease-aware then invokes
the explicit materialisation fallback.

### CPU-copy status and measurement

Software H.264 is now a genuine native `FrameLease::ArenaVideo` producer. The
normal frame-coded path reconstructs and deblocks directly into final-format
pooled `u8`/little-endian `u16` planes, freezes the allocation once, and retains
the exact same arena frame in both `RefPicStore` and the output lease. A decoder
regression compares the two luma pointers directly.

`d8ca4c2` removes the remaining automatic `Owned` output escapes from software
H.264. PAFF still needs an assembly copy for a complementary field pair: top and
bottom half-height pictures are interleaved into a reusable full-frame assembly
arena, then that arena is frozen and emitted directly as `ArenaVideo`. An orphaned
field needs no output copy and simply freezes its existing picture allocation.
`separate_colour_plane_flag` likewise keeps the three monochrome sub-decoder
outputs as arena leases and copies their plane bytes directly into one reusable
three-plane assembly arena; the merged public output is `ArenaVideo` rather than a
legacy `VideoFrame`. These are copy-on-assembly cases, not materialisation to the
legacy heap representation.

The decoder owns a lazily allocated 32-frame picture arena pool plus a separate
reusable 32-slot assembly pool for PAFF/SCP output. Normal producer/consumer
playback reuses those buffers when the DPB and downstream leases release them.
`94de8f2` first made temporary pool saturation participate in that back-pressure
model instead of turning a full pool into a slice-level `ResourceExhausted` failure.
`4fe137c`, `e47b459`, and `3f2ce28` complete that contract. `oxideav-core` now exposes
a monotonic `CancellationToken`, `Error::Cancelled`, and
`ArenaPool::lease_wait_cancellable()`. Cancellation explicitly wakes registered
condition-variable waiters; it does not depend on a polling timeout. The staged
executor supplies its shared cancellation token to decoders and cancels it on abort,
so a decoder blocked inside `send_packet()` wakes immediately and the worker unwinds
as a clean stop. `ResourceExhausted` is treated as a hard decoder failure rather than
a recoverable skipped packet.

Software H.264 additionally prevents a blocking wait from deadlocking on resources
that only the decoder itself can release. Sync arenas expose opaque stable allocation
identities (pool generation plus allocation identity), and before waiting on a full
picture or PAFF/SCP assembly pool H.264 deduplicates every arena retained by its own
current picture, reference store, output DPB, ready queue, pending field and SCP
internal queues. If those decoder-owned identities cover the whole full pool, waiting
cannot make progress and the decoder returns `ResourceExhausted` immediately with a
self-deadlock diagnostic. If at least one full-pool arena is external-only, the wait
is legitimate downstream back-pressure and continues until that lease is released or
cancellation is requested. Without a cancellation token the decoder refuses an
indefinite saturated-pool wait. Pool generation identities remain stable even when a
decoder replaces its pool while old frame leases survive.

The non-blocking `Picture::new_in()` path remains available to callers that need it;
no already-bumped arena is converted into `Owned` storage merely to free a slot, and
`reset()` still detaches fresh pools from pre-reset decode state. Regressions cover
downstream-release wakeup, cancellation wakeup while the retained arena remains
checked out, picture-pool self-deadlock, assembly-pool self-deadlock, replacement-pool
identity non-aliasing, and pipeline abort of a decoder parked in an arena wait. The
validated library suites pass with 292 `oxideav-core`, 79 `oxideav-pipeline`, and
1,354 `oxideav-h264` tests, with Clippy warnings denied.

`oxideplay` transports `ArenaVideo` unchanged through `ChannelSink`, `EngineMsg`,
the timed video queue and `OutputDriver` to the concrete `VideoEngine` boundary.
`d6a6dda` adds a pointer-identity regression for the arena `ChannelSink` path and
debug diagnostics for arena-backed frames. A renderer that is not lease-aware
still materialises at its final presentation boundary; the player/pipeline itself
does not copy the frame.

A muted end-to-end software-H.264 regression deliberately disabled VDPAU and
used the hash renderer:

```text
OXIDEPLAY_SINK_DEBUG=1 VDPAU_DRIVER=__oxideav_force_software__ DISPLAY= \
  target/debug/oxideplay <local-720p-h264-aac-fixture> \
  --vo hash --ao none
```

The sink reported all 96 video frames as native arena output (for example
`arena=Yuv420P 1280x720`) and the final hash remained
`2649ac8f78fdf85b`. The hash engine intentionally materialises at the final
renderer boundary because it hashes CPU bytes.

`b5caf78` makes oxideplay's `WinitVideoEngine` lease-aware for native
arena-backed `Yuv420P`. `VideoRenderer::render_arena()` validates the arena
geometry/strides and passes the original borrowed Y/U/V slices directly to
`wgpu::Queue::write_texture()` using each arena plane's real `bytes_per_row`.
There is no `FrameLease::materialize()`, `VideoFrame` allocation, `plane_tight()`
or other full-frame CPU repack on this fast path. Unsupported formats,
high-bit-depth conversion and sources that must be downscaled still use the
existing materialisation/conversion fallback. `Queue::write_texture()` may stage
or copy the supplied bytes internally as part of the CPU-to-GPU transfer; the
claim here is zero additional full-frame CPU copies in OxideAV/oxideplay before
that upload call.


The direct path was exercised on GhostBSD with software H.264 forced and audio
disabled:

```text
OXIDEPLAY_SINK_DEBUG=1 VDPAU_DRIVER=__oxideav_force_software__ \
  target/debug/oxideplay <local-720p-h264-aac-fixture> \
  --vo winit --ao none
```

The winit+wgpu backend initialised on the GTX 1080/Vulkan path, the sink reported
96/96 frames as `arena=Yuv420P 1280x720`, playback completed with exit code 0,
and wgpu reported no validation/upload errors.

A temporary counting-global-allocator probe measured the first 96 access units of
`<local-hls-h264-fixture>`, draining after every AU so the normal bounded-pool reuse path
was exercised. File reading and AU packetisation were completed before allocator
counters were enabled. Fresh `alloc`/`alloc_zeroed` traffic was:

```text
H.264 revision                         frames   alloc calls   allocated bytes
pre-compact a44bbc5                    96       10,196,722    2,970,052,050
compact/separate 94b6372               96       10,197,118    2,348,081,482
shared-arena fda3143                   96       10,195,487    1,992,196,947
```

Thus compact storage removed about 593.2 MiB / 20.9% of fresh allocation traffic
from this decode, and shared DPB/output arenas removed a further 339.4 MiB / 15.2%.
Relative to the pre-compact decoder, the current path requested about 932.6 MiB /
32.9% fewer fresh allocation bytes. Reallocation traffic was essentially unchanged
between `94b6372` and `fda3143` (difference only 14,272 bytes), so it does not
explain the reduction. The stage-3 change also eliminated 1,631 allocation calls
over these 96 pictures.

Two repeated RSS measurements for the direct software-decode probe gave
`63,372` and `67,056` KiB at `94b6372` versus `43,380` and `39,136` KiB at
`fda3143`: mean peak RSS fell from 65,214 to 41,258 KiB, about 23.4 MiB / 36.7%.
One pre-compact `a44bbc5` run peaked at 78,636 KiB. Wall-clock timings from this
debug/counting-allocator probe are intentionally not used as performance claims.

The first 96 access units contain 54 reference pictures and 42 non-reference
pictures. At `94b6372` the normal decoder therefore performed 96 full-picture
copies to create output `VideoFrame`s plus 54 full-picture pixel copies for
`RefPicStore`. At `fda3143` both decoder-side copy classes are zero: reference
storage and output retain the same arena allocation. Before `b5caf78`, the wgpu
boundary then added 96 arena-materialisation copies plus 96 `plane_tight` copies.
`b5caf78` removes both renderer-side classes for native arena `Yuv420P`, so the
normal 96-frame path now performs **zero full-picture CPU copies from completed
H.264 reconstruction through the `Queue::write_texture()` call**. Relative to the
`94b6372` pre-stage-3 wgpu path, that is 246 → 0 full-picture CPU copy events
(96 decoder-output + 54 DPB-reference + 96 renderer repack). Relative to the
immediately preceding `fda3143` arena path, it is 192 → 0 (96 materialisations +
96 renderer repacks). The only remaining transfer on the fast path is
wgpu's upload/staging of the borrowed arena bytes into GPU texture storage.

The earlier player-only copy measurement remains useful for ordinary heap-backed
CPU decoders. Before `9c2f497`, `ChannelSink::write_frame(&Frame)` deep-cloned the
decoded frame before `EngineMsg`; the lease path removes that copy. On the same
96-frame 1280x720 software-H.264 fixture:

```text
                         old deep copy        lease transport
3-run mean wall time       38.58 s              38.24 s
3-run mean max RSS       132063 KiB           121851 KiB
```

The isolated queue-boundary benchmark moved 10,000 720p I420 frames (13.824 GB of
potential payload): the old deep-clone operation averaged 466.1 ms versus 5.59 ms
for `Arc` retention, about 83x faster for that boundary operation. This is not an
83x playback-speed claim; it quantifies the eliminated copy only.

A separate investigation checked the long
`integration_b_rate_control::b_rate_control_holds_the_rd_curve` test after the
arena change because a package-wide test command exceeded the Remote Control
timeout. The pre-stage-3 `94b6372` test completed in 157.55 s. Current isolated
runs completed successfully at 192.14 s, 162.22 s and, with temporary timing
instrumentation, 150.03 s; the desktop was under variable unrelated CPU load.
The instrumented run showed the four 60-frame `run_b_session()` encoder phases at
37.034, 36.853, 36.997 and 37.636 s, while the corresponding `decode_own()` calls
were only 0.491, 0.364, 0.310 and 0.329 s. The test is therefore ~99% encoder
work and shows no stage-3 decoder hang/regression. The earlier umbrella timeout
also covered multiple expensive rate-control tests; additionally, a Remote
Control timeout left a child Cargo/test process alive, temporarily causing a
build-directory lock until it was identified.

A muted real-hardware regression on the GhostBSD GTX 1080 used:

```text
oxideplay <local-720p-h264-aac-fixture> --vo hash --ao none
```

The sink observed 96/96 video frames as `hardware=vdpau 1280x720`; the final hash
was `2649ac8f78fdf85b`. Thus the hardware decoder → pipeline → oxideplay queue
path is GPU-resident. The hash engine intentionally performs the final readback;
a future wgpu lease-aware video engine can instead import or sample the live
VDPAU surface.

## Native AAC optimisation

Components:

```text
38a8443 perf(aac): replace direct IMDCT with fast transform
26f4127 fix(aac): decode non-shared CPE channels independently
b760012 fix(aac): expose decoded PCM output parameters
72ef547 feat(codec): expose decoder output parameters
a0c9d78 fix(pipeline): prefer decoder output parameters
7c4e895 fix(aac): preserve decoded timing and output format
b105819 feat(pipeline): publish decoded stream updates
```

The old AAC filterbank evaluated the specification's IMDCT literally with a
nested cosine sum: O(N²), with `cos()` in the inner loop. Release-mode native
AAC decoded only about 0.38× real time on the initial benchmark.

OxideAV already contained an O(N log N) MDCT/IMDCT factorisation in its own
Vorbis implementation. AAC now uses the same OxideAV-native mathematical shape,
specialised to its f64 working precision, with no external DSP/AAC framework.
The direct AAC implementation remains under tests as the independent numerical
oracle; the fast transform agrees below roughly `2e-13` absolute error on the
power-of-two AAC sizes. Non-radix-2 geometries keep the direct fallback.

Measured validation after the change:

```text
Big Buck short baseline: 0.640 s audio in ~0.0095 s  ≈67× realtime
Big Buck full fixture:   9.195 s audio in ~0.147 s   ≈63× realtime
Twitch AAC segment:     10.027 s audio in ~0.254 s   ≈39.5× realtime
```

This eliminated the justification for the temporary
`oxideav-symphonia-aac` adapter. That crate/dependency was removed; the native
OxideAV AAC decoder is the intended path.

The Twitch fixture also exposed a separate correctness bug: a
`common_window == 0` CPE can legally carry independent left/right `ics_info`
and different window geometry. Those channels now run the independent channel
reconstruction path; a geometry mismatch remains invalid when joint-stereo
syntax actually requires band-for-band pairing.

## Decoder output parameters

Containers describe the **compressed** stream and may not know the final
decoder output shape early enough to configure a sink. MPEG-TS AAC is the
concrete example: initial parameters can be incomplete/stale while the decoder
learns the real PCM rate/channels from the elementary stream.

`oxideav_core::Decoder::output_params()` therefore lets a decoder expose its
uncompressed output parameters. `7c4e895` also removes the AAC runtime decoder's
old 44.1 kHz/stereo guesses: unknown values remain unknown until the elementary
stream establishes the real PCM shape. The same change advances PTS correctly
when one packet contains multiple AAC access units, using each decoded frame's
sample count/rate in the packet time base.

`b105819` adds an ordered `JobSink::stream_update()` path for direct decode-to-sink
routes. The staged decoder checks `output_params()` after `send_packet()` and
forwards a changed decoded format before any frame produced with it. Routes with
frame transforms or encoders keep their existing fixed sink-facing parameters,
since those stages may change the shape themselves.

Embedding applications can use that update to defer sysaudio creation until rate,
channels and sample format are authoritative. A muted real Twitch/OSS smoke
observed provisional AAC metadata with unknown rate/channels, then a 48 kHz
stereo S16 stream update, followed by OSS opening at 48 kHz. This resolves the
previous 44.1 kHz fallback mismatch.

## Current framework follow-ups

1. Import/sample hardware `FrameLease` surfaces directly in wgpu/Vulkan consumers
   without final CPU materialisation.
2. HLS discontinuities, byte ranges, fMP4/MAP, encryption and live/ABR support as
   required by real sources.
