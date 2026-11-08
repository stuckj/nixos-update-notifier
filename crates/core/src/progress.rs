//! Live progress of a rebuild, from nix's `--log-format internal-json` stream.
//!
//! Nix's progress bar only appears on a TTY, but the same information is available as
//! machine-readable events: every line nix writes is `@nix {json}`, describing activities
//! (`start`/`stop`), their progress (`result`) and plain messages (`msg`). The rebuild
//! writes them to the apply log, and the client folds them into totals here. Root does no
//! parsing at all; it only passes the flag.
//!
//! Totals follow nix's own progress bar: per activity type, the counts of finished
//! activities accumulate, and the expected total is the larger of the sum over activities
//! and what a parent announced with `setExpected`.

use serde_json::Value;
use std::collections::{BTreeMap, HashMap, VecDeque};

/// Activity types (`ActivityType` in nix's `logging.hh`).
pub mod act {
    pub const COPY_PATH: u64 = 100;
    pub const FILE_TRANSFER: u64 = 101;
    pub const COPY_PATHS: u64 = 103;
    pub const BUILDS: u64 = 104;
    pub const BUILD: u64 = 105;
}

/// Result types (`ResultType` in nix's `logging.hh`).
mod res {
    pub const BUILD_LOG_LINE: u64 = 101;
    pub const SET_PHASE: u64 = 104;
    pub const PROGRESS: u64 = 105;
    pub const SET_EXPECTED: u64 = 106;
}

/// The most verbose message level shown as log text (nix's `lvlInfo`), matching what a
/// plain rebuild prints.
const LVL_INFO: u64 = 3;

/// Builder output kept per build, and how many finished builds' output is kept. With the
/// JSON log format nix's own build-failure error carries no builder output, so without
/// this a failed compile would report only "builder failed with exit code 1".
const LOG_TAIL_LINES: usize = 25;
const LOG_TAIL_BUILDS: usize = 64;

#[derive(Debug, Clone, Default)]
struct Activity {
    kind: u64,
    text: String,
    /// For a build: the derivation, and the last lines its builder printed.
    drv: Option<String>,
    log: VecDeque<String>,
    phase: Option<String>,
    done: u64,
    expected: u64,
    running: u64,
    failed: u64,
    /// `setExpected` announced by this activity, per child activity type.
    expected_by_type: HashMap<u64, u64>,
}

#[derive(Debug, Clone, Copy, Default)]
struct TypeTotals {
    done: u64,
    failed: u64,
    expected: u64,
}

/// Aggregate counts for one activity type.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub done: u64,
    pub expected: u64,
    pub running: u64,
    pub failed: u64,
}

impl Counts {
    pub fn fraction(&self) -> Option<f64> {
        (self.expected > 0).then(|| (self.done as f64 / self.expected as f64).min(1.0))
    }
}

/// Something in flight right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Current {
    pub kind: u64,
    pub text: String,
    pub phase: Option<String>,
    pub done: u64,
    pub expected: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Progress {
    activities: BTreeMap<u64, Activity>,
    totals: HashMap<u64, TypeTotals>,
    /// The output tail of recently finished builds, by derivation, oldest first.
    build_logs: VecDeque<(String, VecDeque<String>)>,
    /// Whether any `@nix` event has been seen, i.e. whether there is progress to show.
    pub structured: bool,
}

impl Progress {
    /// Fold one line of the log in. Returns the text to show in the log view, if any: plain
    /// lines pass through, events become their message or activity description.
    pub fn feed_line(&mut self, line: &str) -> Option<String> {
        let Some(json) = line.strip_prefix("@nix ") else {
            return Some(line.to_string());
        };
        let Ok(ev) = serde_json::from_str::<Value>(json) else {
            return Some(line.to_string());
        };
        self.structured = true;
        let u = |k: &str| ev.get(k).and_then(Value::as_u64);
        let id = u("id").unwrap_or(0);
        match ev.get("action").and_then(Value::as_str).unwrap_or("") {
            "msg" => {
                let level = u("level").unwrap_or(0);
                let msg = ev.get("msg").and_then(Value::as_str).unwrap_or("");
                (level <= LVL_INFO).then(|| self.with_build_log(strip_ansi(msg)))
            }
            "start" => {
                let kind = u("type").unwrap_or(0);
                let text = strip_ansi(ev.get("text").and_then(Value::as_str).unwrap_or(""));
                let level = u("level").unwrap_or(0);
                let shown = (level <= LVL_INFO && !text.is_empty()).then(|| text.clone());
                let drv = (kind == act::BUILD)
                    .then(|| {
                        ev.get("fields")
                            .and_then(|f| f.get(0))
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .flatten();
                self.activities.insert(
                    id,
                    Activity {
                        kind,
                        text,
                        drv,
                        ..Default::default()
                    },
                );
                shown
            }
            "stop" => {
                if let Some(a) = self.activities.remove(&id) {
                    if let (Some(drv), false) = (&a.drv, a.log.is_empty()) {
                        if self.build_logs.len() == LOG_TAIL_BUILDS {
                            self.build_logs.pop_front();
                        }
                        self.build_logs.push_back((drv.clone(), a.log.clone()));
                    }
                    let t = self.totals.entry(a.kind).or_default();
                    t.done += a.done;
                    t.failed += a.failed;
                    // What it announced was only a forecast; its children's own counts
                    // are what remain.
                    for (kind, n) in a.expected_by_type {
                        let t = self.totals.entry(kind).or_default();
                        t.expected = t.expected.saturating_sub(n);
                    }
                }
                None
            }
            "result" => {
                let fields = ev.get("fields").and_then(Value::as_array);
                let f = |i: usize| fields.and_then(|f| f.get(i)).and_then(Value::as_u64);
                let a = self.activities.get_mut(&id)?;
                match u("type") {
                    Some(res::PROGRESS) => {
                        a.done = f(0).unwrap_or(a.done);
                        a.expected = f(1).unwrap_or(a.expected);
                        a.running = f(2).unwrap_or(a.running);
                        a.failed = f(3).unwrap_or(a.failed);
                    }
                    Some(res::SET_EXPECTED) => {
                        if let (Some(kind), Some(n)) = (f(0), f(1)) {
                            let prev = a.expected_by_type.insert(kind, n).unwrap_or(0);
                            let t = self.totals.entry(kind).or_default();
                            t.expected = (t.expected + n).saturating_sub(prev);
                        }
                    }
                    Some(res::BUILD_LOG_LINE) if a.drv.is_some() => {
                        if let Some(line) = fields.and_then(|f| f.first()).and_then(Value::as_str) {
                            if a.log.len() == LOG_TAIL_LINES {
                                a.log.pop_front();
                            }
                            a.log.push_back(strip_ansi(line));
                        }
                    }
                    Some(res::SET_PHASE) => {
                        a.phase = fields
                            .and_then(|f| f.first())
                            .and_then(Value::as_str)
                            .map(str::to_string);
                    }
                    _ => {}
                }
                None
            }
            _ => None,
        }
    }

    /// Append the builder output of a build the message reports as failed, which nix's
    /// own message leaves out under the JSON log format.
    fn with_build_log(&self, msg: String) -> String {
        let drv = msg
            .split(['\'', ' ', '\n'])
            .find(|w| w.starts_with("/nix/store/") && w.ends_with(".drv"));
        let tail = drv.and_then(|d| {
            self.build_logs
                .iter()
                .rev()
                .find(|(drv, _)| drv == d)
                .map(|(_, log)| log)
        });
        match tail {
            Some(log) if msg.contains("failed") || msg.contains("Cannot build") => {
                let mut out = msg;
                out.push_str("\n       Last lines of the build log:");
                // Marked as nix marks builder output, so a builder's own `error: …` line
                // is not read as nix's error (which names the failed package).
                for l in log {
                    out.push_str("\n       > ");
                    out.push_str(l);
                }
                out
            }
            _ => msg,
        }
    }

    /// Totals for one activity type, e.g. `act::BUILDS`.
    pub fn counts(&self, kind: u64) -> Counts {
        let t = self.totals.get(&kind).copied().unwrap_or_default();
        let mut c = Counts {
            done: t.done,
            expected: t.done,
            running: 0,
            failed: t.failed,
        };
        for a in self.activities.values().filter(|a| a.kind == kind) {
            c.done += a.done;
            c.expected += a.expected;
            c.running += a.running;
            c.failed += a.failed;
        }
        c.expected = c.expected.max(t.expected);
        c
    }

    /// Paths being downloaded and derivations being built, oldest first.
    pub fn current(&self) -> Vec<Current> {
        self.activities
            .values()
            .filter(|a| matches!(a.kind, act::COPY_PATH | act::BUILD))
            .map(|a| Current {
                kind: a.kind,
                text: a.text.clone(),
                phase: a.phase.clone(),
                done: a.done,
                expected: a.expected,
            })
            .collect()
    }
}

/// Remove ANSI escape sequences; nix colours its messages even in JSON.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                // CSI: parameters and intermediates, then one final byte in @..~.
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Download rate and stall detection over a sliding window of byte-count samples.
#[derive(Debug, Clone, Default)]
pub struct RateMeter {
    samples: Vec<(f64, u64)>,
    last_change: Option<f64>,
}

impl RateMeter {
    const WINDOW_SECS: f64 = 10.0;

    /// Record the cumulative byte count at time `t` (seconds, any monotonic origin).
    pub fn record(&mut self, t: f64, bytes: u64) {
        let changed = self.samples.last().map_or(true, |(_, b)| *b != bytes);
        if changed {
            self.last_change = Some(t);
        }
        self.samples.push((t, bytes));
        self.samples.retain(|(s, _)| t - s <= Self::WINDOW_SECS);
    }

    /// Bytes per second over the window, once it spans at least a second.
    pub fn rate(&self) -> Option<f64> {
        let (t0, b0) = *self.samples.first()?;
        let (t1, b1) = *self.samples.last()?;
        (t1 - t0 >= 1.0).then(|| b1.saturating_sub(b0) as f64 / (t1 - t0))
    }

    /// Seconds since the byte count last moved.
    pub fn idle_for(&self, now: f64) -> f64 {
        self.last_change.map_or(0.0, |t| now - t)
    }
}

/// `1.6 GiB`, `812.0 MiB`, `3.2 KiB`.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64 / 1024.0;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    format!("{v:.1} {}", UNITS[u])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(p: &mut Progress, lines: &[&str]) -> Vec<String> {
        lines.iter().filter_map(|l| p.feed_line(l)).collect()
    }

    /// Real `nix build --log-format internal-json` output: four paths substituted from
    /// cache.nixos.org, with most intermediate progress results dropped.
    /// Pins the event shapes this module depends on.
    #[test]
    fn folds_a_real_substitution_log() {
        let text = include_str!("../tests/fixtures/nix-log/substitute.jsonl");
        let mut p = Progress::default();
        let shown: Vec<String> = text.lines().filter_map(|l| p.feed_line(l)).collect();

        assert!(p.structured);
        // What a plain rebuild would print: the summary and one line per path copied, not
        // the narinfo/nar downloads below it.
        assert_eq!(
            shown[0],
            "these 4 paths will be fetched (14.6 MiB download, 61.0 MiB unpacked):"
        );
        assert_eq!(
            shown
                .iter()
                .filter(|l| l.starts_with("copying path"))
                .count(),
            4
        );
        assert!(!shown.iter().any(|l| l.starts_with("downloading")));

        let paths = p.counts(act::COPY_PATHS);
        assert_eq!((paths.done, paths.expected, paths.running), (4, 4, 0));
        // 14.6 MiB of nars, plus the few KB of narinfos fetched before them.
        let dl = p.counts(act::FILE_TRANSFER);
        assert_eq!(dl.done, dl.expected);
        assert_eq!(human_bytes(dl.done), "14.6 MiB");
        assert!(p.current().is_empty());
    }

    #[test]
    fn a_failed_build_reports_what_its_builder_printed() {
        // Real `nix build --log-format internal-json` (nix 2.35) of a builder that prints a
        // compiler error and exits 1: nix's own error says only "builder failed with exit
        // code 1"; the compiler's line arrives as a build-log result before it.
        let text = include_str!("../tests/fixtures/nix-log/build-failure.jsonl");
        let mut p = Progress::default();
        let shown: Vec<String> = text.lines().filter_map(|l| p.feed_line(l)).collect();
        let error = shown
            .iter()
            .find(|l| l.starts_with("error: Cannot build"))
            .expect("nix's failure message");
        assert!(error.contains("builder failed with exit code 1"), "{error}");
        assert!(
            error.contains("foo.c:3: error: expected semicolon"),
            "builder output missing: {error}"
        );
        // Builder output is not streamed into the log as it happens.
        assert!(!shown.iter().any(|l| l == "compiling foo.c"));
        let failure = crate::failure::Failure::from_output("applying", &shown.join("\n"));
        assert!(
            failure.details.contains("foo.c:3: error"),
            "{}",
            failure.details
        );
    }

    /// The shape of a nixos-rebuild failure: the failing derivation is a dependency of the
    /// one requested (real nix 2.35 log). Its build stops before nix reports it, and the
    /// requested one then fails only because its dependency did.
    #[test]
    fn a_failed_dependency_reports_its_builder_output() {
        let text = include_str!("../tests/fixtures/nix-log/dependency-failure.jsonl");
        let mut p = Progress::default();
        let shown: Vec<String> = text.lines().filter_map(|l| p.feed_line(l)).collect();
        let dep = shown
            .iter()
            .find(|l| l.contains("Cannot build") && l.contains("nun-dep-fail.drv"))
            .expect("the dependency's failure");
        assert!(dep.contains("> dep.c:7: error: unknown type"), "{dep}");
        let top = shown
            .iter()
            .find(|l| l.contains("Cannot build") && l.contains("nun-top.drv"))
            .expect("the requested derivation's failure");
        assert!(!top.contains("Last lines"), "{top}");

        // The builder's own `error:` text does not displace nix's line as the summary.
        let failure = crate::failure::Failure::from_output("applying", &shown.join("\n"));
        assert!(
            failure.summary.contains("nun-dep-fail.drv"),
            "{}",
            failure.summary
        );
    }

    #[test]
    fn folds_a_real_build_log() {
        let text = include_str!("../tests/fixtures/nix-log/build.jsonl");
        let mut p = Progress::default();
        let shown: Vec<String> = text.lines().filter_map(|l| p.feed_line(l)).collect();
        assert!(shown.iter().any(|l| l.starts_with("building '/nix/store/")));
        let b = p.counts(act::BUILDS);
        assert_eq!((b.done, b.expected, b.failed), (1, 1, 0));
    }

    #[test]
    fn plain_lines_pass_through_untouched() {
        let mut p = Progress::default();
        let out = feed(
            &mut p,
            &["building the system configuration...", "@nix not json"],
        );
        assert_eq!(
            out,
            vec!["building the system configuration...", "@nix not json"]
        );
        assert!(!p.structured);
    }

    #[test]
    fn messages_are_shown_at_info_and_below_without_colour() {
        let mut p = Progress::default();
        let out = feed(
            &mut p,
            &[
                r#"@nix {"action":"msg","level":0,"msg":"\u001b[31;1merror:\u001b[0m boom"}"#,
                r#"@nix {"action":"msg","level":5,"msg":"chatty detail"}"#,
            ],
        );
        assert_eq!(out, vec!["error: boom"]);
    }

    #[test]
    fn totals_combine_finished_live_and_announced_counts() {
        let mut p = Progress::default();
        let out = feed(
            &mut p,
            &[
                // The parent announces the paths to copy, then each copy is an activity.
                r#"@nix {"action":"start","id":1,"level":0,"parent":0,"text":"","type":103,"fields":[]}"#,
                r#"@nix {"action":"result","id":1,"type":106,"fields":[100,3000]}"#,
                r#"@nix {"action":"result","id":1,"type":105,"fields":[1,5,2,0]}"#,
                r#"@nix {"action":"start","id":2,"level":3,"parent":1,"text":"copying path '/nix/store/a-electron-42' from 'https://cache.nixos.org'","type":100,"fields":[]}"#,
                r#"@nix {"action":"result","id":2,"type":105,"fields":[400,1000,0,0]}"#,
                r#"@nix {"action":"start","id":3,"level":3,"parent":1,"text":"copying path '/nix/store/b-hello' from 'https://cache.nixos.org'","type":100,"fields":[]}"#,
                r#"@nix {"action":"result","id":3,"type":105,"fields":[500,500,0,0]}"#,
                r#"@nix {"action":"stop","id":3}"#,
            ],
        );
        assert_eq!(
            out.len(),
            2,
            "the two copy activities are described: {out:?}"
        );

        assert_eq!(
            p.counts(act::COPY_PATHS),
            Counts {
                done: 1,
                expected: 5,
                running: 2,
                failed: 0
            }
        );
        // 500 finished + 400 live; expected is the announced 3000, not the 1500 summed.
        let bytes = p.counts(act::COPY_PATH);
        assert_eq!((bytes.done, bytes.expected), (900, 3000));

        let cur = p.current();
        assert_eq!(cur.len(), 1);
        assert!(cur[0].text.contains("electron"));
        assert_eq!((cur[0].done, cur[0].expected), (400, 1000));
    }

    #[test]
    fn builds_report_their_phase() {
        let mut p = Progress::default();
        feed(
            &mut p,
            &[
                r#"@nix {"action":"start","id":7,"level":3,"parent":0,"text":"building '/nix/store/x-foo.drv'","type":105,"fields":[]}"#,
                r#"@nix {"action":"result","id":7,"type":104,"fields":["buildPhase"]}"#,
            ],
        );
        assert_eq!(p.current()[0].phase.as_deref(), Some("buildPhase"));
    }

    #[test]
    fn a_repeated_set_expected_replaces_rather_than_adds() {
        let mut p = Progress::default();
        feed(
            &mut p,
            &[
                r#"@nix {"action":"start","id":1,"level":0,"parent":0,"text":"","type":104,"fields":[]}"#,
                r#"@nix {"action":"result","id":1,"type":106,"fields":[105,4]}"#,
                r#"@nix {"action":"result","id":1,"type":106,"fields":[105,6]}"#,
            ],
        );
        assert_eq!(p.counts(act::BUILD).expected, 6);
    }

    /// A substitution that falls back to building delivers less than it announced; once
    /// it stops, the announcement must not keep the total short of complete.
    #[test]
    fn a_stopped_announcement_no_longer_counts() {
        let mut p = Progress::default();
        feed(
            &mut p,
            &[
                r#"@nix {"action":"start","id":1,"level":0,"parent":0,"text":"","type":102,"fields":[]}"#,
                r#"@nix {"action":"result","id":1,"type":106,"fields":[101,1000]}"#,
                r#"@nix {"action":"start","id":2,"level":4,"parent":1,"text":"downloading","type":101,"fields":[]}"#,
                r#"@nix {"action":"result","id":2,"type":105,"fields":[600,600,0,0]}"#,
                r#"@nix {"action":"stop","id":2}"#,
            ],
        );
        assert_eq!(p.counts(act::FILE_TRANSFER).expected, 1000);
        p.feed_line(r#"@nix {"action":"stop","id":1}"#);
        let dl = p.counts(act::FILE_TRANSFER);
        assert_eq!((dl.done, dl.expected), (600, 600));
    }

    #[test]
    fn events_for_unknown_activities_are_ignored() {
        let mut p = Progress::default();
        assert_eq!(
            p.feed_line(r#"@nix {"action":"result","id":99,"type":105,"fields":[1,2,0,0]}"#),
            None
        );
        assert_eq!(p.feed_line(r#"@nix {"action":"stop","id":99}"#), None);
        assert_eq!(p.counts(act::BUILDS), Counts::default());
    }

    #[test]
    fn rate_and_stall() {
        let mut m = RateMeter::default();
        m.record(0.0, 0);
        assert_eq!(m.rate(), None);
        m.record(2.0, 2_000_000);
        assert_eq!(m.rate(), Some(1_000_000.0));
        m.record(5.0, 2_000_000);
        assert_eq!(m.idle_for(5.0), 3.0);
        // Samples older than the window drop out.
        m.record(20.0, 2_000_000);
        assert_eq!(m.rate(), None);
        assert_eq!(m.idle_for(20.0), 18.0);
    }

    #[test]
    fn byte_sizes_read_naturally() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(3 * 1024 + 200), "3.2 KiB");
        assert_eq!(human_bytes(812 * 1024 * 1024), "812.0 MiB");
        assert_eq!(human_bytes(1_717_986_918), "1.6 GiB");
    }

    #[test]
    fn strips_colour_codes() {
        assert_eq!(strip_ansi("\u{1b}[1mbold\u{1b}[0m text"), "bold text");
        assert_eq!(strip_ansi("plain"), "plain");
    }
}
