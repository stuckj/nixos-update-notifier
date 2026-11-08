//! Core logic for nixos-update-notifier.
//!
//! This crate is deliberately free of any GUI/tray dependencies (no GTK, no ksni) so it
//! builds and unit-tests on a bare machine with no system libraries. The daemon and the
//! GTK client both depend on it.

pub mod apply;
pub mod changelog;
pub mod check;
pub mod config;
pub mod diff;
pub mod failure;
pub mod lock;
pub mod nix;
pub mod pkgs;
pub mod progress;
pub mod release;

/// Scratch directories for unit tests.
///
/// Unit tests run as threads of one process, so a directory named from the PID alone is
/// shared by every test that picks the same name, and one test's cleanup deletes another's
/// fixture mid-run. Each call gets a directory of its own, removed when the guard drops
/// (also when the test panics).
#[cfg(test)]
pub(crate) mod testutil {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    pub struct ScratchDir(PathBuf);

    pub fn scratch_dir(tag: &str) -> ScratchDir {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("nun-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creating a test scratch dir");
        ScratchDir(dir)
    }

    impl std::ops::Deref for ScratchDir {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl AsRef<Path> for ScratchDir {
        fn as_ref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn every_scratch_dir_is_its_own() {
        let a = scratch_dir("same");
        let b = scratch_dir("same");
        assert_ne!(&*a, &*b);
        let path = a.to_path_buf();
        drop(a);
        assert!(!path.exists(), "dropping the guard removes the directory");
        assert!(b.exists(), "and leaves the other one alone");
    }
}

/// D-Bus well-known name, object path, and interface the daemon exposes and the GTK
/// client talks to. Kept here so both sides share one definition.
pub mod ipc {
    use crate::diff::PackageChange;
    use crate::failure::Failure;
    use crate::release::ReleaseOffer;
    use serde::{Deserialize, Serialize};

    pub const BUS_NAME: &str = "org.nixos.UpdateNotifier";
    pub const OBJECT_PATH: &str = "/org/nixos/UpdateNotifier";
    pub const INTERFACE: &str = "org.nixos.UpdateNotifier1";

    /// Daemon state beyond the status and the update list, served as JSON by
    /// `GetDetails`. Every field defaults, so either side can be older than the other.
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    #[serde(default)]
    pub struct Details {
        pub apply: ApplyInfo,
        /// Why the last check failed; `None` after a successful one.
        pub check_failure: Option<Failure>,
        /// The repo, with its own lock, describes a different system from the running one.
        pub running_differs: bool,
        /// Restoring the newest `flake.lock` backup would make the repo match the running
        /// system.
        pub can_revert_lock: bool,
        pub release: Option<ReleaseOffer>,
        /// Whether the user dismissed `release`'s newest version.
        pub release_dismissed: bool,
        pub upgrade: UpgradeInfo,
        /// What the running system lacks compared with the repo, when it is not applied.
        pub unapplied: Option<UnappliedDiff>,
    }

    /// The package difference between the running system and the repo as it is now.
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    #[serde(default)]
    pub struct UnappliedDiff {
        /// Empty with no `error`: only configuration differs, no package version.
        pub changes: Vec<PackageChange>,
        /// Why the difference could not be listed.
        pub error: Option<String>,
    }

    /// The current or most recent apply. `id` increases with every apply, so a window
    /// that followed one apply can tell that a later one has started.
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    #[serde(default)]
    pub struct ApplyInfo {
        pub id: u64,
        pub state: ApplyState,
        /// What kind of apply: "update", "rebuild" or "upgrade to 26.11".
        pub kind: String,
        pub failure: Option<Failure>,
    }

    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "kebab-case")]
    pub enum ApplyState {
        /// No apply has run since the daemon started.
        #[default]
        None,
        Running,
        Succeeded,
        Failed,
        /// The new system went live but the switch did not finish.
        PartlyApplied,
    }

    /// A release upgrade preview.
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    #[serde(default)]
    pub struct UpgradeInfo {
        pub state: UpgradeState,
        pub target: String,
        /// `(input, old ref, new ref)`.
        pub retargeted: Vec<(String, String, String)>,
        pub changes: Vec<PackageChange>,
        pub failure: Option<Failure>,
    }

    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "kebab-case")]
    pub enum UpgradeState {
        #[default]
        None,
        Previewing,
        Ready,
        Failed,
    }
}
