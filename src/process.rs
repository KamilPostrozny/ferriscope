//! A client's Steam app id from its process (`Process::GetAppIdFromPid`,
//! `Utils/Process.cpp:782`).
//!
//! An Xwayland window carries its app id in `STEAM_GAME`; one without it (and
//! a native Wayland window, which has no such property) gets one derived from
//! the client's process, as gamescope does (`wlserver.cpp:2020` for Wayland,
//! `steamcompmgr.cpp:5946` for X11, using the XRes client pid): first from a systemd scope named
//! `app-steam-app<appid>-<pid>.scope` in its cgroup, then from the
//! `reaper SteamLaunch AppId=<appid>` process it runs under. A Proton game with
//! `PROTON_ENABLE_WAYLAND=1` is such a window.
//!
//! Also the compositor's own Xwayland child, found the same way
//! ([`xwayland_child`]) so the session can stop it.

use std::fs;

/// Memoised [`app_id_from_pid`], so the `/proc` walk runs once per pid rather
/// than once per window (a game maps many windows from one process).
///
/// gamescope does not cache (`steamcompmgr.cpp:5946` walks `/proc` per window);
/// the old Moonshine compositor did. A process keeps its cgroup and ancestry
/// for life, so the only staleness is a recycled pid, bounded by [`Self::CAP`]
/// (the cache is dropped when it fills).
#[derive(Debug, Default)]
pub(crate) struct AppIdCache {
	by_pid: std::collections::HashMap<u32, u32>,
}

impl AppIdCache {
	const CAP: usize = 256;

	/// The app id of `pid` (0 for none), derived through `derive` on a miss.
	pub(crate) fn get_with(&mut self, pid: u32, derive: impl FnOnce(u32) -> u32) -> u32 {
		if let Some(app_id) = self.by_pid.get(&pid) {
			return *app_id;
		}
		if self.by_pid.len() >= Self::CAP {
			self.by_pid.clear();
		}
		let app_id = derive(pid);
		self.by_pid.insert(pid, app_id);
		app_id
	}

	/// The app id of `pid` from `/proc`.
	pub(crate) fn get(&mut self, pid: u32) -> u32 {
		self.get_with(pid, app_id_from_pid)
	}
}

/// The app id of process `pid`, or 0 when it has none.
pub(crate) fn app_id_from_pid(pid: u32) -> u32 {
	let from_cgroup = fs::read_to_string(format!("/proc/{pid}/cgroup"))
		.map(|cgroup| app_id_from_cgroup(&cgroup))
		.unwrap_or(0);
	if from_cgroup != 0 {
		return from_cgroup;
	}
	app_id_from_reaper(
		pid,
		|pid| fs::read_to_string(format!("/proc/{pid}/stat")).ok(),
		|pid| fs::read(format!("/proc/{pid}/cmdline")).ok(),
	)
}

/// `GetAppIdFromCgroup` (`Process.cpp:666`): the app id of the first
/// `app-steam-app<appid>-<pid>.scope` in a `/proc/<pid>/cgroup` listing.
fn app_id_from_cgroup(cgroup: &str) -> u32 {
	cgroup
		.lines()
		.filter_map(|line| {
			// hierarchy-ID:controller-list:cgroup-path
			let path = line.splitn(3, ':').nth(2)?;
			let scope = path.rsplit('/').next()?;
			let rest = scope.strip_prefix("app-steam-app")?.strip_suffix(".scope")?;
			let (app_id, reaper) = rest.split_once('-')?;
			reaper.parse::<i32>().ok()?;
			app_id.parse::<u32>().ok().filter(|id| *id != 0)
		})
		.next()
		.unwrap_or(0)
}

/// `GetAppIdFromReaper` (`Process.cpp:691`): walk the parents of `pid` and
/// take the app id of the outermost `reaper SteamLaunch AppId=<appid>`.
///
/// `stat` and `cmdline` read `/proc/<pid>/stat` and `/proc/<pid>/cmdline`.
fn app_id_from_reaper(pid: u32, stat: impl Fn(u32) -> Option<String>, cmdline: impl Fn(u32) -> Option<Vec<u8>>) -> u32 {
	let mut found = 0;
	let mut next = pid;
	// A pid chain ends at 1; the bound only guards against a malformed /proc.
	for _ in 0..4096 {
		let Some(line) = stat(next) else {
			break;
		};
		let Some((name, parent)) = name_and_parent(&line) else {
			break;
		};
		if name == "reaper"
			&& let Some(app_id) = cmdline(next).and_then(|cmdline| steam_launch_app_id(&cmdline))
		{
			found = app_id;
		}
		if parent <= 0 {
			break;
		}
		next = parent as u32;
	}
	found
}

/// The `AppId=<appid>` of a `reaper` command line that has `SteamLaunch`
/// before it, looking only at the reaper's own arguments (up to `--`).
fn steam_launch_app_id(cmdline: &[u8]) -> Option<u32> {
	let mut steam_launch = false;
	let mut app_id = None;
	// The first argument is the program itself, as gamescope skips it.
	for arg in cmdline.split(|byte| *byte == 0).skip(1) {
		match arg {
			b"SteamLaunch" => steam_launch = true,
			b"--" => break,
			_ => {
				let parsed = std::str::from_utf8(arg)
					.ok()
					.and_then(|arg| arg.strip_prefix("AppId="))
					.and_then(|id| id.parse::<u32>().ok())
					.filter(|id| *id != 0);
				if let Some(id) = parsed
					&& steam_launch
				{
					app_id = Some(id);
				}
			},
		}
	}
	app_id
}

/// The name and parent pid of a `/proc/<pid>/stat` line.
fn name_and_parent(line: &str) -> Option<(&str, i64)> {
	// `pid (comm) state ppid ...`; the name may itself contain ')'.
	let (open, close) = (line.find('(')?, line.rfind(')')?);
	let parent = line
		.get(close + 1..)?
		.split_whitespace()
		.nth(1)
		.and_then(|ppid| ppid.parse::<i64>().ok())
		.unwrap_or(-1);
	Some((line.get(open + 1..close)?, parent))
}

/// The pid of the `Xwayland` this process started for X display `display`,
/// live or not yet reaped.
///
/// smithay's `XWayland::spawn` keeps the child's handle to itself, so it is
/// found by its command line, which smithay starts with `Xwayland :<display>`.
/// The display number is unique while the session holds its X lock.
pub(crate) fn xwayland_child(display: u32) -> Option<u32> {
	let pids = fs::read_dir("/proc")
		.ok()?
		.filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<u32>().ok());
	find_xwayland(
		std::process::id(),
		display,
		pids,
		|pid| fs::read_to_string(format!("/proc/{pid}/stat")).ok(),
		|pid| fs::read(format!("/proc/{pid}/cmdline")).ok(),
	)
}

/// [`xwayland_child`] over `pids`, with `stat` and `cmdline` reading
/// `/proc/<pid>/stat` and `/proc/<pid>/cmdline`.
fn find_xwayland(
	parent: u32,
	display: u32,
	pids: impl IntoIterator<Item = u32>,
	stat: impl Fn(u32) -> Option<String>,
	cmdline: impl Fn(u32) -> Option<Vec<u8>>,
) -> Option<u32> {
	let display = format!(":{display}");
	pids.into_iter().find(|&pid| {
		stat(pid).is_some_and(|line| {
			name_and_parent(&line).is_some_and(|(name, ppid)| name == "Xwayland" && ppid == i64::from(parent))
		}) && cmdline(pid).is_some_and(|cmdline| cmdline.split(|byte| *byte == 0).nth(1) == Some(display.as_bytes()))
	})
}

#[cfg(test)]
mod tests {
	use std::collections::HashMap;

	use super::*;

	#[test]
	fn the_cache_derives_once_per_pid_and_evicts_when_full() {
		let mut cache = AppIdCache::default();
		let mut calls = 0;
		for _ in 0..3 {
			assert_eq!(
				cache.get_with(5, |_| {
					calls += 1;
					9
				}),
				9
			);
		}
		assert_eq!(calls, 1);
		for pid in 100..100 + AppIdCache::CAP as u32 {
			cache.get_with(pid, |_| 1);
		}
		assert!(cache.by_pid.len() <= AppIdCache::CAP);
		assert_eq!(cache.get_with(5, |_| 0), 0, "evicted, so derived again");
	}

	#[test]
	fn a_steam_app_scope_names_the_app() {
		let cgroup = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-steam-app3405340-2835357.scope\n";
		assert_eq!(app_id_from_cgroup(cgroup), 3_405_340);
	}

	#[test]
	fn a_cgroup_without_a_steam_scope_has_no_app() {
		let cgroup = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/steam.service\n";
		assert_eq!(app_id_from_cgroup(cgroup), 0);
		assert_eq!(app_id_from_cgroup("0::/app-steam-appX-1.scope\n"), 0);
	}

	/// A process table: pid -> (comm, ppid, cmdline).
	fn table(entries: &[(u32, &str, u32, &[&str])]) -> HashMap<u32, (String, u32, Vec<u8>)> {
		entries
			.iter()
			.map(|(pid, comm, ppid, args)| {
				let cmdline = args.iter().flat_map(|arg| arg.bytes().chain([0])).collect();
				(*pid, (comm.to_string(), *ppid, cmdline))
			})
			.collect()
	}

	fn lookup(processes: &HashMap<u32, (String, u32, Vec<u8>)>, pid: u32) -> u32 {
		app_id_from_reaper(
			pid,
			|pid| {
				processes
					.get(&pid)
					.map(|(comm, ppid, _)| format!("{pid} ({comm}) S {ppid} 0 0"))
			},
			|pid| processes.get(&pid).map(|(_, _, cmdline)| cmdline.clone()),
		)
	}

	#[test]
	fn the_reaper_a_proton_game_runs_under_names_the_app() {
		// The chain of a Proton game under pressure-vessel.
		let processes = table(&[
			(2835544, "Megabonk.exe", 2835424, &["Megabonk.exe"]),
			(2835424, "pv-adverb", 2835358, &["pv-adverb"]),
			(2835358, "srt-bwrap", 2835357, &["srt-bwrap"]),
			(
				2835357,
				"reaper",
				2831508,
				&[
					"/steam/ubuntu12_32/reaper",
					"SteamLaunch",
					"AppId=3405340",
					"--",
					"/entry-point",
					"AppId=1",
				],
			),
			(2831508, "steam", 1, &["steam"]),
		]);
		assert_eq!(lookup(&processes, 2835544), 3_405_340);
	}

	#[test]
	fn an_app_id_without_steam_launch_or_after_the_separator_does_not_count() {
		let processes = table(&[
			(10, "game", 11, &["game"]),
			(11, "reaper", 1, &["reaper", "AppId=5", "--", "SteamLaunch", "AppId=6"]),
		]);
		assert_eq!(lookup(&processes, 10), 0);
	}

	#[test]
	fn the_outermost_reaper_wins_and_odd_names_parse() {
		let processes = table(&[
			(20, "a (weird) name", 21, &["x"]),
			(21, "reaper", 22, &["reaper", "SteamLaunch", "AppId=7", "--"]),
			(22, "reaper", 1, &["reaper", "SteamLaunch", "AppId=8", "--"]),
		]);
		assert_eq!(lookup(&processes, 20), 8);
	}

	#[test]
	fn a_process_outside_steam_has_no_app() {
		let processes = table(&[(30, "vkcube", 1, &["vkcube"])]);
		assert_eq!(lookup(&processes, 30), 0);
	}

	fn find(processes: &HashMap<u32, (String, u32, Vec<u8>)>, parent: u32, display: u32) -> Option<u32> {
		let mut pids: Vec<u32> = processes.keys().copied().collect();
		pids.sort_unstable();
		find_xwayland(
			parent,
			display,
			pids,
			|pid| {
				processes
					.get(&pid)
					.map(|(comm, ppid, _)| format!("{pid} ({comm}) S {ppid} 0 0"))
			},
			|pid| processes.get(&pid).map(|(_, _, cmdline)| cmdline.clone()),
		)
	}

	#[test]
	fn the_xwayland_this_process_spawned_for_the_display_is_found() {
		let processes = table(&[
			(40, "Xwayland", 7, &["Xwayland", ":0", "-rootless", "-terminate"]),
			(41, "Xwayland", 7, &["Xwayland", ":3", "-rootless", "-terminate"]),
			(42, "vkcube", 7, &["vkcube"]),
		]);
		assert_eq!(find(&processes, 7, 3), Some(41));
		assert_eq!(find(&processes, 7, 0), Some(40));
	}

	#[test]
	fn another_parents_xwayland_or_display_is_not_ours() {
		let processes = table(&[
			// The desktop's Xwayland on the same display number, under niri.
			(50, "Xwayland", 9, &["Xwayland", ":3", "-rootless"]),
			// Ours, on another display.
			(51, "Xwayland", 7, &["Xwayland", ":30", "-rootless"]),
			// A child of ours named otherwise, with the same argument.
			(52, "Xvfb", 7, &["Xvfb", ":3"]),
		]);
		assert_eq!(find(&processes, 7, 3), None);
	}
}
