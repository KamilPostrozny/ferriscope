//! `FERRISCOPE_WSI_HOLD`: which presents ferriscope-wsi holds until the
//! compositor's next tick.
//!
//! ferriscope-wsi compiles this same file (`#[path]`), so the layer and an
//! embedder deciding from an application's environment whether the layer will
//! hold it read the variable the same way.

/// Which applications ferriscope-wsi holds at the end of `vkQueuePresentKHR`
/// until the compositor can take their next frame in time
/// (`ferriscope_swapchain_v1.frame_ready`), so each loop iteration, input
/// included, starts right after a tick and the application renders one frame
/// per tick on the compositor's frame clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WsiHold {
	/// Every present mode (the default): an application with VSync off renders
	/// one frame per tick instead of unbounded frames only the newest of which
	/// is shown.
	Every,
	/// FIFO only (`fifo`): MAILBOX and IMMEDIATE render unbounded.
	Fifo,
	/// No application (`off`, `0`, `false`).
	Off,
}

impl WsiHold {
	/// The environment variable that selects the hold.
	pub const VARIABLE: &str = "FERRISCOPE_WSI_HOLD";

	/// The hold a `FERRISCOPE_WSI_HOLD` value selects: `fifo`, or `off` (also
	/// `0`, `false`); anything else, unset included, holds every present mode.
	pub fn from_value(value: Option<&str>) -> Self {
		match value {
			Some("fifo") => Self::Fifo,
			Some("off" | "0" | "false") => Self::Off,
			_ => Self::Every,
		}
	}
}
