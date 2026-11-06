//! `FERRISCOPE_WSI_HOLD`: which presents ferriscope-wsi holds until the
//! compositor's next tick.
//!
//! ferriscope-wsi compiles this same file (`#[path]`), so the layer and an
//! embedder deciding from an application's environment whether the layer will
//! hold it read the variable the same way.

use std::fmt;

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

/// A `FERRISCOPE_WSI_HOLD` value other than `fifo`, `off`, `0` and `false`
/// (`FIFO`, `Off`, `no`, ...).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnrecognisedHold(pub String);

impl fmt::Display for UnrecognisedHold {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(
			f,
			"{}={:?} is not one of fifo, off, 0, false",
			WsiHold::VARIABLE,
			self.0
		)
	}
}

impl std::error::Error for UnrecognisedHold {}

impl WsiHold {
	/// The environment variable that selects the hold.
	pub const VARIABLE: &str = "FERRISCOPE_WSI_HOLD";

	/// The hold a `FERRISCOPE_WSI_HOLD` value selects: unset or empty holds
	/// every present mode, `fifo` FIFO only, and `off` (also `0`, `false`)
	/// nothing. Any other value is an error, never "hold everything": it was
	/// set to opt an application out, and a typo must not leave it held.
	pub fn parse(value: Option<&str>) -> Result<Self, UnrecognisedHold> {
		match value {
			None | Some("") => Ok(Self::Every),
			Some("fifo") => Ok(Self::Fifo),
			Some("off" | "0" | "false") => Ok(Self::Off),
			Some(other) => Err(UnrecognisedHold(other.to_string())),
		}
	}
}
