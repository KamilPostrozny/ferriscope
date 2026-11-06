# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Initial implementation.
- `CompositorHandles::frame_clock`: an embedder steers the frame-callback grid with bounded `FrameClockCorrection`s (a period within 1% of nominal and a relative phase shift of at most half a period, applied at most 250 µs per frame). Without a correction for one second the grid goes back to the nominal period.

### Fixed

- `FrameClockSender::new_consumer`: a new consumer (the embedder's next client) is no longer acknowledged a correction the previous one sent, and corrections queued for the previous one are dropped.
- A frame-clock correction queued for one second or longer (a stalled compositor) is dropped instead of installed as fresh.
- The `GAMESCOPE_FPS_LIMIT` gate allows one phase step (250 µs) more slack than gamescope's 0.2 ms, so a cap at the session rate no longer skips a callback when the grid is steered earlier.
