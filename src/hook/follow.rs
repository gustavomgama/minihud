//! Follow mode: watch for a target process and manage the present-hook
//! injection lifecycle (detect → inject → retarget → unhook).
//!
//! This module is deliberately **hooking-only**. It never reads the shared
//! frame ring and never computes fps/frametime — the consequences of hooking
//! are out of scope here. The loop logs lifecycle events only.
//!
//! Target selection ([`matches`], [`select_target`], [`skip_reason`],
//! [`parse_follow`]) is pure and unit-tested; the watch loop ([`run`]) is I/O
//! and is exercised by running it against a real target.

use std::time::Duration;

/// Default poll interval for `--follow` (ms).
pub const DEFAULT_POLL_MS: u64 = 400;

/// Parsed `--follow [--match <glob>] [--poll-ms <n>] [--follow-secs <n>]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FollowArgs {
    pub match_glob: Option<String>,
    pub poll_ms: u64,
    /// Stop after this many seconds (a bounded follow). `None` runs until Ctrl+C.
    pub secs: Option<u64>,
}

impl Default for FollowArgs {
    /// The `--follow`-as-default arguments: follow the foreground process on the
    /// default cadence until Ctrl+C, with no name filter.
    fn default() -> Self {
        Self {
            match_glob: None,
            poll_ms: DEFAULT_POLL_MS,
            secs: None,
        }
    }
}

/// Parse `--follow [--match <glob>] [--poll-ms <n>] [--follow-secs <n>]` from
/// the process arguments.
///
/// Returns `None` only when no follow flag is present at all. Any of
/// `--follow`/`--match`/`--poll-ms`/`--follow-secs` selects follow mode (it is
/// the default), so `--follow-secs 5` alone bounds the default run. A
/// `--match`/`--poll-ms`/`--follow-secs` without a value (or an unparseable
/// numeric one) yields `None`, so the caller falls back rather than guessing.
pub fn parse_follow(args: &[String]) -> Option<FollowArgs> {
    let present = args.iter().any(|a| {
        matches!(
            a.as_str(),
            "--follow" | "--match" | "--poll-ms" | "--follow-secs"
        )
    });
    if !present {
        return None;
    }
    let mut match_glob = None;
    let mut poll_ms = DEFAULT_POLL_MS;
    let mut secs = None;
    for (i, a) in args.iter().enumerate() {
        match a.as_str() {
            "--match" => match_glob = Some(args.get(i + 1)?.clone()),
            "--poll-ms" => poll_ms = args.get(i + 1)?.parse().ok()?,
            "--follow-secs" => secs = Some(args.get(i + 1)?.parse().ok()?),
            _ => {}
        }
    }
    Some(FollowArgs {
        match_glob,
        poll_ms,
        secs,
    })
}

/// The file name component of a path, treating both `\` and `/` as separators.
fn basename(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// True when `exe_basename` (a path or bare name) matches `glob`,
/// case-insensitively, with `*` (any run) and `?` (one char) wildcards.
///
/// A `.exe` on the process name is optional in the pattern, so `--match
/// overwatch` matches `Overwatch.exe` — consistent with the `--capture-hook`
/// target resolver.
pub fn matches(exe_basename: &str, glob: &str) -> bool {
    let name = basename(exe_basename).to_ascii_lowercase();
    let pattern = glob.to_ascii_lowercase();
    let stem = name.strip_suffix(".exe").unwrap_or(&name);
    glob_match(name.as_bytes(), pattern.as_bytes())
        || glob_match(stem.as_bytes(), pattern.as_bytes())
}

/// Byte-wise wildcard match with `*` and `?` (no backslash escaping).
fn glob_match(text: &[u8], pattern: &[u8]) -> bool {
    let (mut ti, mut pi) = (0, 0);
    let mut star = None;
    let mut resume = 0;
    while ti < text.len() {
        if pi < pattern.len() && (pattern[pi] == b'?' || pattern[pi] == text[ti]) {
            ti += 1;
            pi += 1;
        } else if pi < pattern.len() && pattern[pi] == b'*' {
            star = Some(pi);
            resume = ti;
            pi += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            resume += 1;
            ti = resume;
        } else {
            return false;
        }
    }
    pattern[pi..].iter().all(|&c| c == b'*')
}

/// Pick the target pid from the current foreground process and the full process
/// list.
///
/// With a glob: prefer the foreground process when it matches, otherwise the
/// first matching process. Without a glob: the foreground process (any), else
/// `None`.
pub fn select_target(
    glob: Option<&str>,
    foreground: Option<(u32, String)>,
    processes: &[(u32, String)],
) -> Option<(u32, String)> {
    match glob {
        Some(g) => {
            if let Some(fg) = &foreground {
                if matches(&fg.1, g) {
                    return Some(fg.clone());
                }
            }
            processes.iter().find(|(_, name)| matches(name, g)).cloned()
        }
        None => foreground,
    }
}

/// Shell/desktop processes that are never a capture target.
const SKIP_EXES: &[&str] = &[
    "explorer.exe",
    "cmd.exe",
    "powershell.exe",
    "pwsh.exe",
    "conhost.exe",
    "windowsterminal.exe",
    "wt.exe",
    "dwm.exe",
    "winlogon.exe",
    "csrss.exe",
    "wininit.exe",
    "services.exe",
    "lsass.exe",
    "smss.exe",
    "svchost.exe",
    "searchhost.exe",
    "startmenuexperiencehost.exe",
    "textinputhost.exe",
    "sihost.exe",
    "taskhostw.exe",
];

/// Why a process must never be injected. `None` means "safe to consider".
pub fn skip_reason(pid: u32, exe_basename: &str, own_pid: u32) -> Option<String> {
    if pid == 0 || pid == 4 {
        return Some("system pid".to_string());
    }
    if pid == own_pid {
        return Some("own pid".to_string());
    }
    let name = basename(exe_basename).to_ascii_lowercase();
    if SKIP_EXES.contains(&name.as_str()) {
        return Some(format!("shell/desktop process {name}"));
    }
    None
}

/// One step of the follow loop's decision, given the desired target and the
/// currently hooked one.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Step {
    /// Nothing to do this tick.
    Idle,
    /// The hooked target is no longer selected: unhook it.
    UnhookGone,
    /// No target is hooked and one is selected: inject it.
    Inject(u32, String),
    /// A different target is selected: unhook the old one and inject the new.
    Retarget(u32, String),
}

/// Decide the loop's next step from the desired target and the current one.
///
/// The same pid is a no-op (not a re-inject); a *different* selected pid is a
/// retarget; a dropped selection with something still hooked is an unhook.
fn next_action(desired: Option<(u32, String)>, current: Option<(u32, String)>) -> Step {
    match (desired, current) {
        (None, None) => Step::Idle,
        (None, Some(_)) => Step::UnhookGone,
        (Some((pid, exe)), None) => Step::Inject(pid, exe),
        (Some((pid, exe)), Some((cur, _))) if pid != cur => Step::Retarget(pid, exe),
        _ => Step::Idle,
    }
}

/// The currently hooked target.
struct Hooked {
    pid: u32,
    exe: String,
    /// True when *we* injected the recorder; false when it was already loaded and
    /// we are only observing it. A recorder we did not install is left alone.
    fresh: bool,
}

/// Owns the currently hooked target and unhooks it on drop, so no exit path
/// (Ctrl+C, early return, process exit) leaves a stale in-process patch.
struct FollowGuard {
    current: Option<Hooked>,
}

/// Release a hooked target and produce the lifecycle log line.
///
/// A target that has already exited took its in-process patches with it, so the
/// remote `mh_uninstall` is pointless — and fails, because the pid is gone. In
/// that case the remote call is skipped and the release is still reported as
/// `unhooked` (the target exited); otherwise the remote unhook's outcome is
/// reported.
fn release_target(
    exe: &str,
    pid: u32,
    process_alive: bool,
    remote_unhook: impl FnOnce() -> Result<(), String>,
) -> String {
    if !process_alive {
        return format!("unhooked {exe} (pid {pid}) (process already exited)");
    }
    match remote_unhook() {
        Ok(()) => format!("unhooked {exe} (pid {pid})"),
        Err(e) => format!("unhook skipped for {exe} (pid {pid}): {e}"),
    }
}

impl FollowGuard {
    /// Unhook the current target, returning the lifecycle log line, if any.
    fn unhook_current(&mut self) -> Option<String> {
        let h = self.current.take()?;
        if !h.fresh {
            // A recorder we did not install (a pre-existing injection) is left
            // alone — unhooking it would break another session's capture.
            return Some(format!(
                "left recorder in {} (pid {}) (not ours to unhook)",
                h.exe, h.pid
            ));
        }
        Some(release_target(
            &h.exe,
            h.pid,
            super::proc::process_alive(h.pid),
            || super::unhook(h.pid),
        ))
    }
}

impl Drop for FollowGuard {
    fn drop(&mut self) {
        // Backstop only; never panic from Drop. The main path logs the outcome.
        let _ = self.unhook_current();
    }
}

/// Lifecycle log line (routed to the console by the tracing writer).
fn log(msg: &str) {
    tracing::info!("follow: {msg}");
}

/// Print `msg` only when it differs from the previous status (avoids logging
/// the same "skipped"/"idle" line twice a second forever).
fn log_once(last: &mut Option<String>, msg: String) {
    if last.as_deref() != Some(msg.as_str()) {
        log(&msg);
        *last = Some(msg);
    }
}

/// Inject into `pid` and, on success, record it as the hooked target.
fn try_inject(
    guard: &mut FollowGuard,
    last_status: &mut Option<String>,
    refused: &mut Option<u32>,
    pid: u32,
    exe: String,
) {
    log_once(last_status, format!("target={exe} (pid {pid}); injecting"));
    let fresh = super::find_loaded_recorder(pid).is_none();
    match super::inject(pid) {
        Ok(mask) => {
            log_once(
                last_status,
                format!(
                    "injected into {exe} (pid {pid}) — installed hooks {} (mask {mask:#x})",
                    super::describe_installed(mask)
                ),
            );
            guard.current = Some(Hooked { pid, exe, fresh });
            *refused = None;
        }
        Err(e) => {
            log_once(last_status, format!("skipped {exe} (pid {pid}): {e}"));
            *refused = Some(pid);
        }
    }
}

/// The sleep between follow ticks. `--poll-ms 0` is clamped to 1 ms so the loop
/// cannot busy-spin; a huge value is kept (the loop simply waits).
fn poll_delay(poll_ms: u64) -> Duration {
    Duration::from_millis(poll_ms.max(1))
}

/// The bounded-follow deadline, or `None` when `secs` is absent or so large the
/// instant would overflow.
///
/// `secs` is user input: `Instant + Duration` panics with "overflow when adding
/// duration to instant" for an astronomically large bound, so a value that does
/// not fit is treated as no practical bound (run until Ctrl+C) rather than
/// crashing the host.
fn follow_deadline(now: std::time::Instant, secs: Option<u64>) -> Option<std::time::Instant> {
    secs.and_then(|s| now.checked_add(Duration::from_secs(s)))
}

/// Watch for a target and manage the present-hook injection lifecycle.
///
/// Stops on Ctrl+C, or after `secs` seconds when that is `Some` (a bounded
/// follow, which a non-interactive caller can drive to completion). Each
/// `poll_ms` tick it resolves the current target (foreground + optional glob),
/// then injects it if it changed and unhooks the previous one. It logs lifecycle
/// events and shows the hooked target's fps/frametime on the status line
/// (read-only) beside the hardware stats.
pub fn run(match_glob: Option<String>, poll_ms: u64, secs: Option<u64>) -> Result<(), String> {
    let poll = poll_delay(poll_ms);
    let own = std::process::id();
    super::install_ctrl_handler();

    let glob_desc = match_glob.as_deref().unwrap_or("<foreground>");
    let bound = secs.map(|s| format!(", secs={s}")).unwrap_or_default();
    log(&format!(
        "started (match={glob_desc}, poll={}ms, own pid={own}{bound})",
        poll.as_millis()
    ));

    let deadline = follow_deadline(std::time::Instant::now(), secs);

    let mut guard = FollowGuard { current: None };
    let mut last_status: Option<String> = None;
    let mut refused: Option<u32> = None;
    // Fps/frametime display for the currently-hooked target (updated each tick).
    let mut fps_reader: Option<super::FrameReader> = None;
    let mut fps_pid: u32 = 0;
    let mut last_seq: Option<u64> = None;

    while super::should_run() && deadline.is_none_or(|d| std::time::Instant::now() < d) {
        let foreground = super::proc::foreground_pid()
            .and_then(|pid| super::proc::process_name(pid).map(|name| (pid, name)));
        let processes = super::proc::list_processes();
        let chosen = select_target(match_glob.as_deref(), foreground, &processes);

        let desired = match chosen {
            None => None,
            Some((pid, exe)) => {
                if let Some(reason) = skip_reason(pid, &exe, own) {
                    log_once(
                        &mut last_status,
                        format!("skipped {exe} (pid {pid}): {reason}"),
                    );
                    None
                } else if refused == Some(pid) {
                    None
                } else {
                    Some((pid, exe))
                }
            }
        };

        let current = guard.current.as_ref().map(|h| (h.pid, h.exe.clone()));
        match next_action(desired, current.clone()) {
            Step::Idle => {}
            Step::UnhookGone => {
                if let Some((pid, exe)) = current {
                    log_once(&mut last_status, format!("target={exe} (pid {pid}) gone"));
                }
                if let Some(msg) = guard.unhook_current() {
                    log(&msg);
                }
                last_status = None;
                refused = None;
            }
            Step::Inject(pid, exe) => {
                try_inject(&mut guard, &mut last_status, &mut refused, pid, exe);
            }
            Step::Retarget(pid, exe) => {
                log_once(
                    &mut last_status,
                    format!("target changed to {exe} (pid {pid})"),
                );
                if let Some(msg) = guard.unhook_current() {
                    log(&msg);
                }
                try_inject(&mut guard, &mut last_status, &mut refused, pid, exe);
            }
        }

        // Keep the status line's fps segment current for the hooked target, so
        // the default `minihud` run shows fps/frametime beside the hardware
        // stats. (This is the one place follow touches the ring — read-only.)
        match guard.current.as_ref() {
            Some(h) => {
                if fps_pid != h.pid {
                    fps_reader = super::FrameReader::open(h.pid).ok();
                    fps_pid = h.pid;
                    last_seq = None;
                }
                if let Some(r) = &fps_reader {
                    super::print_sample(h.pid, r, &mut last_seq);
                }
            }
            None => {
                if fps_reader.is_some() {
                    fps_reader = None;
                    fps_pid = 0;
                    crate::console::status_capture("");
                }
            }
        }

        std::thread::sleep(poll);
    }

    let reason = if super::should_run() {
        "follow-secs elapsed"
    } else {
        "interrupt"
    };
    log(&format!("stopping ({reason})"));
    if let Some(msg) = guard.unhook_current() {
        log(&msg);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn follow_default_follows_the_foreground_on_the_default_cadence() {
        // Catches: a `Default` that polls at 0 ms (busy-spin) or picks a name
        // filter, when `--follow` is the default mode with no arguments.
        let d = FollowArgs::default();
        assert_eq!(d.match_glob, None);
        assert_eq!(d.poll_ms, DEFAULT_POLL_MS);
        assert_eq!(d.secs, None);
    }

    #[test]
    fn follow_flags_select_follow_without_the_explicit_flag() {
        // Catches: `--follow-secs`/`--match`/`--poll-ms` being ignored when
        // `--follow` is the default (the bound or name filter would silently do
        // nothing), while a non-follow command still yields `None`.
        assert_eq!(
            parse_follow(&args(&["--follow-secs", "5"])).map(|f| f.secs),
            Some(Some(5))
        );
        assert_eq!(
            parse_follow(&args(&["--match", "game.exe"])).map(|f| f.match_glob),
            Some(Some("game.exe".to_string()))
        );
        assert_eq!(
            parse_follow(&args(&["--poll-ms", "100"])).map(|f| f.poll_ms),
            Some(100)
        );
        assert_eq!(parse_follow(&args(&["--capture-hook", "1"])), None);
        assert_eq!(parse_follow(&args(&[])), None);
    }

    #[test]
    fn glob_matches_literal_names_case_insensitively() {
        assert!(matches("game.exe", "game.exe"));
        assert!(matches("Game.EXE", "game.exe"));
        assert!(!matches("game.dll", "game.exe"));
    }

    #[test]
    fn glob_tolerates_a_missing_exe_on_the_process_name() {
        // Catches: `--match overwatch` never matching `Overwatch.exe` (the follow
        // would silently never inject) while a genuinely different name still
        // fails.
        assert!(matches("Overwatch.exe", "overwatch"));
        assert!(matches("Overwatch.exe", "OVERWATCH"));
        assert!(matches("Overwatch.exe", "overwatch*"));
        assert!(!matches("Overwatch2.exe", "overwatch"));
        assert!(!matches("game.dll", "game"));
    }

    #[test]
    fn glob_star_matches_any_run_including_empty() {
        assert!(matches("deadlock.exe", "deadlock*.exe"));
        assert!(matches("DeadLock-shipping.exe", "deadlock*.exe"));
        assert!(!matches("deadlocked.dll", "deadlock*.exe"));
    }

    #[test]
    fn glob_question_matches_exactly_one_char() {
        assert!(matches("game.exe", "gam?.exe"));
        assert!(!matches("gam.exe", "gam?.exe"));
    }

    #[test]
    fn glob_applies_to_the_basename_of_a_full_path() {
        assert!(matches(r"C:\Games\Deadlock\deadlock.exe", "deadlock*.exe"));
        assert!(matches("/usr/bin/game.exe", "game.exe"));
    }

    #[test]
    fn select_prefers_a_matching_foreground_process() {
        let fg = Some((10, "Deadlock.exe".to_string()));
        let procs = vec![
            (10, "Deadlock.exe".to_string()),
            (20, "other.exe".to_string()),
        ];
        assert_eq!(
            select_target(Some("deadlock*.exe"), fg, &procs),
            Some((10, "Deadlock.exe".to_string()))
        );
    }

    #[test]
    fn select_falls_back_to_the_first_matching_process() {
        let fg = Some((1, "explorer.exe".to_string()));
        let procs = vec![
            (2, "deadlock.exe".to_string()),
            (3, "deadlock2.exe".to_string()),
        ];
        assert_eq!(
            select_target(Some("deadlock*.exe"), fg, &procs),
            Some((2, "deadlock.exe".to_string()))
        );
    }

    #[test]
    fn select_returns_none_when_no_process_matches_the_glob() {
        let fg = Some((1, "explorer.exe".to_string()));
        let procs = vec![(1, "explorer.exe".to_string())];
        assert_eq!(select_target(Some("deadlock*.exe"), fg, &procs), None);
    }

    #[test]
    fn select_without_a_glob_uses_the_foreground_process() {
        let fg = Some((5, "whatever.exe".to_string()));
        let procs = vec![(5, "whatever.exe".to_string())];
        assert_eq!(
            select_target(None, fg, &procs),
            Some((5, "whatever.exe".to_string()))
        );
    }

    #[test]
    fn select_without_a_glob_and_no_foreground_is_none() {
        assert_eq!(select_target(None, None, &[]), None);
    }

    #[test]
    fn skips_system_self_and_shell_processes() {
        assert!(skip_reason(0, "System", 100).is_some());
        assert!(skip_reason(4, "System", 100).is_some());
        assert!(skip_reason(100, "minihud.exe", 100).is_some());
        assert!(skip_reason(7, "explorer.exe", 100).is_some());
        assert!(skip_reason(8, "cmd.exe", 100).is_some());
    }

    #[test]
    fn does_not_skip_a_normal_target() {
        assert_eq!(skip_reason(42, "deadlock.exe", 100), None);
    }

    #[test]
    fn parses_follow_with_defaults() {
        assert_eq!(
            parse_follow(&args(&["--follow"])),
            Some(FollowArgs {
                match_glob: None,
                poll_ms: DEFAULT_POLL_MS,
                secs: None
            })
        );
    }

    #[test]
    fn parses_follow_with_match_and_poll() {
        assert_eq!(
            parse_follow(&args(&[
                "--follow",
                "--match",
                "deadlock*.exe",
                "--poll-ms",
                "300"
            ])),
            Some(FollowArgs {
                match_glob: Some("deadlock*.exe".to_string()),
                poll_ms: 300,
                secs: None
            })
        );
    }

    #[test]
    fn parse_follow_is_none_without_the_flag() {
        assert_eq!(parse_follow(&args(&["--capture-hook", "1"])), None);
    }

    #[test]
    fn parses_follow_secs_as_a_bounded_run() {
        // Catches: a `--follow-secs` that is ignored (the loop then never exits
        // without a Ctrl+C) or is confused with `--poll-ms`.
        assert_eq!(
            parse_follow(&args(&["--follow", "--follow-secs", "5"])),
            Some(FollowArgs {
                match_glob: None,
                poll_ms: DEFAULT_POLL_MS,
                secs: Some(5)
            })
        );
        assert_eq!(
            parse_follow(&args(&[
                "--follow",
                "--match",
                "g.exe",
                "--poll-ms",
                "100",
                "--follow-secs",
                "2"
            ])),
            Some(FollowArgs {
                match_glob: Some("g.exe".to_string()),
                poll_ms: 100,
                secs: Some(2)
            })
        );
        assert_eq!(
            parse_follow(&args(&["--follow"])).expect("follow").secs,
            None,
            "no bound by default"
        );
        assert_eq!(
            parse_follow(&args(&["--follow", "--follow-secs"])),
            None,
            "a valueless --follow-secs is rejected like --poll-ms"
        );
        assert_eq!(
            parse_follow(&args(&["--follow", "--follow-secs", "soon"])),
            None,
            "an unparseable bound is rejected"
        );
    }

    #[test]
    fn parse_follow_rejects_a_valueless_match() {
        assert_eq!(parse_follow(&args(&["--follow", "--match"])), None);
    }

    #[test]
    fn parse_follow_ignores_unknown_tokens_and_takes_the_last_value() {
        // Catches: an unknown token after `--follow` aborting the parse (the
        // whole follow config silently falls back to defaults) or a repeated flag
        // taking the first value instead of the last.
        let got = parse_follow(&args(&[
            "--follow", "--match", "a.exe", "--bogus", "--match", "b.exe",
        ]))
        .expect("parsed");
        assert_eq!(
            got.match_glob.as_deref(),
            Some("b.exe"),
            "the last --match wins and unknown tokens are ignored"
        );
    }

    #[test]
    fn release_of_an_exited_target_skips_the_remote_unhook() {
        // Catches: remote-calling `mh_uninstall` in a pid that already exited
        // (the observed `unhook skipped ... OpenProcess: invalid parameter`) and
        // reporting the lifecycle as a failure instead of "unhooked".
        let mut called = false;
        let line = release_target("game.exe", 7, false, || -> Result<(), String> {
            called = true;
            Ok(())
        });
        assert!(
            !called,
            "a target that already exited must not be remote-unhooked"
        );
        assert!(line.contains("unhooked game.exe (pid 7)"), "{line}");
        assert!(line.contains("exited"), "{line}");
    }

    #[test]
    fn release_of_a_live_target_reports_the_remote_outcome() {
        assert_eq!(
            release_target("game.exe", 7, true, || Ok(())),
            "unhooked game.exe (pid 7)"
        );
        let failed = release_target("game.exe", 7, true, || Err("boom".to_string()));
        assert!(failed.contains("unhook skipped"), "{failed}");
        assert!(failed.contains("boom"), "{failed}");
    }

    #[test]
    fn glob_star_matches_the_whole_name_including_the_empty_string() {
        assert!(matches("game.exe", "*"));
        assert!(matches("", "*"), "a lone star matches an empty name");
        assert!(matches("a", "**"));
        assert!(matches("game.exe", "*.exe"));
    }

    #[test]
    fn glob_directories_do_not_participate_in_the_match() {
        // Catches: matching the glob against a full path instead of the
        // basename, so a directory whose name happens to contain the pattern
        // selects the wrong process (or fails to match a real target).
        assert!(matches(r"C:\deadlock\deadlock.exe", "deadlock*.exe"));
        assert!(
            !matches(r"C:\deadlock\helper.exe", "deadlock*"),
            "a matching directory name must not make an unrelated exe match"
        );
        assert!(matches("C:/Games/Game.EXE", "game.exe"));
    }

    #[test]
    fn select_prefers_a_matching_foreground_even_when_a_child_also_matches() {
        // Catches: a selection that always takes process-list order (ignoring
        // the foreground), or returns None while a match exists. Here a
        // launcher `game.exe` (pid 10) and its child `game.exe` (pid 50) both
        // match the glob; the foreground (50) must win.
        let fg = Some((50, "game.exe".to_string()));
        let procs = vec![(10, "game.exe".to_string()), (50, "game.exe".to_string())];
        assert_eq!(
            select_target(Some("game.exe"), fg, &procs),
            Some((50, "game.exe".to_string()))
        );
    }

    #[test]
    fn parse_follow_accepts_the_poll_ms_bounds() {
        // Catches: rejecting `0` or a huge `--poll-ms`, which would silently
        // fall back to the default poll instead of honouring the request.
        assert_eq!(
            parse_follow(&args(&["--follow", "--poll-ms", "0"]))
                .unwrap()
                .poll_ms,
            0
        );
        let huge = u64::MAX.to_string();
        assert_eq!(
            parse_follow(&args(&["--follow", "--poll-ms", &huge]))
                .unwrap()
                .poll_ms,
            u64::MAX
        );
    }

    #[test]
    fn poll_delay_clamps_zero_to_one_ms_and_keeps_large_values() {
        // Catches: `--poll-ms 0` spinning the watch loop (busy-poll at 0 ms) and
        // a huge value overflowing the Duration conversion.
        assert_eq!(poll_delay(0), Duration::from_millis(1));
        assert_eq!(poll_delay(1), Duration::from_millis(1));
        assert_eq!(poll_delay(500), Duration::from_millis(500));
        assert_eq!(poll_delay(u64::MAX), Duration::from_millis(u64::MAX));
    }

    #[test]
    fn follow_deadline_handles_zero_absent_and_an_overflowing_bound() {
        // Catches: `--follow-secs <huge>` panicking the host with
        // "overflow when adding duration to instant" (reproduced standalone:
        // `Instant::now() + Duration::from_secs(u64::MAX)` panics). An
        // overflowing bound is treated as unbounded, not a crash.
        let now = std::time::Instant::now();
        assert_eq!(
            follow_deadline(now, None),
            None,
            "no bound means run until Ctrl+C"
        );
        assert_eq!(
            follow_deadline(now, Some(0)),
            Some(now),
            "zero seconds expires on the first tick"
        );
        assert!(
            follow_deadline(now, Some(60)).unwrap() > now,
            "a normal bound lands in the future"
        );
        assert_eq!(
            follow_deadline(now, Some(u64::MAX)),
            None,
            "an overflowing bound must not panic and is treated as unbounded"
        );
    }

    #[test]
    fn next_action_selects_inject_retarget_and_unhook() {
        // Catches: a follow loop that re-injects the same pid every tick, never
        // retargets when the pid changes, or leaves a stale hook when the
        // target is gone.
        let a = || (1u32, "a.exe".to_string());
        assert_eq!(next_action(None, None), Step::Idle);
        assert_eq!(next_action(None, Some(a())), Step::UnhookGone);
        assert_eq!(
            next_action(Some(a()), None),
            Step::Inject(1, "a.exe".into())
        );
        assert_eq!(
            next_action(Some((2, "b.exe".to_string())), Some(a())),
            Step::Retarget(2, "b.exe".into())
        );
        assert_eq!(
            next_action(Some(a()), Some(a())),
            Step::Idle,
            "the already-hooked pid is a no-op, not a re-inject"
        );
    }
}
