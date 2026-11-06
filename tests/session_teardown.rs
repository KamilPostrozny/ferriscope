//! Stopping a compositor releases everything its session created.
//!
//! An embedder such as Moonshine launches one compositor per stream and keeps
//! running after it stops, so a stopped session must leave nothing behind: no
//! Xwayland child, no listening Wayland or X11 socket, no lock file, and no
//! thread started for the session. Two launch-stop cycles run back to back, so
//! the second also checks that the first left nothing that blocks a new
//! session.
//!
//! It is `#[ignore]`d: it needs a render node and Xwayland. Run it alone:
//!
//! ```text
//! cargo test -p ferriscope --test session_teardown -- --ignored --nocapture
//! ```

use std::{
	path::{Path, PathBuf},
	time::Duration,
};

use ferriscope::{Compositor, CompositorConfig, Ready};

/// Output width.
const WIDTH: u32 = 320;
/// Output height.
const HEIGHT: u32 = 240;
/// Budget for the `Ready` handshake (Xwayland startup dominates).
const READY_TIMEOUT: Duration = Duration::from_secs(20);
/// Launch-stop cycles.
const CYCLES: usize = 2;

/// The name the compositor thread and every thread it spawns without a name of
/// its own carry: `ferriscope-compositor`, truncated to the kernel's 15 bytes.
const COMPOSITOR_THREAD: &str = "ferriscope-comp";

/// This process's live and zombie children named `Xwayland`.
fn xwayland_children() -> Vec<u32> {
	let me = std::process::id();
	let Ok(entries) = std::fs::read_dir("/proc") else {
		return Vec::new();
	};
	entries
		.filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<u32>().ok())
		.filter(|pid| {
			let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
				return false;
			};
			// `pid (comm) state ppid …`; comm may hold spaces and parentheses.
			let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else {
				return false;
			};
			let comm = &stat[open + 1..close];
			let ppid = stat[close + 1..]
				.split_whitespace()
				.nth(1)
				.and_then(|p| p.parse::<u32>().ok());
			comm == "Xwayland" && ppid == Some(me)
		})
		.collect()
}

/// This process's threads named like the compositor thread.
fn compositor_threads() -> usize {
	std::fs::read_dir("/proc/self/task")
		.map(|tasks| {
			tasks
				.filter_map(|task| std::fs::read_to_string(task.ok()?.path().join("comm")).ok())
				.filter(|comm| comm.trim_end() == COMPOSITOR_THREAD)
				.count()
		})
		.unwrap_or(0)
}

/// The files a session's sockets live in.
fn session_files(runtime_dir: &Path, ready: &Ready) -> Vec<PathBuf> {
	let display = ready.xdisplay.expect("xwayland reports an X display");
	vec![
		runtime_dir.join(&ready.wayland_display),
		runtime_dir.join(format!("{}.lock", ready.wayland_display)),
		PathBuf::from(format!("/tmp/.X11-unix/X{display}")),
		PathBuf::from(format!("/tmp/.X{display}-lock")),
	]
}

#[test]
#[ignore = "requires GPU and Xwayland"]
fn start_stop_cycles_leave_no_xwayland_socket_lock_file_or_thread() {
	let runtime_dir = std::env::temp_dir().join(format!("ferriscope-teardown-{}", std::process::id()));
	std::fs::create_dir_all(&runtime_dir).expect("create the runtime directory");
	let threads_before = compositor_threads();

	for cycle in 1..=CYCLES {
		let config = CompositorConfig {
			width: WIDTH,
			height: HEIGHT,
			xwayland: true,
			command: None,
			runtime_dir: Some(runtime_dir.clone()),
			..CompositorConfig::default()
		};
		let (compositor, handles) = Compositor::launch(config).expect("launch compositor");
		let ready = handles
			.ready
			.recv_timeout(READY_TIMEOUT)
			.expect("ready handshake within timeout");
		let files = session_files(&runtime_dir, &ready);

		// The session really created what the checks below expect gone.
		assert_eq!(
			xwayland_children().len(),
			1,
			"cycle {cycle}: one Xwayland while running"
		);
		for file in &files {
			assert!(file.exists(), "cycle {cycle}: {} exists while running", file.display());
		}

		compositor.stop().expect("stop compositor");
		drop(handles);

		assert_eq!(
			xwayland_children(),
			Vec::<u32>::new(),
			"cycle {cycle}: an Xwayland child outlived its session"
		);
		for file in &files {
			assert!(!file.exists(), "cycle {cycle}: {} outlived its session", file.display());
		}
		assert_eq!(
			compositor_threads(),
			threads_before,
			"cycle {cycle}: a thread the compositor started outlived it"
		);
	}

	let _ = std::fs::remove_dir(&runtime_dir);
}
