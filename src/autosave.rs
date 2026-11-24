// SPDX-License-Identifier: GPL-3.0-or-later
//
// Debounced, change-only saving of the effect-mode settings to the controller's
// memory (the save report `EC 3F 55`).
//
// The controller cannot be asked what it has stored, so the plugin keeps a record of
// what it believes is stored: one small file per controller in a state directory,
// one line per channel. A save is only written when the settings that have been in
// place for `delay` differ from that record, and never while any channel is in
// direct (per-LED) mode, which the controller's memory cannot hold.

use crate::device::{AuraDevice, SaveOutcome};
use crate::discovery::ControllerKey;
use log::{debug, warn};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// Default directory, matching `StateDirectory=cc-asus-aura-argb` in the unit.
pub const DEFAULT_STATE_DIR: &str = "/var/lib/cc-asus-aura-argb";
pub const DEFAULT_DELAY: Duration = Duration::from_secs(30);

const HEADER: &str = "cc-asus-aura-argb saved-state v1";

/// Per-controller records of the settings believed to be saved on it.
#[derive(Clone, Debug)]
pub struct StateStore {
    dir: PathBuf,
}

impl StateStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        StateStore { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `<vendor>-<product>_usb-<usb name>.saved`, e.g. `0b05-19af_usb-3-2.saved`.
    pub fn path_for(&self, key: &ControllerKey) -> PathBuf {
        let usb: String = key
            .usb_name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let usb = if usb.starts_with('.') {
            format!("_{usb}")
        } else {
            usb
        };
        self.dir.join(format!(
            "{:04x}-{:04x}_usb-{usb}.saved",
            key.vendor, key.product
        ))
    }

    /// The recorded lines, or `None` for "unknown" (missing, unreadable or invalid).
    pub fn load(&self, key: &ControllerKey) -> Option<Vec<String>> {
        let path = self.path_for(key);
        match fs::read_to_string(&path) {
            Ok(text) => {
                let parsed = parse(&text);
                if parsed.is_none() {
                    warn!("Ignoring invalid saved-state record {}", path.display());
                }
                parsed
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => {
                warn!("Cannot read {}: {e}", path.display());
                None
            }
        }
    }

    /// Records `lines` as saved. Atomic: temp file, fsync, rename.
    pub fn store(&self, key: &ControllerKey, lines: &[String]) -> io::Result<()> {
        let path = self.path_for(key);
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        let result = (|| {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(format(lines).as_bytes())?;
            f.sync_all()?;
            fs::rename(&tmp, &path)?;
            if let Ok(d) = fs::File::open(&self.dir) {
                let _ = d.sync_all();
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    /// Forgets the record, so the controller's memory counts as unknown.
    pub fn clear(&self, key: &ControllerKey) -> io::Result<()> {
        match fs::remove_file(self.path_for(key)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

fn format(lines: &[String]) -> String {
    let mut s = String::from(HEADER);
    s.push('\n');
    for l in lines {
        s.push_str(l);
        s.push('\n');
    }
    s
}

fn parse(text: &str) -> Option<Vec<String>> {
    let mut lines = text.lines();
    if lines.next()? != HEADER {
        return None;
    }
    let body: Vec<String> = lines
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    if body.is_empty() { None } else { Some(body) }
}

/// The CLI `save` and `set --save` commands: the CLI does not know every channel's
/// state, so the record is dropped and the service's belief becomes "unknown".
/// Returns `Ok(false)` if the state directory does not exist (nothing to update).
pub fn cli_forget(dir: &Path, key: &ControllerKey) -> io::Result<bool> {
    if !dir.is_dir() {
        return Ok(false);
    }
    StateStore::new(dir).clear(key)?;
    Ok(true)
}

/// The service's state directory: `--state-dir` (`flag`), else `$STATE_DIRECTORY`
/// (`env`, the first of a `:`-separated list), created if missing. `Err` says why
/// there is none.
pub fn state_dir(flag: Option<&Path>, env: Option<&std::ffi::OsStr>) -> Result<PathBuf, String> {
    let dir = match (flag, env) {
        (Some(d), _) => d.to_path_buf(),
        (None, Some(v)) => PathBuf::from(v.to_string_lossy().split(':').next().unwrap_or("")),
        (None, None) => {
            return Err(
                "no state directory (StateDirectory= in the unit, or --state-dir)".to_string(),
            );
        }
    };
    if dir.as_os_str().is_empty() {
        return Err("empty state directory".to_string());
    }
    fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Decides whether the service auto-saves and where its records live:
/// `--state-dir` (`flag`), else `$STATE_DIRECTORY` (`env`, the first of a
/// `:`-separated list). Without either, or with `disabled`, auto-save is off. A
/// directory that does not exist is created; if that fails, auto-save is off.
pub fn configure(
    disabled: bool,
    flag: Option<&Path>,
    env: Option<&std::ffi::OsStr>,
    delay: Duration,
) -> Option<Arc<AutoSave>> {
    if disabled {
        log::info!("Auto-save disabled (--no-auto-save)");
        return None;
    }
    let dir = match state_dir(flag, env) {
        Ok(d) => d,
        Err(why) => {
            warn!("Auto-save disabled: {why}");
            return None;
        }
    };
    log::info!(
        "Auto-save on: changed effect settings are saved to the controller after {}s \
         unchanged; records in {}",
        delay.as_secs(),
        dir.display()
    );
    Some(AutoSave::new(StateStore::new(dir), delay))
}

/// Schedules debounced saves for the gRPC service.
pub struct AutoSave {
    store: StateStore,
    delay: Duration,
    completed: watch::Sender<u64>,
}

impl AutoSave {
    pub fn new(store: StateStore, delay: Duration) -> Arc<Self> {
        Arc::new(AutoSave {
            store,
            delay,
            completed: watch::Sender::new(0),
        })
    }

    pub fn store(&self) -> &StateStore {
        &self.store
    }

    pub fn delay(&self) -> Duration {
        self.delay
    }

    /// Counts finished save attempts (any outcome). For tests and diagnostics.
    pub fn completed(&self) -> watch::Receiver<u64> {
        self.completed.subscribe()
    }

    /// Called after apply number `generation` succeeded. Every apply starts its own
    /// timer; only the one whose generation is still current when it fires acts, so
    /// the save happens `delay` after the last change on any channel.
    pub fn schedule(self: &Arc<Self>, dev: Arc<AuraDevice>, generation: u64) {
        let me = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(me.delay).await;
            me.run(dev, Some(generation)).await;
        });
    }

    /// Runs one compare-then-save attempt. `generation: None` means "whatever is
    /// pending now" (shutdown flush).
    pub async fn run(&self, dev: Arc<AuraDevice>, generation: Option<u64>) -> SaveOutcome {
        let store = self.store.clone();
        let id = dev.id().to_string();
        let outcome = tokio::task::spawn_blocking(move || dev.try_save(&store, generation))
            .await
            .unwrap_or_else(|e| {
                warn!("{id}: save task failed: {e}");
                SaveOutcome::Failed
            });
        debug!("{id}: auto-save attempt: {outcome:?}");
        self.completed.send_modify(|n| *n += 1);
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(usb: &str) -> ControllerKey {
        ControllerKey {
            vendor: 0x0b05,
            product: 0x19af,
            usb_name: usb.to_string(),
        }
    }

    fn lines() -> Vec<String> {
        vec![
            "mainboard effect=01 channel=00 slots=0+1 color=ff0000".to_string(),
            "argb1 unknown".to_string(),
        ]
    }

    #[test]
    fn round_trip_and_keying() {
        let dir = tempfile::tempdir().unwrap();
        let s = StateStore::new(dir.path());
        let a = key("3-2");
        assert_eq!(s.load(&a), None);
        s.store(&a, &lines()).unwrap();
        assert_eq!(s.load(&a), Some(lines()));
        assert_eq!(s.load(&key("3-3")), None);
        assert!(s.path_for(&a).ends_with("0b05-19af_usb-3-2.saved"));
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        s.clear(&a).unwrap();
        assert_eq!(s.load(&a), None);
        s.clear(&a).unwrap();
    }

    #[test]
    fn invalid_records_are_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let s = StateStore::new(dir.path());
        let a = key("3-2");
        for bad in [
            "",
            "garbage\n",
            "cc-asus-aura-argb saved-state v1\n",
            "cc-asus-aura-argb saved-state v2\nmainboard unknown\n",
        ] {
            fs::write(s.path_for(&a), bad).unwrap();
            assert_eq!(s.load(&a), None, "{bad:?}");
        }
        fs::remove_file(s.path_for(&a)).unwrap();
        fs::create_dir(s.path_for(&a)).unwrap();
        assert_eq!(s.load(&a), None);
    }

    #[test]
    fn path_is_sanitised() {
        let s = StateStore::new("/state");
        for evil in ["../../etc/x", "..", "a/b"] {
            let p = s.path_for(&key(evil));
            assert_eq!(p.parent(), Some(Path::new("/state")), "{evil}");
        }
    }

    #[test]
    fn cli_forget_helper() {
        let dir = tempfile::tempdir().unwrap();
        let a = key("3-2");
        StateStore::new(dir.path()).store(&a, &lines()).unwrap();
        assert!(cli_forget(dir.path(), &a).unwrap());
        assert_eq!(StateStore::new(dir.path()).load(&a), None);
        let missing = dir.path().join("missing");
        assert!(!cli_forget(&missing, &a).unwrap());
        assert!(!missing.exists());
    }
}
