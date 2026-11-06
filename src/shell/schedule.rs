//! Frame scheduling: when a frame is composited and delivered, and when frame
//! callbacks go out.
//!
//! There is no vblank. A frame leaves as soon as three things hold:
//!
//! 1. the scene is *damaged* (its signature changed since the last delivered
//!    frame, or the static keepalive is due),
//! 2. a pool slot is free (the consumer has released one), and
//! 3. a delivery *token* is available (the session-rate cap).
//!
//! The loop is woken by the events that can make those true: a client commit,
//! a consumer release on a calloop channel, a release fence signalling, or the
//! single timer. [`FerriscopeState::service`] runs after every dispatch, so a
//! slot freed by a release is refilled in the same iteration it was freed, and
//! the timer only ever covers a wait that no event will end: the token refill
//! while damage is pending, the next frame-callback time, or the keepalive.
//!
//! Frame callbacks keep their own session-rate cadence (gamescope parity):
//! suppressing them stalls Xwayland present, so they are independent of
//! delivery.
//!
//! [`Scheduler`] is the pure decision core (no clock, no GPU), unit-tested
//! below.

use std::{
	sync::atomic::Ordering,
	time::{Duration, Instant},
};

use smithay::{
	reexports::calloop::{
		Interest, Mode, PostAction,
		generic::Generic,
		timer::{TimeoutAction, Timer},
	},
	utils::IsAlive,
};

use crate::{
	frame_clock::{
		CORRECTION_TIMEOUT, FrameClockCorrection, MAX_PHASE_STEP, max_phase_shift_ns, nominal_period, session_rate,
	},
	shell::{FerriscopeState, render::RenderOutcome},
};

/// How long a static screen is re-composited as a keepalive.
///
/// Without it a paused screen would emit no frames at all, and a consumer that
/// waits for one could stall.
pub(in crate::shell) const STATIC_KEEPALIVE: Duration = Duration::from_secs(1);

/// Delivery-token capacity, in intervals.
///
/// The rate cap refills about one delivery token per session interval (see
/// [`TOKEN_REFILL_HEADROOM`]) up to this capacity. A sub-rate application always finds a token and is delivered at
/// commit time; a client presenting above the session rate (a game that renders
/// per input event) is held to it. One spare interval absorbs the phase jitter
/// of a session-rate client without letting it pull ahead.
pub(in crate::shell) const TOKEN_CAPACITY: f64 = 2.0;

/// How much faster than the session rate delivery tokens refill while the
/// scene changes no faster than the session rate.
///
/// A client presenting at exactly the session rate consumes tokens as fast as
/// they refill, so a deficit it ever picks up (a hitch, or the phase at which it
/// started) would never drain: every later frame would wait the same fraction
/// of an interval for its token. Refilling slightly faster drains any deficit
/// within a few frames. A scene that changes faster than the session rate (a
/// client committing above it, or pointer motion moving the cursor) gets no
/// headroom (see [`OVER_RATE`]), so it is held to exactly the rate.
const TOKEN_REFILL_HEADROOM: f64 = 1.05;

/// Scene changes closer together than this count as one for the change
/// cadence: the surfaces of one frame (subsurfaces, several windows) commit
/// within a dispatch of each other.
const CHANGE_GROUPING: Duration = Duration::from_micros(500);

/// How many of the latest change spacings the change cadence is the median of.
///
/// A median rather than an average: a second surface that commits sparsely
/// (a launcher or helper window, every few hundred ms) splits one spacing in
/// two, which drags an average of a session-rate scene below [`OVER_RATE`]
/// and takes its headroom away, while the median of a window this long
/// ignores it. A scene that changes faster than the rate on most spacings
/// still reads as over it.
const CADENCE_WINDOW: usize = 15;

/// A change spacing sample is clamped to this many intervals, so an idle gap
/// does not hide a scene that resumes changing above the rate for long.
const CADENCE_SAMPLE_CAP: f64 = 2.0;

/// The change cadence, in intervals, below which the scene counts as changing
/// faster than the session rate. A client paced at the session rate (FIFO on
/// the tick, frame callbacks) runs on ferriscope's own clock and changes the
/// scene exactly one interval apart; the margin keeps measurement noise from
/// taking its headroom away.
const OVER_RATE: f64 = 0.995;

/// Slack on the token comparison, so float rounding in the refill deadline does
/// not wake the loop a hair too early and re-arm for a nanosecond.
const TOKEN_EPSILON: f64 = 1e-6;

/// The scheduler's pure state: when the next frame may be delivered and when the
/// next frame callbacks are due.
#[derive(Debug)]
pub(in crate::shell) struct Scheduler {
	/// The session interval: the frame clock's period, `1 / rate` unless an
	/// embedder's correction set it.
	interval: Duration,
	/// The nominal interval (`1 / rate`), which the clock goes back to when
	/// corrections stop.
	nominal: Duration,
	/// The nominal rate (`1 / nominal`), in Hz.
	rate: f64,
	/// Phase shift still to apply to the frame-callback grid, in nanoseconds
	/// (positive: later); at most [`MAX_PHASE_STEP`] per callback.
	pending_shift_ns: i64,
	/// When the last frame-clock correction arrived; `None` while the clock
	/// runs at its nominal period.
	last_correction: Option<Instant>,
	/// Monotonic count of surface commits observed; folded into the scene
	/// signature so any client commit is a change.
	commit_epoch: u64,
	/// Content signature of the last frame handed to the sink.
	last_scene: Option<u64>,
	/// When the last frame was handed to the sink, for the static keepalive.
	last_frame_at: Instant,
	/// Delivery tokens, refilled at the session rate up to [`TOKEN_CAPACITY`].
	/// Every delivery charges one.
	tokens: f64,
	/// When `tokens` was last refilled.
	last_refill: Instant,
	/// The latest change spacings (grouped by [`CHANGE_GROUPING`]), in
	/// intervals, oldest overwritten first; the first `spacings_len` are set.
	spacings: [f64; CADENCE_WINDOW],
	/// How many of `spacings` hold a spacing.
	spacings_len: usize,
	/// Where the next spacing goes in `spacings`.
	spacings_next: usize,
	/// The median of `spacings`; `None` before the second change.
	cadence: Option<f64>,
	/// The scene signature [`Self::observe`] last saw.
	observed_scene: Option<u64>,
	/// When the last change counted for `cadence` was observed.
	last_change_at: Option<Instant>,
	/// Do not attempt another delivery before this instant.
	///
	/// Set when a frame could not be handed over although a slot was free (the
	/// sink dropped it, or rendering failed), so a stuck consumer is retried at
	/// the session rate instead of spinning.
	retry_at: Option<Instant>,
	/// When the next frame callbacks are due.
	next_callback: Instant,
	/// When the oldest commit not yet delivered arrived.
	pending_since: Option<Instant>,
	/// Epoch of the last delivered frame, for the duplicate diagnostic.
	last_delivery_epoch: Option<u64>,
	/// When the last frame was delivered.
	last_delivery_at: Option<Instant>,
	/// Frames handed to the sink so far; also the next frame's sequence number.
	frames_delivered: u64,
}

/// What one delivery looked like, for the cadence trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::shell) struct Delivery {
	/// From the oldest undelivered commit to the delivery, if there was one.
	pub(in crate::shell) commit_to_delivery: Option<Duration>,
	/// Since the previous delivery.
	pub(in crate::shell) since_last: Option<Duration>,
	/// Whether no commit landed since the previous delivery (a cursor redraw or
	/// a keepalive rather than new content).
	pub(in crate::shell) repeat: bool,
}

impl Scheduler {
	/// A scheduler for a session at `rate` Hz, starting at `now`.
	pub(in crate::shell) fn new(rate: f64, now: Instant) -> Self {
		let interval = nominal_period(rate);
		Self {
			interval,
			nominal: interval,
			rate: session_rate(rate),
			pending_shift_ns: 0,
			last_correction: None,
			commit_epoch: 0,
			last_scene: None,
			last_frame_at: now,
			tokens: TOKEN_CAPACITY,
			last_refill: now,
			spacings: [0.0; CADENCE_WINDOW],
			spacings_len: 0,
			spacings_next: 0,
			cadence: None,
			observed_scene: None,
			last_change_at: None,
			retry_at: None,
			next_callback: now + interval,
			pending_since: None,
			last_delivery_epoch: None,
			last_delivery_at: None,
			frames_delivered: 0,
		}
	}

	/// The session interval: the frame clock's current period.
	pub(in crate::shell) fn interval(&self) -> Duration {
		self.interval
	}

	/// The cycle of a `GAMESCOPE_FPS_LIMIT` of `limit_hz`, in session ticks;
	/// `None` when there is no limit (zero).
	pub(in crate::shell) fn fps_limit_cycle(&self, limit_hz: u32) -> Option<f64> {
		(limit_hz != 0).then(|| self.rate / f64::from(limit_hz))
	}

	/// Apply an embedder's frame-clock correction at `now` (already checked
	/// against its bounds by [`crate::FrameClockSender`]).
	///
	/// The period holds from the next callback after the one already armed; the
	/// phase shift adds to any not yet applied, bounded to half the nominal
	/// period in total.
	pub(in crate::shell) fn correct(&mut self, correction: FrameClockCorrection, now: Instant) {
		self.expire_correction(now);
		// Refill at the period the tokens had so far.
		self.tokens = self.tokens_at(now);
		self.last_refill = now;
		self.interval = correction.period;
		let bound = max_phase_shift_ns(self.nominal) as i64;
		self.pending_shift_ns = self
			.pending_shift_ns
			.saturating_add(correction.phase_shift_ns)
			.clamp(-bound, bound);
		self.last_correction = Some(now);
	}

	/// Whether an embedder's correction steers the frame clock.
	pub(in crate::shell) fn steered(&self) -> bool {
		self.last_correction.is_some()
	}

	/// A new consumer steers from `now`: back to the nominal period, dropping
	/// any phase shift not yet applied, so it does not start on the last
	/// consumer's steering.
	pub(in crate::shell) fn new_consumer(&mut self, now: Instant) {
		self.go_nominal(now);
	}

	/// Go back to the nominal period, dropping any phase shift not yet applied,
	/// once no correction has arrived for [`CORRECTION_TIMEOUT`].
	fn expire_correction(&mut self, now: Instant) {
		if self
			.last_correction
			.is_some_and(|at| now.saturating_duration_since(at) >= CORRECTION_TIMEOUT)
		{
			self.go_nominal(now);
		}
	}

	fn go_nominal(&mut self, now: Instant) {
		// Refill at the period the tokens had so far.
		self.tokens = self.tokens_at(now);
		self.last_refill = now;
		self.interval = self.nominal;
		self.pending_shift_ns = 0;
		self.last_correction = None;
	}

	/// Record one surface commit.
	pub(in crate::shell) fn commit(&mut self, now: Instant) {
		self.commit_epoch = self.commit_epoch.wrapping_add(1);
		self.pending_since.get_or_insert(now);
	}

	/// Note the scene signature at `now`, folding a change into the change
	/// cadence.
	///
	/// Called after every dispatch. Commits are not the only changes: pointer
	/// motion moves the cursor, which is a new frame too, so the cadence follows
	/// the signature rather than the commits.
	pub(in crate::shell) fn observe(&mut self, scene: u64, now: Instant) {
		if self.observed_scene == Some(scene) {
			return;
		}
		self.observed_scene = Some(scene);
		let Some(last) = self.last_change_at else {
			self.last_change_at = Some(now);
			return;
		};
		let gap = now.saturating_duration_since(last);
		if gap < CHANGE_GROUPING {
			return;
		}
		// Refill at the rate the tokens had so far, so the switch does not
		// rewrite the elapsed time.
		self.tokens = self.tokens_at(now);
		self.last_refill = now;
		let sample = (gap.as_secs_f64() / self.interval.as_secs_f64()).min(CADENCE_SAMPLE_CAP);
		self.spacings[self.spacings_next] = sample;
		self.spacings_next = (self.spacings_next + 1) % CADENCE_WINDOW;
		self.spacings_len = (self.spacings_len + 1).min(CADENCE_WINDOW);
		let mut window = self.spacings;
		let window = &mut window[..self.spacings_len];
		let middle = window.len() / 2;
		let (_, median, _) = window.select_nth_unstable_by(middle, f64::total_cmp);
		self.cadence = Some(*median);
		self.last_change_at = Some(now);
	}

	/// Whether the scene changes faster than the session rate.
	fn over_rate(&self) -> bool {
		self.cadence.is_some_and(|cadence| cadence < OVER_RATE)
	}

	/// The commit epoch, folded into the scene signature.
	pub(in crate::shell) fn commit_epoch(&self) -> u64 {
		self.commit_epoch
	}

	/// Frames handed to the sink so far.
	pub(in crate::shell) fn frames_delivered(&self) -> u64 {
		self.frames_delivered
	}

	/// The time one delivery token takes to refill: one interval while the
	/// clients commit faster than the session rate, a little less otherwise.
	fn refill_period(&self) -> Duration {
		if self.over_rate() {
			self.interval
		} else {
			self.interval.div_f64(TOKEN_REFILL_HEADROOM)
		}
	}

	/// Delivery tokens available at `now`, without consuming any.
	fn tokens_at(&self, now: Instant) -> f64 {
		let elapsed = now.saturating_duration_since(self.last_refill);
		(self.tokens + elapsed.as_secs_f64() / self.refill_period().as_secs_f64()).min(TOKEN_CAPACITY)
	}

	/// The earliest instant a frame may be delivered, slots aside.
	///
	/// A damaged scene waits only for a token; an undamaged one waits for the
	/// keepalive. A pending retry holds both back.
	fn ready_at(&self, scene: u64, now: Instant) -> Instant {
		let wanted = if self.last_scene == Some(scene) {
			self.last_frame_at + STATIC_KEEPALIVE
		} else {
			now
		};
		let tokens = self.tokens_at(now);
		let token_ready = if tokens >= 1.0 - TOKEN_EPSILON {
			now
		} else {
			now + self.refill_period().mul_f64(1.0 - tokens)
		};
		wanted.max(token_ready).max(self.retry_at.unwrap_or(now))
	}

	/// Whether the scene at `scene` should be composited and delivered at `now`.
	pub(in crate::shell) fn should_render(&self, scene: u64, slot_free: bool, now: Instant) -> bool {
		slot_free && self.ready_at(scene, now) <= now
	}

	/// The next instant the loop needs to wake on its own.
	///
	/// The earliest of the next frame-callback time and, when a slot is free and
	/// there is a scene to deliver, the instant that scene becomes deliverable
	/// (token refill while damaged, the keepalive otherwise). With no free slot
	/// nothing but a release can deliver, and that is an event, so no deadline is
	/// armed for it.
	pub(in crate::shell) fn next_deadline(&self, scene: Option<u64>, slot_free: bool, now: Instant) -> Instant {
		match scene {
			Some(scene) if slot_free => self.next_callback.min(self.ready_at(scene, now)),
			_ => self.next_callback,
		}
	}

	/// Record a frame handed to the sink at `now`.
	pub(in crate::shell) fn delivered(&mut self, scene: u64, now: Instant) -> Delivery {
		self.tokens = self.tokens_at(now) - 1.0;
		self.last_refill = now;
		self.last_scene = Some(scene);
		self.last_frame_at = now;
		self.retry_at = None;
		self.frames_delivered += 1;
		let delivery = Delivery {
			commit_to_delivery: self
				.pending_since
				.take()
				.map(|since| now.saturating_duration_since(since)),
			since_last: self.last_delivery_at.map(|at| now.saturating_duration_since(at)),
			repeat: self.last_delivery_epoch == Some(self.commit_epoch),
		};
		self.last_delivery_epoch = Some(self.commit_epoch);
		self.last_delivery_at = Some(now);
		delivery
	}

	/// Record that a frame could not be handed over although a slot was free
	/// (the sink dropped it, or rendering failed): retry one interval later.
	pub(in crate::shell) fn deferred(&mut self, now: Instant) {
		self.retry_at = Some(now + self.interval);
	}

	/// Whether frame callbacks are due.
	pub(in crate::shell) fn callbacks_due(&self, now: Instant) -> bool {
		now >= self.next_callback
	}

	/// Record that frame callbacks went out at `now`.
	///
	/// The target advances by one interval from its previous value rather than
	/// from `now`, so the cadence does not drift by the dispatch latency, plus at
	/// most [`MAX_PHASE_STEP`] of a pending phase shift; if the loop overran, it
	/// snaps forward instead of bursting. The delivery token clock moves by the
	/// same step, so a frame that waits for a token (a scene held to the rate)
	/// moves with the Phase shift as one delivered on commit does.
	pub(in crate::shell) fn callbacks_sent(&mut self, now: Instant) {
		self.expire_correction(now);
		let max_step = MAX_PHASE_STEP.min(self.interval / 2).as_nanos() as i64;
		let step = self.pending_shift_ns.clamp(-max_step, max_step);
		self.pending_shift_ns -= step;
		if step != 0 {
			self.tokens = self.tokens_at(now) - step as f64 / self.refill_period().as_nanos() as f64;
			self.last_refill = now;
		}
		let shift = Duration::from_nanos(step.unsigned_abs());
		self.next_callback = if step >= 0 {
			self.next_callback + self.interval + shift
		} else {
			self.next_callback + (self.interval - shift)
		};
		if self.next_callback <= now {
			self.next_callback = now + self.interval;
		}
	}
}

/// Whether the FPS limiter covers a window (`steamcompmgr_window_should_limit_fps`,
/// `steamcompmgr.cpp:1475`): the Steam UI and the overlays are exempt
/// (`window_is_limiter_exempt`, `:1303`). The VR-scene exemptions have no
/// ferriscope counterpart.
pub(in crate::shell) fn limiter_covers(meta: &crate::focus::WindowMeta) -> bool {
	let steam = meta.is_steam_legacy_big_picture || meta.app_id == crate::focus::STEAM_BIG_PICTURE_APP_ID;
	!(steam || meta.is_overlay || meta.is_external_overlay)
}

/// Float slack on the FPS-limit schedule, in ticks: a cycle that is a whole
/// number of ticks is due on that tick even when the sum of cycles rounds a
/// hair above it.
const FPS_LIMIT_EPSILON: f64 = 1e-6;

/// A window's place on the `GAMESCOPE_FPS_LIMIT` schedule.
///
/// gamescope sends a covered window its next frame callbacks one target cycle
/// of wall time after the last requested ones, less 0.2 ms
/// (`steamcompmgr.cpp:6736-6744`), on a fixed vblank. Here ticks are steered
/// (a period up to 1% shorter, a tick up to one phase step earlier), so a wall
/// time slack big enough for that would let every cap within it of the session
/// rate through on every tick. The schedule counts session ticks instead,
/// which steering does not change: a cap at the session rate or above passes
/// every tick, one at half of it every second tick, and one just under it
/// (118 fps on a 120 Hz session) skips a tick as often as averaging the cap
/// needs.
#[derive(Debug, Default, Clone, Copy)]
pub(in crate::shell) struct FpsLimit {
	/// Ticks until the window may receive requested frame callbacks again; due
	/// at zero or less.
	wait: f64,
}

impl FpsLimit {
	/// Advance one session tick; whether the window may receive frame
	/// callbacks on it.
	pub(in crate::shell) fn tick(&mut self) -> bool {
		self.wait -= 1.0;
		self.wait <= FPS_LIMIT_EPSILON
	}

	/// Record that the window received requested frame callbacks on this tick,
	/// under a cap of `cycle` ticks ([`Scheduler::fps_limit_cycle`]).
	///
	/// Less than a tick past due is where the cycle fell between two ticks:
	/// it carries over, so the window averages the cap. More means the window
	/// requested nothing on a due tick, so the cycle starts over, as
	/// gamescope's does from the last requested callbacks.
	pub(in crate::shell) fn sent(&mut self, cycle: f64) {
		self.wait = if self.wait > -1.0 { self.wait + cycle } else { cycle };
	}
}

impl FerriscopeState {
	/// Deliver a frame if one is due, then re-arm the timer.
	///
	/// Runs after every dispatch, so every wake-up (a commit, a consumer
	/// release, a signalled fence, input, the timer) gets an immediate
	/// decision; the timer is only for waits no event ends.
	pub(in crate::shell) fn service(&mut self) {
		let now = Instant::now();
		self.check_cursor_suspension(now);
		let populated = self.space.elements().next().is_some() || !self.overlays.is_empty();
		let slot_free = self.slot_free();
		let scene = populated.then(|| self.scene_signature());

		if let Some(scene) = scene {
			self.scheduler.observe(scene, now);
			if self.scheduler.should_render(scene, slot_free, now) {
				let render_started = Instant::now();
				match self.render_frame() {
					Ok(RenderOutcome::Delivered { zero_copy }) => {
						let delivery = self.scheduler.delivered(scene, now);
						// Latency tooling: `sequence` is the delivered frame's
						// `FrameInfo::sequence`.
						tracing::trace!(
							target: "ferriscope::latency",
							sequence = self.scheduler.frames_delivered() - 1,
							"delivered"
						);
						// The frame is with the consumer now: that is when
						// its commits count as presented.
						self.send_presentation_feedback(self.scheduler.frames_delivered() - 1, zero_copy);
						tracing::trace!(
							target: "ferriscope::cadence",
							commit_to_delivery_us = delivery.commit_to_delivery.map(|d| d.as_micros() as u64),
							render_us = render_started.elapsed().as_micros() as u64,
							since_last_us = delivery.since_last.map(|d| d.as_micros() as u64),
							repeat = delivery.repeat,
							sequence = self.scheduler.frames_delivered() - 1,
							"delivered"
						);
					},
					// No pool slot was free; the scene is still dirty.
					Ok(RenderOutcome::Skipped) => {
						tracing::trace!(target: "ferriscope::cadence", "no free slot; retrying");
						self.scheduler.deferred(now);
					},
					Err(e) => {
						tracing::warn!("render: {e}");
						self.scheduler.deferred(now);
					},
				}
			}
		}

		if let Some(max) = self.stop_after_frames
			&& self.scheduler.frames_delivered() >= max
		{
			tracing::info!("reached frame limit ({max}), stopping");
			self.request_stop();
		}

		let slot_free = self.slot_free();
		let scene = scene.map(|_| self.scene_signature());
		let now = Instant::now();
		let mut deadline = self.scheduler.next_deadline(scene, slot_free, now);
		if let Some(hide_at) = self.cursor_suspension.deadline(now) {
			deadline = deadline.min(hide_at);
		}
		self.arm_timer(deadline);
	}

	/// Make the single timer fire no later than `deadline`.
	///
	/// A timer already armed for an earlier instant is left alone: it wakes the
	/// loop, finds nothing due and [`Self::service`] re-arms it. A later one is
	/// replaced.
	fn arm_timer(&mut self, deadline: Instant) {
		if let Some((token, armed)) = self.timer.take() {
			if armed <= deadline {
				self.timer = Some((token, armed));
				return;
			}
			self.loop_handle.remove(token);
		}
		let timer = Timer::from_deadline(deadline);
		match self.loop_handle.insert_source(timer, |_, _, state: &mut Self| {
			state.timer = None;
			state.on_timer();
			// `service` runs right after this dispatch and arms the next one.
			TimeoutAction::Drop
		}) {
			Ok(token) => self.timer = Some((token, deadline)),
			Err(e) => tracing::error!("arm frame timer: {e:?}"),
		}
	}

	/// The consumer corrected the frame clock (see [`crate::frame_clock`]).
	pub(in crate::shell) fn correct_frame_clock(&mut self, correction: FrameClockCorrection) {
		if !self.scheduler.steered() {
			tracing::info!(
				period_ns = correction.period.as_nanos() as u64,
				"frame clock: steered by the consumer"
			);
		}
		tracing::trace!(
			target: "ferriscope::latency",
			period_ns = correction.period.as_nanos() as u64,
			phase_shift_ns = correction.phase_shift_ns,
			"frame clock correction"
		);
		self.scheduler.correct(correction, Instant::now());
	}

	/// A new consumer steers the frame clock from here (see
	/// [`crate::FrameClockSender::new_consumer`]).
	pub(in crate::shell) fn new_frame_clock_consumer(&mut self) {
		if self.scheduler.steered() {
			tracing::info!("frame clock: new consumer; back to the nominal period");
		}
		self.scheduler.new_consumer(Instant::now());
	}

	/// The timer fired: the housekeeping and frame callbacks that run at the
	/// session rate.
	fn on_timer(&mut self) {
		let now = Instant::now();
		if !self.scheduler.callbacks_due(now) {
			return;
		}
		let steered = self.scheduler.steered();
		self.scheduler.callbacks_sent(now);
		if steered && !self.scheduler.steered() {
			tracing::info!("frame clock: no correction for {CORRECTION_TIMEOUT:?}; back to the nominal period");
		}

		self.space.refresh();
		self.popups.cleanup();
		// Overlays live outside `space`, so `space.refresh` does not reclaim
		// them; drop any whose client disconnected without an unmap callback.
		self.overlays.retain(|overlay| overlay.window.alive());
		// A Wayland window destroyed by its client has no xwm callback; clear
		// focus so it cannot dangle on the dead surface.
		self.clear_dead_focus();
		// Apply any focus change marked dirty since the last pass (gamescope
		// runs `DetermineAndApplyFocus` in its main loop when the serial is
		// dirty).
		self.determine_and_apply_focus();

		// Frame callbacks go out every interval regardless of delivery, and
		// whether anything is drawn, as gamescope's vblank does: clients keep
		// presenting (Xwayland present stalls without them).
		// Latency tooling: the session-rate tick (frame callbacks, FIFO).
		tracing::trace!(target: "ferriscope::latency", "tick");
		let ticked = self.send_frame_callbacks();
		// The callback tick emulates a vblank at the session rate, which is
		// what releases FIFO barriers: independent of delivery, so a hidden
		// window or a stalled consumer does not hold a client.
		self.release_fifo_barriers(&ticked);
		// Windows that are not drawn get their presentation feedback from the
		// same vblank; drawn ones get it at delivery.
		self.send_undrawn_presentation_feedback();
		self.warn_stale_fences();
		if self.child.is_some() && !self.child_watched {
			self.reap_child();
		}
	}

	pub(in crate::shell) fn post_dispatch(&mut self) {
		// The event loop resets its own stop flag when it starts, so an
		// external stop that raced setup is honoured from here.
		if self.external_stop.load(Ordering::Acquire) {
			self.request_stop();
			return;
		}

		self.space.refresh();
		self.popups.cleanup();
		// Apply any focus change marked dirty during dispatch before flushing, so
		// a Wayland configure queued by the pass is flushed in the same batch
		// (gamescope runs its focus pass in the main loop when the serial is
		// dirty).
		self.determine_and_apply_focus();
		if let Err(e) = self.display_handle.flush_clients() {
			tracing::warn!("flush_clients: {e:?}");
		}

		self.service();
	}

	/// Watch the hosted child with a pidfd, so its exit wakes the loop instead of
	/// being polled with `waitpid` after every dispatch.
	///
	/// If the pidfd cannot be created the child is still reaped, from the
	/// frame-callback timer.
	pub(in crate::shell) fn watch_child(&mut self) {
		let Some(child) = self.child.as_ref() else {
			return;
		};
		let Some(pid) = smithay::reexports::rustix::process::Pid::from_raw(child.id() as i32) else {
			return;
		};
		let pidfd = match smithay::reexports::rustix::process::pidfd_open(
			pid,
			smithay::reexports::rustix::process::PidfdFlags::empty(),
		) {
			Ok(pidfd) => pidfd,
			Err(e) => {
				tracing::warn!("pidfd_open: {e}; polling the hosted application instead");
				return;
			},
		};
		let source = Generic::new(pidfd, Interest::READ, Mode::Level);
		match self.loop_handle.insert_source(source, |_, _, state: &mut Self| {
			state.reap_child();
			Ok(PostAction::Remove)
		}) {
			Ok(_) => self.child_watched = true,
			Err(e) => tracing::warn!("watch hosted application: {e:?}; polling it instead"),
		}
	}

	/// Terminate the hosted child on shutdown: SIGTERM, then SIGKILL if it has
	/// not exited within [`CHILD_TERM_GRACE`]. A no-op when it already exited.
	pub(in crate::shell) fn terminate_child(&mut self) {
		let Some(mut child) = self.child.take() else {
			return;
		};
		self.child_watched = false;
		if matches!(child.try_wait(), Ok(Some(_))) {
			return;
		}
		if let Some(pid) = smithay::reexports::rustix::process::Pid::from_raw(child.id() as i32) {
			let _ = smithay::reexports::rustix::process::kill_process(
				pid,
				smithay::reexports::rustix::process::Signal::TERM,
			);
		}
		let deadline = Instant::now() + CHILD_TERM_GRACE;
		while Instant::now() < deadline {
			if matches!(child.try_wait(), Ok(Some(_))) {
				tracing::info!("hosted application terminated");
				return;
			}
			std::thread::sleep(Duration::from_millis(10));
		}
		tracing::warn!("hosted application ignored SIGTERM; killing it");
		let _ = child.kill();
		let _ = child.wait();
	}

	/// Collect the hosted child's exit status and stop, if it has exited.
	fn reap_child(&mut self) {
		let Some(child) = self.child.as_mut() else {
			return;
		};
		match child.try_wait() {
			Ok(Some(status)) => {
				tracing::info!(?status, "hosted application exited");
				self.child = None;
				self.child_watched = false;
				self.request_stop();
			},
			Ok(None) => {},
			Err(e) => tracing::warn!("wait: {e}"),
		}
	}
}

/// How long the hosted child gets to exit after SIGTERM before it is killed.
const CHILD_TERM_GRACE: Duration = Duration::from_secs(2);

#[cfg(test)]
mod tests {
	use super::*;

	/// 100 Hz keeps the arithmetic round: a 10 ms interval.
	const RATE: f64 = 100.0;
	const INTERVAL: Duration = Duration::from_millis(10);

	fn ms(n: u64) -> Duration {
		Duration::from_millis(n)
	}

	fn scheduler() -> (Scheduler, Instant) {
		let t0 = Instant::now();
		(Scheduler::new(RATE, t0), t0)
	}

	#[test]
	fn the_first_frame_is_due_immediately() {
		let (s, t0) = scheduler();
		assert!(s.should_render(1, true, t0));
	}

	#[test]
	fn an_unchanged_scene_is_not_re_composited_until_the_keepalive() {
		let (mut s, t0) = scheduler();
		s.delivered(7, t0);
		assert!(
			!s.should_render(7, true, t0 + ms(500)),
			"unchanged scene must not repeat"
		);
		assert!(s.should_render(8, true, t0 + ms(500)), "a changed scene must compose");
		assert!(
			s.should_render(7, true, t0 + STATIC_KEEPALIVE + ms(1)),
			"a static screen keeps alive"
		);
		assert_eq!(
			s.next_deadline(Some(7), true, t0 + ms(5)),
			s.next_callback,
			"the callback comes before the keepalive"
		);
	}

	#[test]
	fn no_free_slot_means_no_delivery_and_no_deadline_for_one() {
		let (mut s, t0) = scheduler();
		s.delivered(1, t0);
		s.delivered(2, t0 + INTERVAL);
		let now = t0 + 3 * INTERVAL;
		assert!(!s.should_render(3, false, now));
		// Only a release can end the wait, and that is an event: the only
		// deadline left is the callback tick.
		assert_eq!(s.next_deadline(Some(3), false, now), s.next_callback);
	}

	#[test]
	fn slot_free_retries_a_pending_commit_immediately() {
		// Damage arrives while every slot is busy: nothing is due. The instant a
		// slot frees (no clock advance at all), the same state renders.
		let (mut s, t0) = scheduler();
		s.delivered(1, t0);
		let now = t0 + ms(30);
		s.commit(now);
		assert!(!s.should_render(2, false, now));
		assert!(s.should_render(2, true, now), "a freed slot must deliver at once");
		assert!(
			s.next_deadline(Some(2), true, now) <= now,
			"and the deadline is already due"
		);
	}

	#[test]
	fn a_sub_rate_client_is_delivered_at_commit_time() {
		let (mut s, t0) = scheduler();
		let mut now = t0;
		for epoch in 1..=20u64 {
			now += 2 * INTERVAL;
			s.commit(now);
			assert!(s.should_render(epoch, true, now), "commit {epoch} must not wait");
			let delivery = s.delivered(epoch, now);
			assert_eq!(delivery.commit_to_delivery, Some(Duration::ZERO));
		}
	}

	#[test]
	fn an_over_rate_client_is_held_to_the_session_rate() {
		// A client committing ten times per interval: the token bucket admits
		// the burst allowance and then one frame per interval.
		let (mut s, t0) = scheduler();
		let intervals = 50u32;
		let mut delivered = 0;
		let mut now = t0;
		for step in 0..(intervals * 10) {
			now += INTERVAL / 10;
			s.commit(now);
			s.observe(u64::from(step), now);
			if s.should_render(u64::from(step), true, now) {
				s.delivered(u64::from(step), now);
				delivered += 1;
			}
		}
		// The cadence needs a few frames to see the client is over the rate.
		let rate_cap = intervals as usize + TOKEN_CAPACITY as usize + 1;
		assert!(delivered <= rate_cap, "delivered {delivered}, cap {rate_cap}");
		assert!(delivered >= intervals as usize - 1, "starved: delivered {delivered}");
	}

	#[test]
	fn a_client_slightly_over_the_rate_is_held_to_exactly_the_rate() {
		// 5% over the session rate: the refill headroom alone would let all of
		// it through.
		let (mut s, t0) = scheduler();
		let period = INTERVAL.div_f64(1.05);
		let seconds = 10u32;
		let commits = (f64::from(seconds) * RATE * 1.05) as u64;
		let mut delivered = 0usize;
		let mut now = t0;
		for epoch in 1..=commits {
			now += period;
			s.commit(now);
			s.observe(epoch, now);
			if s.should_render(epoch, true, now) {
				s.delivered(epoch, now);
				delivered += 1;
			} else {
				// Delivered at the token's refill, before the next commit.
				let due = s.next_deadline(Some(epoch), true, now);
				if due < now + period && s.should_render(epoch, true, due) {
					s.delivered(epoch, due);
					delivered += 1;
				}
			}
		}
		let intervals = (now - t0).as_secs_f64() / INTERVAL.as_secs_f64();
		let rate_cap = intervals.ceil() as usize + TOKEN_CAPACITY as usize + 1;
		assert!(
			delivered <= rate_cap,
			"delivered {delivered} in {intervals:.1} intervals"
		);
		assert!(delivered as f64 >= intervals - 2.0, "starved: delivered {delivered}");
	}

	#[test]
	fn the_surfaces_of_one_frame_count_once_for_the_cadence() {
		// Two surfaces committing together at the session rate are not over it:
		// each frame changes the scene twice, 50 us apart.
		let (mut s, t0) = scheduler();
		let mut now = t0;
		for frame in 0..40u64 {
			now += INTERVAL;
			s.commit(now);
			s.observe(2 * frame, now);
			s.commit(now + Duration::from_micros(50));
			s.observe(2 * frame + 1, now + Duration::from_micros(50));
		}
		assert!(!s.over_rate(), "cadence {:?}", s.cadence);
	}

	#[test]
	fn pointer_motion_without_commits_is_held_to_exactly_the_rate() {
		// A mouse moving the cursor at 1 kHz over a client that does not commit:
		// every motion is a new scene, so the cursor must not get the headroom.
		let (mut s, t0) = scheduler();
		let seconds = 10u32;
		let mut delivered = 0usize;
		let mut now = t0;
		for motion in 1..=u64::from(seconds) * 1000 {
			now += Duration::from_millis(1);
			s.observe(motion, now);
			if s.should_render(motion, true, now) {
				s.delivered(motion, now);
				delivered += 1;
			}
		}
		let intervals = (now - t0).as_secs_f64() / INTERVAL.as_secs_f64();
		let rate_cap = intervals.ceil() as usize + TOKEN_CAPACITY as usize + 1;
		assert!(
			delivered <= rate_cap,
			"delivered {delivered} in {intervals:.1} intervals"
		);
		assert!(delivered as f64 >= intervals - 2.0, "starved: delivered {delivered}");
	}

	#[test]
	fn an_unchanged_scene_does_not_move_the_cadence() {
		// Dispatches that change nothing (a release, a fence) are not changes.
		let (mut s, t0) = scheduler();
		for step in 0..40u32 {
			s.observe(7, t0 + Duration::from_millis(u64::from(step)));
		}
		assert_eq!(s.cadence, None);
	}

	#[test]
	fn token_refill_is_the_deadline_while_damage_is_pending() {
		let (mut s, t0) = scheduler();
		// Drain the bucket.
		s.delivered(1, t0);
		s.delivered(2, t0);
		s.commit(t0);
		assert!(!s.should_render(3, true, t0));
		let deadline = s.next_deadline(Some(3), true, t0);
		assert_eq!(
			deadline,
			s.next_callback.min(t0 + s.refill_period()),
			"one refill period to refill one token"
		);
		assert!(s.should_render(3, true, t0 + s.refill_period() + Duration::from_micros(1)));
	}

	#[test]
	fn a_session_rate_client_does_not_latch_a_token_deficit() {
		// A client presenting exactly once per interval that once had to wait
		// for a token (here: it starts with an empty bucket, half an interval
		// out of phase) must converge to delivery at commit time.
		let (mut s, t0) = scheduler();
		s.delivered(1, t0);
		s.delivered(2, t0);
		let mut commit_at = t0 + INTERVAL / 2;
		let mut waits = Vec::new();
		for epoch in 3..60u64 {
			s.commit(commit_at);
			let mut now = commit_at;
			while !s.should_render(epoch, true, now) {
				now = s
					.next_deadline(Some(epoch), true, now)
					.max(now + Duration::from_micros(1));
			}
			s.delivered(epoch, now);
			waits.push(now - commit_at);
			commit_at += INTERVAL;
		}
		assert!(waits[0] > Duration::ZERO, "the setup must start with a deficit");
		assert!(
			waits[30..].iter().all(|wait| *wait < Duration::from_micros(10)),
			"the deficit latched: {:?}",
			&waits[30..]
		);
	}

	#[test]
	fn the_earliest_of_the_candidates_wins() {
		let (mut s, t0) = scheduler();
		s.delivered(1, t0);
		// Idle: callbacks every interval beat the one-second keepalive.
		assert_eq!(s.next_deadline(Some(1), true, t0), t0 + INTERVAL);
		// Slower than the keepalive: the keepalive wins.
		let mut slow = Scheduler::new(1.0, t0);
		slow.delivered(1, t0);
		slow.next_callback = t0 + 5 * STATIC_KEEPALIVE;
		assert_eq!(slow.next_deadline(Some(1), true, t0), t0 + STATIC_KEEPALIVE);
		// Nothing to show: only the callback tick.
		assert_eq!(s.next_deadline(None, true, t0), s.next_callback);
	}

	#[test]
	fn a_dropped_frame_is_retried_one_interval_later_not_spun() {
		let (mut s, t0) = scheduler();
		s.commit(t0);
		assert!(s.should_render(1, true, t0));
		s.deferred(t0);
		assert!(!s.should_render(1, true, t0 + ms(1)));
		assert_eq!(s.next_deadline(Some(1), true, t0), s.next_callback.min(t0 + INTERVAL));
		assert!(s.should_render(1, true, t0 + INTERVAL));
	}

	#[test]
	fn callbacks_keep_the_session_cadence_and_snap_after_an_overrun() {
		let (mut s, t0) = scheduler();
		assert!(!s.callbacks_due(t0));
		assert!(s.callbacks_due(t0 + INTERVAL));
		// Slightly late: the target advances from its old value, no drift.
		s.callbacks_sent(t0 + INTERVAL + ms(2));
		assert_eq!(s.next_callback, t0 + 2 * INTERVAL);
		// A long stall snaps forward instead of bursting.
		let late = t0 + ms(500);
		s.callbacks_sent(late);
		assert_eq!(s.next_callback, late + INTERVAL);
	}

	#[test]
	fn no_limit_has_no_cycle() {
		let (s, _) = scheduler();
		assert_eq!(s.fps_limit_cycle(0), None);
		assert_eq!(s.fps_limit_cycle(50), Some(2.0));
	}

	#[test]
	fn the_limit_cycle_ignores_frame_clock_corrections() {
		// The grid steered as far as it may: the period 1% shorter, a phase shift
		// pending. The cycle is in ticks, so it stays put.
		let (mut s, t0) = scheduler();
		s.correct(correction(INTERVAL - INTERVAL / 100, -5000), t0);
		assert_eq!(s.fps_limit_cycle(RATE as u32), Some(1.0));
	}

	/// The ticks, of the first `ticks` of a `rate` Hz session under a `limit_hz`
	/// cap, on which a window requesting callbacks on every tick in `requests`
	/// receives them.
	fn limited_ticks(rate: f64, limit_hz: u32, ticks: u32, requests: impl Fn(u32) -> bool) -> Vec<u32> {
		let cycle = Scheduler::new(rate, Instant::now()).fps_limit_cycle(limit_hz).unwrap();
		let mut limit = FpsLimit::default();
		let mut sent = Vec::new();
		for n in 0..ticks {
			if limit.tick() && requests(n) {
				limit.sent(cycle);
				sent.push(n);
			}
		}
		sent
	}

	#[test]
	fn a_limit_at_the_session_rate_passes_every_tick() {
		for rate in [30, 60, 120] {
			let sent = limited_ticks(f64::from(rate), rate, 10 * rate, |_| true);
			assert_eq!(sent.len(), 10 * rate as usize, "{rate} Hz");
		}
	}

	#[test]
	fn a_limit_above_the_session_rate_passes_every_tick() {
		assert_eq!(limited_ticks(60.0, 144, 600, |_| true).len(), 600);
	}

	#[test]
	fn a_limit_at_half_the_session_rate_passes_every_second_tick() {
		for rate in [30, 60, 120] {
			let sent = limited_ticks(f64::from(rate), rate / 2, 10 * rate, |_| true);
			assert_eq!(sent, (0..10 * rate).step_by(2).collect::<Vec<_>>(), "{rate} Hz");
		}
	}

	#[test]
	fn a_fractional_cycle_repeats_its_pattern_exactly() {
		// 50 fps on 120 Hz: 2.4 ticks a cycle, five callbacks in every 12 ticks,
		// although five cycles sum to 12 ticks only up to float rounding.
		let sent = limited_ticks(120.0, 50, 1200, |_| true);
		assert_eq!(sent.len(), 500);
		for (n, chunk) in sent.chunks(5).enumerate() {
			let base = 12 * n as u32;
			assert_eq!(chunk, [0, 3, 5, 8, 10].map(|tick| base + tick), "cycle {n}");
		}
	}

	#[test]
	fn a_limit_just_under_the_session_rate_is_enforced() {
		// 118 fps on a 120 Hz session: the window skips two ticks in every 120,
		// never two in a row.
		let sent = limited_ticks(120.0, 118, 1200, |_| true);
		assert_eq!(sent.len(), 1180);
		assert!(sent.windows(2).all(|pair| pair[1] - pair[0] <= 2));
	}

	#[test]
	fn a_window_that_requested_nothing_starts_a_fresh_cycle() {
		// 30 fps on 60 Hz: idle from tick 1 to 10, the window is served on the
		// first tick it asks again, then one cycle later, not on the next tick.
		let sent = limited_ticks(60.0, 30, 16, |n| n == 0 || n > 10);
		assert_eq!(sent, vec![0, 11, 13, 15]);
	}

	#[test]
	fn steam_and_overlays_are_exempt_from_the_limiter() {
		use crate::focus::{WindowKind, WindowMeta};
		let mut meta = WindowMeta::new(1, WindowKind::Xwayland);
		assert!(limiter_covers(&meta));
		meta.is_overlay = true;
		assert!(!limiter_covers(&meta));
		meta.is_overlay = false;
		meta.is_external_overlay = true;
		assert!(!limiter_covers(&meta));
		meta.is_external_overlay = false;
		meta.app_id = crate::focus::STEAM_BIG_PICTURE_APP_ID;
		assert!(!limiter_covers(&meta));
		meta.app_id = 0;
		meta.is_steam_legacy_big_picture = true;
		assert!(!limiter_covers(&meta));
	}

	#[test]
	fn delivery_reports_commit_latency_and_repeats() {
		let (mut s, t0) = scheduler();
		s.commit(t0);
		let first = s.delivered(1, t0 + ms(3));
		assert_eq!(first.commit_to_delivery, Some(ms(3)));
		assert!(!first.repeat);
		// A cursor redraw with no new commit repeats the epoch.
		let second = s.delivered(2, t0 + ms(20));
		assert_eq!(second.commit_to_delivery, None);
		assert_eq!(second.since_last, Some(ms(17)));
		assert!(second.repeat);
	}

	fn correction(period: Duration, phase_shift_us: i64) -> FrameClockCorrection {
		FrameClockCorrection {
			period,
			phase_shift_ns: phase_shift_us * 1000,
		}
	}

	/// Run the frame-callback tick from `from` to `until`, sending callbacks the
	/// moment they are due, with a client that commits on each one and is
	/// delivered on commit. Returns the commit times.
	fn commits_on_the_tick(s: &mut Scheduler, from: Instant, until: Instant) -> Vec<Instant> {
		let mut commits = Vec::new();
		let mut scene = s.commit_epoch() + 1000;
		let mut now = from;
		loop {
			now = now.max(s.next_deadline(None, true, now));
			if now > until {
				return commits;
			}
			assert!(s.callbacks_due(now));
			s.callbacks_sent(now);
			s.commit(now);
			scene += 1;
			s.observe(scene, now);
			assert!(
				s.should_render(scene, true, now),
				"a tick-paced client is delivered on commit"
			);
			s.delivered(scene, now);
			commits.push(now);
		}
	}

	fn spacings_us(times: &[Instant]) -> Vec<u64> {
		times
			.windows(2)
			.map(|pair| (pair[1] - pair[0]).as_micros() as u64)
			.collect()
	}

	/// [`commits_on_the_tick`] with a second surface (a launcher or helper
	/// window) that commits on its own every `every` from `*sparse_at`, and
	/// frames delivered as soon as the scheduler allows rather than asserted on
	/// commit. Returns each tick commit with the delivery that carried it.
	fn ticks_with_a_sparse_surface(
		s: &mut Scheduler,
		from: Instant,
		until: Instant,
		sparse_at: &mut Instant,
		every: Duration,
	) -> Vec<(Instant, Instant)> {
		let mut frames = Vec::new();
		let mut waiting = Vec::new();
		let mut pending = None;
		let mut scene = s.commit_epoch() + 1000;
		let mut now = from;
		loop {
			let ready = pending.map_or(s.next_deadline(None, true, now), |scene| {
				s.next_deadline(Some(scene), true, now)
			});
			now = now.max(ready.min(*sparse_at));
			if now > until {
				return frames;
			}
			if s.callbacks_due(now) {
				s.callbacks_sent(now);
				s.commit(now);
				scene += 1;
				s.observe(scene, now);
				pending = Some(scene);
				waiting.push(now);
			}
			if now >= *sparse_at {
				s.commit(now);
				scene += 1;
				s.observe(scene, now);
				pending = Some(scene);
				*sparse_at += every;
			}
			if let Some(scene) = pending
				&& s.should_render(scene, true, now)
			{
				s.delivered(scene, now);
				frames.extend(waiting.drain(..).map(|commit| (commit, now)));
				pending = None;
			}
		}
	}

	/// Where `at` falls in the nominal 10 ms grid that starts at `t0`, in us.
	fn grid_phase_us(at: Instant, t0: Instant) -> u64 {
		((at - t0).as_nanos() % INTERVAL.as_nanos()) as u64 / 1000
	}

	#[test]
	fn the_clock_follows_a_corrected_period() {
		let (mut s, t0) = scheduler();
		s.correct(correction(Duration::from_micros(10_050), 0), t0);
		let commits = commits_on_the_tick(&mut s, t0, t0 + ms(100));
		assert_eq!(s.interval(), Duration::from_micros(10_050));
		// The tick already armed keeps its time; every one after is 10.05 ms on.
		assert_eq!(commits[0], t0 + INTERVAL);
		assert!(
			spacings_us(&commits).iter().all(|spacing| *spacing == 10_050),
			"{:?}",
			spacings_us(&commits)
		);
	}

	#[test]
	fn a_phase_shift_moves_the_next_frames_commit_times() {
		let (mut s, t0) = scheduler();
		let before = commits_on_the_tick(&mut s, t0, t0 + ms(30));
		let now = *before.last().unwrap();
		// 600 us later, applied at most 250 us per frame from the tick after the
		// one already armed.
		s.correct(correction(INTERVAL, 600), now);
		let later = commits_on_the_tick(&mut s, now, now + ms(60));
		assert_eq!(later[0], now + INTERVAL);
		assert_eq!(spacings_us(&later[..5]), [10_250, 10_250, 10_100, 10_000]);
		// And earlier: 300 us.
		let now = *later.last().unwrap();
		s.correct(correction(INTERVAL, -300), now);
		let earlier = commits_on_the_tick(&mut s, now, now + ms(40));
		assert_eq!(earlier[0], now + INTERVAL);
		assert_eq!(spacings_us(&earlier[..4]), [9_750, 9_950, 10_000]);
	}

	#[test]
	fn a_phase_shift_moves_deliveries_although_a_second_surface_commits_sparsely() {
		// Ticket 50's 32-bit DXVK run: a tick-paced Game and a second X11
		// window committing every 500 ms, half an interval off the tick. Its
		// commits must not leave the Game's frames waiting for tokens on a
		// clock that Phase shifts do not move.
		let (mut s, t0) = scheduler();
		let mut sparse_at = t0 + ms(5);
		let before = ticks_with_a_sparse_surface(&mut s, t0, t0 + ms(4_000), &mut sparse_at, ms(500));
		let lag = |frames: &[(Instant, Instant)]| {
			frames
				.iter()
				.map(|(commit, delivery)| *delivery - *commit)
				.max()
				.unwrap()
		};
		assert!(
			lag(&before[100..]) < Duration::from_micros(500),
			"lag before the shift: {:?}",
			lag(&before[100..])
		);
		let now = before.last().unwrap().1;
		let phase = grid_phase_us(before.last().unwrap().1, t0);
		s.correct(correction(INTERVAL, 1_500), now);
		let after = ticks_with_a_sparse_surface(&mut s, now, now + ms(2_000), &mut sparse_at, ms(500));
		assert!(
			lag(&after) < Duration::from_micros(500),
			"lag after the shift: {:?}",
			lag(&after)
		);
		let moved: Vec<u64> = after[10..]
			.iter()
			.map(|(_, delivery)| grid_phase_us(*delivery, t0))
			.collect();
		assert!(
			moved.iter().all(|at| at.abs_diff(phase + 1_500) <= 50),
			"deliveries at {phase} us must move to {} us: {moved:?}",
			phase + 1_500
		);
	}

	#[test]
	fn a_phase_shift_moves_deliveries_held_to_the_rate() {
		// A scene changing every 100 us (cursor motion, an uncapped client) is
		// delivered on the token clock, not on commits: the token clock must
		// slew with the frame clock, or the Phase lock cannot move it.
		let (mut s, t0) = scheduler();
		let step = Duration::from_micros(100);
		let mut deliveries = Vec::new();
		let mut scene = 0u64;
		let mut now = t0;
		while now < t0 + ms(3_000) {
			now += step;
			if now == t0 + ms(2_000) {
				s.correct(correction(INTERVAL, 1_500), now);
			}
			if s.callbacks_due(now) {
				s.callbacks_sent(now);
			}
			s.commit(now);
			scene += 1;
			s.observe(scene, now);
			if s.should_render(scene, true, now) {
				s.delivered(scene, now);
				deliveries.push(now);
			}
		}
		// How far each delivery in `from..until` ms sits after `phase` on the
		// grid, in us.
		let offsets = |from: u64, until: u64, phase: u64| -> Vec<u64> {
			deliveries
				.iter()
				.filter(|at| **at > t0 + ms(from) && **at < t0 + ms(until))
				.map(|at| (grid_phase_us(*at, t0) + 10_000 - phase) % 10_000)
				.collect()
		};
		let phase = grid_phase_us(*deliveries.iter().find(|at| **at > t0 + ms(1_000)).unwrap(), t0);
		let before = offsets(1_000, 2_000, phase);
		assert!(
			before.iter().all(|offset| *offset <= 100 || *offset >= 9_900),
			"{before:?}"
		);
		let after = offsets(2_200, 3_000, phase);
		assert!(
			after.iter().all(|offset| offset.abs_diff(1_500) <= 100),
			"deliveries must move 1500 us: {after:?}"
		);
	}

	#[test]
	fn shifts_waiting_to_be_applied_add_up_to_at_most_half_a_period() {
		let (mut s, t0) = scheduler();
		// Two 4 ms shifts: 8 ms waiting, bounded to 5 ms.
		s.correct(correction(INTERVAL, 4_000), t0);
		s.correct(correction(INTERVAL, 4_000), t0);
		let commits = commits_on_the_tick(&mut s, t0, t0 + ms(400));
		let last = *commits.last().unwrap();
		let n = commits.len() as u32;
		assert_eq!(last, t0 + INTERVAL * n + ms(5));
	}

	#[test]
	fn with_no_correction_before_the_timeout_the_clock_goes_back_to_nominal() {
		let (mut s, t0) = scheduler();
		let steered = Duration::from_micros(10_080);
		s.correct(correction(steered, 5_000), t0);
		assert!(s.steered());
		// A correction just before the timeout keeps the clock steered.
		commits_on_the_tick(&mut s, t0, t0 + ms(900));
		s.correct(correction(steered, 0), t0 + ms(900));
		commits_on_the_tick(&mut s, t0 + ms(900), t0 + ms(1_800));
		assert!(s.steered());
		assert_eq!(s.interval(), steered);
		// Then nothing: from one timeout after the last correction, the nominal
		// period.
		s.correct(correction(steered, 0), t0 + ms(1_800));
		let commits = commits_on_the_tick(&mut s, t0 + ms(1_800), t0 + ms(4_000));
		assert!(!s.steered());
		assert_eq!(s.interval(), INTERVAL);
		let after = commits
			.iter()
			.position(|at| *at >= t0 + ms(1_800) + CORRECTION_TIMEOUT)
			.unwrap();
		assert!(
			spacings_us(&commits[after..]).iter().all(|spacing| *spacing == 10_000),
			"{:?}",
			spacings_us(&commits[after..])
		);
	}

	#[test]
	fn a_new_consumer_starts_at_the_nominal_period_with_no_shift_pending() {
		let (mut s, t0) = scheduler();
		let before = commits_on_the_tick(&mut s, t0, t0 + ms(30));
		let now = *before.last().unwrap();
		// The last consumer's slower period and a 5 ms shift, two frames into
		// applying it.
		s.correct(correction(Duration::from_micros(10_080), 5_000), now);
		let steered = commits_on_the_tick(&mut s, now, now + ms(25));
		let now = *steered.last().unwrap();
		s.new_consumer(now);
		assert!(!s.steered());
		assert_eq!(s.interval(), INTERVAL);
		let after = commits_on_the_tick(&mut s, now, now + ms(200));
		assert!(
			spacings_us(&after).iter().all(|spacing| *spacing == 10_000),
			"{:?}",
			spacings_us(&after)
		);
	}

	#[test]
	fn a_tick_paced_client_keeps_its_headroom_on_a_faster_corrected_period() {
		// 1% faster than nominal: the client still averages one interval, so it
		// is not held back to the nominal rate.
		let (mut s, t0) = scheduler();
		s.correct(correction(INTERVAL.mul_f64(0.99), 0), t0);
		commits_on_the_tick(&mut s, t0, t0 + ms(900));
		assert!(!s.over_rate(), "cadence {:?}", s.cadence);
	}
}
