# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Initial implementation.
- `CompositorHandles::frame_clock`: an embedder steers the frame-callback grid with bounded `FrameClockCorrection`s (a period within 1% of nominal and a relative phase shift of at most half a period, applied at most 250 µs per frame). Without a correction for one second the grid goes back to the nominal period.
- `ferriscope-wsi` holds an application at the end of `vkQueuePresentKHR` until the compositor's next tick in every present mode, not only FIFO: a game with VSync off (MAILBOX, IMMEDIATE) renders one frame per tick on the steered frame clock instead of unbounded. `FERRISCOPE_WSI_HOLD=fifo` holds FIFO only (the old behaviour), `FERRISCOPE_WSI_HOLD=off` nothing; it replaces `FERRISCOPE_WSI_PACING`. `ferriscope_swapchain_v1` version 2 sends `frame_ready` at every tick for non-FIFO swapchains; the layer holds those only on a version 2 compositor.
