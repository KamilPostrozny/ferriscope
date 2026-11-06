//! Steering the session's frame clock from the embedder.
//!
//! Frame callbacks (and the FIFO barriers and presentation feedback that go
//! with them) keep one session-rate grid (see `shell::schedule`). Every client
//! in the session renders on that grid and is delivered on commit, so moving
//! the grid moves when the session's frames exist. An embedder that knows when
//! its consumer wants frames (Moonshine, from a client's display timing) steers
//! the grid with [`FrameClockCorrection`]s: a period and a relative phase
//! shift, both bounded. Without a correction for [`CORRECTION_TIMEOUT`] the
//! grid goes back to the nominal period, so a consumer that stops steering
//! never leaves the session on stale timing.

use std::sync::{
	Arc,
	atomic::{AtomicU32, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use smithay::reexports::calloop;

use crate::{Error, Result};

/// How far a corrected period may be from the nominal one (`1 / rate`), as a
/// fraction of it. A display's real refresh differs from its nominal rate by
/// hundredths of a percent; this leaves room for that and for the controller's
/// period moves, and no more.
pub const MAX_PERIOD_DEVIATION: f64 = 0.01;

/// How far one frame's callback may move from the grid while a phase shift is
/// applied. A larger shift is spread over the next frames, so a client paced on
/// the grid never sees one interval change by more than this.
pub const MAX_PHASE_STEP: Duration = Duration::from_micros(250);

/// Without a correction for this long, the grid goes back to the nominal period
/// and drops any phase shift not yet applied.
pub const CORRECTION_TIMEOUT: Duration = Duration::from_secs(1);

/// One correction to the session's frame clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameClockCorrection {
	/// The grid's period from now on, within [`MAX_PERIOD_DEVIATION`] of the
	/// nominal period.
	pub period: Duration,
	/// How far to move the next frame callbacks, in nanoseconds (positive:
	/// later), at most half the nominal period. It adds to any shift not yet
	/// applied, and is applied at most [`MAX_PHASE_STEP`] per frame.
	pub phase_shift_ns: i64,
}

impl FrameClockCorrection {
	/// Whether the correction is within its bounds for a session whose nominal
	/// period is `nominal`.
	pub fn check(&self, nominal: Duration) -> Result<()> {
		let deviation = self.period.abs_diff(nominal);
		if self.period.is_zero() || deviation > nominal / 100 {
			return Err(Error::FrameClock(format!(
				"period {:?} is more than {}% from the nominal {nominal:?}",
				self.period,
				MAX_PERIOD_DEVIATION * 100.0
			)));
		}
		if self.phase_shift_ns.unsigned_abs() > max_phase_shift_ns(nominal) {
			return Err(Error::FrameClock(format!(
				"phase shift {} ns is more than half the nominal period {nominal:?}",
				self.phase_shift_ns
			)));
		}
		Ok(())
	}
}

/// The largest phase shift, per correction and waiting to be applied in total:
/// half the nominal period (a larger shift is a smaller one the other way).
pub(crate) fn max_phase_shift_ns(nominal: Duration) -> u64 {
	(nominal.as_nanos() / 2) as u64
}

/// Clonable handle that steers a launched compositor's frame clock.
///
/// Wraps the internal calloop channel so no calloop type appears in the public
/// API.
#[derive(Clone)]
pub struct FrameClockSender {
	sender: calloop::channel::Sender<FrameClockCommand>,
	acknowledgements: Arc<Acknowledgements>,
	nominal: Duration,
}

/// Which consumer steers the clock, and the last correction installed.
///
/// Corrections carry the consumer that sent them, so one installed for an
/// earlier consumer is never reported to a later one, even when it is installed
/// while the consumer changes.
#[derive(Default)]
struct Acknowledgements {
	consumer: AtomicU32,
	/// The consumer in the high half, its sequence in the low half.
	applied: AtomicU64,
}

impl Acknowledgements {
	fn consumer(&self) -> u32 {
		self.consumer.load(Ordering::Acquire)
	}

	fn last_applied(&self) -> u32 {
		let applied = self.applied.load(Ordering::Acquire);
		if (applied >> 32) as u32 == self.consumer() {
			applied as u32
		} else {
			0
		}
	}
}

impl FrameClockSender {
	pub(crate) fn new(sender: calloop::channel::Sender<FrameClockCommand>, nominal: Duration) -> Self {
		Self {
			sender,
			nominal,
			acknowledgements: Arc::default(),
		}
	}

	/// The session's nominal period (`1 / rate`).
	pub fn nominal_period(&self) -> Duration {
		self.nominal
	}

	/// The sequence last installed by the compositor thread for the current
	/// consumer (zero before any). Queuing a correction does not acknowledge it.
	pub fn last_applied(&self) -> u32 {
		self.acknowledgements.last_applied()
	}

	/// Start steering for a new consumer, whose sequences start over.
	///
	/// Corrections sent before this are no longer installed, and
	/// [`Self::last_applied`] is zero until one sent after it is.
	pub fn new_consumer(&self) {
		self.acknowledgements.consumer.fetch_add(1, Ordering::AcqRel);
	}

	/// Apply a correction from the next frame callback on.
	///
	/// Fails, and changes nothing, when the correction is out of its bounds
	/// ([`FrameClockCorrection::check`]) or the compositor has shut down. A
	/// correction still queued after [`CORRECTION_TIMEOUT`] is dropped
	/// unapplied: the grid has gone back to its nominal period by then.
	pub fn send(&self, sequence: u32, correction: FrameClockCorrection) -> Result<()> {
		correction.check(self.nominal)?;
		self.sender
			.send(FrameClockCommand {
				correction,
				sequence,
				consumer: self.acknowledgements.consumer(),
				sent_at: Instant::now(),
				acknowledgements: Arc::clone(&self.acknowledgements),
			})
			.map_err(|_| Error::FrameClock("frame clock channel closed".into()))
	}
}

pub(crate) struct FrameClockCommand {
	correction: FrameClockCorrection,
	sequence: u32,
	consumer: u32,
	sent_at: Instant,
	acknowledgements: Arc<Acknowledgements>,
}

impl FrameClockCommand {
	/// The correction to install at `now`; `None` when it was sent for an
	/// earlier consumer, or [`CORRECTION_TIMEOUT`] or longer ago.
	pub(crate) fn installable(&self, now: Instant) -> Option<FrameClockCorrection> {
		let current = self.consumer == self.acknowledgements.consumer();
		let fresh = now.saturating_duration_since(self.sent_at) < CORRECTION_TIMEOUT;
		(current && fresh).then_some(self.correction)
	}

	/// Report the correction installed.
	pub(crate) fn acknowledge(self) {
		let applied = u64::from(self.consumer) << 32 | u64::from(self.sequence);
		self.acknowledgements.applied.store(applied, Ordering::Release);
	}
}

/// The nominal period of a session at `rate` Hz, as the scheduler takes it.
pub(crate) fn nominal_period(rate: f64) -> Duration {
	let rate = if rate.is_finite() { rate.max(1.0) } else { 60.0 };
	Duration::from_secs_f64(1.0 / rate)
}

#[cfg(test)]
mod tests {
	use super::*;

	const NOMINAL: Duration = Duration::from_millis(10);

	fn sender() -> (FrameClockSender, calloop::channel::Channel<FrameClockCommand>) {
		let (tx, rx) = calloop::channel::channel();
		(FrameClockSender::new(tx, NOMINAL), rx)
	}

	fn correction(period_us: u64, phase_shift_us: i64) -> FrameClockCorrection {
		FrameClockCorrection {
			period: Duration::from_micros(period_us),
			phase_shift_ns: phase_shift_us * 1000,
		}
	}

	fn sent(rx: &calloop::channel::Channel<FrameClockCommand>) -> Vec<FrameClockCommand> {
		std::iter::from_fn(|| rx.try_recv().ok()).collect()
	}

	#[test]
	fn corrections_within_the_bounds_go_to_the_compositor() {
		let (sender, rx) = sender();
		// 1% of 10 ms is 100 us; half the period is 5 ms.
		for within in [
			correction(10_000, 0),
			correction(10_100, 5_000),
			correction(9_900, -5_000),
		] {
			sender.send(1, within).expect("within the bounds");
		}
		assert_eq!(sent(&rx).len(), 3);
	}

	#[test]
	fn corrections_outside_the_bounds_are_refused() {
		let (sender, rx) = sender();
		for outside in [
			correction(10_101, 0),
			correction(9_899, 0),
			correction(10_000, 5_001),
			correction(10_000, -5_001),
			correction(0, 0),
		] {
			assert!(
				matches!(sender.send(1, outside), Err(Error::FrameClock(_))),
				"{outside:?} must be refused"
			);
		}
		assert!(sent(&rx).is_empty(), "a refused correction must not reach the clock");
	}

	#[test]
	fn a_correction_to_a_stopped_compositor_fails() {
		let (sender, rx) = sender();
		drop(rx);
		assert!(matches!(
			sender.send(1, correction(10_000, 0)),
			Err(Error::FrameClock(_))
		));
	}

	#[test]
	fn a_queued_correction_is_not_acknowledged_until_installed() {
		let (sender, rx) = sender();
		sender.send(7, correction(10_000, 100)).unwrap();
		assert_eq!(sender.last_applied(), 0);
		rx.try_recv().unwrap().acknowledge();
		assert_eq!(sender.last_applied(), 7);
	}

	#[test]
	fn a_new_consumer_sees_no_acknowledgement_of_an_earlier_ones_corrections() {
		let (sender, rx) = sender();
		sender.send(5, correction(10_000, 100)).unwrap();
		rx.try_recv().unwrap().acknowledge();
		sender.new_consumer();
		assert_eq!(sender.last_applied(), 0);
		sender.send(1, correction(10_000, 100)).unwrap();
		rx.try_recv().unwrap().acknowledge();
		assert_eq!(sender.last_applied(), 1);
	}

	#[test]
	fn a_correction_queued_for_an_earlier_consumer_is_not_installed() {
		let (sender, rx) = sender();
		sender.send(5, correction(10_000, 100)).unwrap();
		sender.new_consumer();
		let command = rx.try_recv().unwrap();
		assert_eq!(command.installable(Instant::now()), None);
		// Installed just before the consumer changed, acknowledged just after.
		command.acknowledge();
		assert_eq!(sender.last_applied(), 0);
	}

	#[test]
	fn a_correction_queued_longer_than_the_timeout_is_not_installed() {
		let (sender, rx) = sender();
		sender.send(3, correction(10_100, -250)).unwrap();
		let command = rx.try_recv().unwrap();
		let sent = Instant::now();
		assert_eq!(command.installable(sent), Some(correction(10_100, -250)));
		assert_eq!(command.installable(sent + CORRECTION_TIMEOUT), None);
	}
}
