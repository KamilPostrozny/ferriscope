# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Initial implementation.
- `CompositorHandles::frame_clock`: an embedder steers the frame-callback grid with bounded `FrameClockCorrection`s (a period within 1% of nominal and a relative phase shift of at most half a period, applied at most 250 µs per frame). Without a correction for one second the grid goes back to the nominal period.
- `ferriscope-wsi` holds an application at the end of `vkQueuePresentKHR` until the compositor's next tick in every present mode, not only FIFO: a game with VSync off (MAILBOX, IMMEDIATE) renders one frame per tick on the steered frame clock instead of unbounded. `FERRISCOPE_WSI_HOLD=fifo` holds FIFO only (the old behaviour), `FERRISCOPE_WSI_HOLD=off` nothing. `ferriscope_swapchain_v1` version 2 sends `frame_ready` at every tick for non-FIFO swapchains bound at version 2 (one bound at version 1 gets it for FIFO only); the layer holds those only on a version 2 compositor.
- `WsiHold`: `FERRISCOPE_WSI_HOLD`'s parser, compiled from the same file by `ferriscope-wsi`, so an embedder reading an application's environment (Moonshine's forced GL sync opt-out) agrees with the layer on which values opt out.

### Changed

- `tools/install-wsi.sh` builds and installs the layer for i686 too (`lib32/ferriscope/libferriscope_wsi.so`, `VkLayer_FERRISCOPE_wsi.i686.json`), so 32-bit Vulkan games and 32-bit DXVK/vkd3d-proton take the Xwayland bypass, HDR and the present hold. It fails, before installing anything, when the `i686-unknown-linux-gnu` Rust target or a 32-bit C toolchain is missing.
- `WsiHold::parse` replaces `WsiHold::from_value`: a `FERRISCOPE_WSI_HOLD` value other than `fifo`, `off`, `0` or `false` (unset or empty still holds every present mode) is an `UnrecognisedHold` error instead of meaning "hold every present mode". `ferriscope-wsi` logs the rejected value as an error and holds nothing, so a mistyped opt-out (`FIFO`, `Off`) can't leave a Game held.

### Removed

- `FERRISCOPE_WSI_PACING`: `FERRISCOPE_WSI_HOLD` replaces it.

### Fixed

- `FrameClockSender::new_consumer`: a new consumer (the embedder's next client) is no longer acknowledged a correction the previous one sent, and corrections queued for the previous one are dropped.
- A frame-clock correction queued for one second or longer (a stalled compositor) is dropped instead of installed as fresh.
- The `GAMESCOPE_FPS_LIMIT` throttle counts session ticks instead of wall time since a window's last requested frame callbacks, so steering the frame clock does not change it: a cap at the session rate or above passes every tick, one at half of it every second tick, and one just under it is enforced (118 fps on a 120 Hz session skips two ticks in every 120).
- A frame-clock phase shift moves delivery with the frame-callback grid in every case. The delivery token clock now slews by each applied phase step, so frames held to the session rate (waiting for a token, not delivered on commit) move with the shift. The change cadence that decides whether a scene is over the session rate is now the median of the last 15 change spacings instead of an average, so a second window committing sparsely (every 500 ms in a 32-bit DXVK game) no longer marks a session-rate game as over the rate. Before, the game lost its token headroom, its frames waited up to an interval for tokens on a clock that phase shifts never moved, and an embedder steering the grid saw delivery ignore its shifts.
- A swapchain recreated on the same surface (a resize, a VSync or fullscreen toggle) gets the session's `refresh_cycle` again. Before, the per-surface "already sent" check kept it from every swapchain after the first, so `ferriscope-wsi` kept its 60 Hz default: in a 120 Hz session it held a Game with 7.8–15.6 ms frames to 60 fps and waited up to 33 ms for `frame_ready`.
- `ferriscope-wsi` counts a present it doesn't hold (the Game's frames too slow for the gate) as answered, so the first held present after an unheld stretch waits for the next tick instead of for the stretch's backlog until the two-refresh budget ran out.
