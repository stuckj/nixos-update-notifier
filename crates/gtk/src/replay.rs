//! Replay a recorded apply log into the progress window, to check its layout without
//! running an apply. Built only with the `replay` feature, which `nix/package.nix` enables
//! for canary versions.

use crate::ui::{run_progress, ApplyFeed};
use anyhow::{Context, Result};
use nun_core::ipc::{ApplyInfo, ApplyState};
use std::cell::Cell;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;

const APP_ID_REPLAY: &str = "org.nixos.UpdateNotifier.Replay";

/// Feeds `lines_per_poll` lines of the recorded log into a scratch file each time the
/// window polls, and reports the apply as running throughout, so whatever is in flight
/// when feeding stops stays on screen.
struct Replay {
    lines: Vec<String>,
    next: Cell<usize>,
    lines_per_poll: usize,
    out: PathBuf,
}

impl ApplyFeed for Replay {
    fn apply(&self) -> Option<ApplyInfo> {
        let start = self.next.get();
        let end = (start + self.lines_per_poll).min(self.lines.len());
        if end > start {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&self.out)
                .ok()?;
            for line in &self.lines[start..end] {
                writeln!(f, "{line}").ok()?;
            }
            self.next.set(end);
        }
        Some(ApplyInfo {
            id: 1,
            state: ApplyState::Running,
            kind: "replay".into(),
            failure: None,
        })
    }

    fn log_path(&self) -> Option<PathBuf> {
        Some(self.out.clone())
    }

    fn cancel(&self) {}
}

/// Show the progress window replaying `log`, feeding at most `until` lines.
pub fn run(log: &Path, lines_per_poll: usize, until: Option<usize>) -> Result<()> {
    let bytes = std::fs::read(log).with_context(|| format!("reading {}", log.display()))?;
    let mut lines: Vec<String> = String::from_utf8_lossy(&bytes)
        .lines()
        .map(str::to_owned)
        .collect();
    if let Some(n) = until {
        lines.truncate(n);
    }

    let out = std::env::temp_dir().join(format!("nun-replay-{}.log", std::process::id()));
    std::fs::File::create(&out).with_context(|| format!("creating {}", out.display()))?;
    let replay = Rc::new(Replay {
        lines,
        next: Cell::new(0),
        lines_per_poll: lines_per_poll.max(1),
        out: out.clone(),
    });

    let result = run_progress(APP_ID_REPLAY, move || {
        Some(replay.clone() as Rc<dyn ApplyFeed>)
    });
    let _ = std::fs::remove_file(out);
    result
}
