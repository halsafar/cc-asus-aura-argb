// SPDX-License-Identifier: GPL-3.0-or-later
//
// Which setting each physical channel should show when both its own setting and the
// `sync` ("All channels") setting exist: the most recent user change wins.
//
// CoolerControl stores one setting per channel and re-sends all of them at daemon
// start and after resume, in an order this plugin cannot rely on. So the plugin keeps
// its own record per logical channel (each physical channel and `sync`): the last
// requested setting and a sequence number from one increasing counter. A physical
// channel shows whichever of {its own record, the `sync` record} has the higher
// number. Re-sent settings that equal the record get no new number, so a re-apply
// storm cannot change the outcome; see `classify` and docs/design.md.
//
// The record is a small file in the state directory, written atomically, so the
// precedence survives restarts.

use crate::device::Setting;
use crate::discovery::ControllerKey;
use crate::protocol::{Effect, Rgb};
use log::warn;
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Id of the logical channel that covers every physical channel.
pub const SYNC_ID: &str = "sync";

const HEADER: &str = "cc-asus-aura-argb precedence v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    pub setting: Setting,
    pub seq: u64,
}

/// The records of one controller, keyed by logical channel id.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Records(pub BTreeMap<String, Record>);

/// A request on one logical channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    Set(Setting),
    /// CoolerControl's `ResetChannel`: forget this logical channel's setting.
    Reset,
}

/// How a `Set` request was treated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Same as the record, inside the re-apply window: no new sequence number.
    Reapply,
    /// A user change: recorded with a new sequence number.
    Changed,
    /// `Reset` of a logical channel that had a record: record dropped.
    Dropped,
    /// `Reset` of a logical channel without a record: nothing to do.
    Unchanged,
}

impl Records {
    pub fn get(&self, logical: &str) -> Option<&Record> {
        self.0.get(logical)
    }

    fn next_seq(&self) -> u64 {
        self.0.values().map(|r| r.seq).max().unwrap_or(0) + 1
    }

    /// Applies one request. `in_window`: the request arrived within the re-apply
    /// window (shortly after `InitializeDevice` or a detected resume).
    pub fn request(&mut self, logical: &str, req: Request, in_window: bool) -> Outcome {
        match req {
            Request::Set(s) => {
                if in_window && self.get(logical).is_some_and(|r| r.setting == s) {
                    return Outcome::Reapply;
                }
                let seq = self.next_seq();
                self.0
                    .insert(logical.to_string(), Record { setting: s, seq });
                Outcome::Changed
            }
            Request::Reset => {
                if self.0.remove(logical).is_some() {
                    Outcome::Dropped
                } else {
                    Outcome::Unchanged
                }
            }
        }
    }

    /// What physical channel `id` should show: the newer of its own and the `sync`
    /// record, or `None` if neither exists.
    pub fn effective(&self, id: &str) -> Option<Setting> {
        let own = self.get(id);
        let sync = self.get(SYNC_ID);
        match (own, sync) {
            (Some(o), Some(s)) => Some(if s.seq > o.seq { s.setting } else { o.setting }),
            (Some(r), None) | (None, Some(r)) => Some(r.setting),
            (None, None) => None,
        }
    }

    fn format(&self) -> String {
        let mut s = String::from(HEADER);
        s.push('\n');
        for (id, r) in &self.0 {
            s.push_str(&format!(
                "{id} {} effect={:02x} color={}\n",
                r.seq,
                r.setting.effect.value(),
                r.setting.color
            ));
        }
        s
    }

    fn parse(text: &str) -> Option<Records> {
        let mut lines = text.lines();
        if lines.next()? != HEADER {
            return None;
        }
        let mut map = BTreeMap::new();
        for line in lines.map(str::trim).filter(|l| !l.is_empty()) {
            let mut it = line.split_whitespace();
            let id = it.next()?;
            if !valid_id(id) {
                return None;
            }
            let seq: u64 = it.next()?.parse().ok()?;
            let effect = it.next()?.strip_prefix("effect=")?;
            let effect = Effect::from_value(u8::from_str_radix(effect, 16).ok()?)?;
            let color = Rgb::parse_hex(it.next()?.strip_prefix("color=")?)?;
            if it.next().is_some() {
                return None;
            }
            let color = if effect.takes_color() {
                color
            } else {
                Rgb::BLACK
            };
            map.insert(
                id.to_string(),
                Record {
                    setting: Setting { effect, color },
                    seq,
                },
            );
        }
        Some(Records(map))
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Where a controller's records live: a file in the state directory, or only memory
/// (no state directory; precedence then does not survive a service restart).
#[derive(Debug)]
pub struct PrecedenceStore {
    path: Option<PathBuf>,
    memory: Records,
}

impl PrecedenceStore {
    pub fn in_memory() -> Self {
        PrecedenceStore {
            path: None,
            memory: Records::default(),
        }
    }

    pub fn in_dir(dir: &Path, key: &ControllerKey) -> Self {
        PrecedenceStore {
            path: Some(path_for(dir, key)),
            memory: Records::default(),
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The current records. From the file when there is one, so changes made by the
    /// CLI are seen; a missing file is empty, an invalid one is logged and treated as
    /// empty (it is replaced at the next change).
    pub fn load(&mut self) -> Records {
        let Some(path) = &self.path else {
            return self.memory.clone();
        };
        match fs::read_to_string(path) {
            Ok(text) => match Records::parse(&text) {
                Some(r) => {
                    self.memory = r.clone();
                    r
                }
                None => {
                    warn!("Ignoring invalid precedence record {}", path.display());
                    Records::default()
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => Records::default(),
            Err(e) => {
                warn!(
                    "Cannot read {}: {e}; using the last known records",
                    path.display()
                );
                self.memory.clone()
            }
        }
    }

    /// Stores `records` (atomically when on disk). Memory is always updated, so a
    /// failed file write still gives the right answer until the service restarts.
    pub fn store(&mut self, records: &Records) -> io::Result<()> {
        self.memory = records.clone();
        let Some(path) = &self.path else {
            return Ok(());
        };
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        let result = (|| {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(records.format().as_bytes())?;
            f.sync_all()?;
            fs::rename(&tmp, path)?;
            if let Some(d) = path.parent().and_then(|d| fs::File::open(d).ok()) {
                let _ = d.sync_all();
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }
}

/// `<vendor>-<product>_usb-<usb name>.precedence`, next to the auto-save record.
pub fn path_for(dir: &Path, key: &ControllerKey) -> PathBuf {
    crate::autosave::StateStore::new(dir)
        .path_for(key)
        .with_extension("precedence")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(effect: Effect, r: u8) -> Setting {
        Setting {
            effect,
            color: if effect.takes_color() {
                Rgb::new(r, 0, 0)
            } else {
                Rgb::BLACK
            },
        }
    }

    #[test]
    fn newest_wins_and_reapply_keeps_numbers() {
        let rainbow = s(Effect::Rainbow, 0);
        let red = s(Effect::Static, 255);
        let mut r = Records::default();
        assert_eq!(
            r.request(SYNC_ID, Request::Set(rainbow), false),
            Outcome::Changed
        );
        assert_eq!(
            r.request("argb2", Request::Set(red), false),
            Outcome::Changed
        );
        assert_eq!(r.effective("argb2"), Some(red));
        assert_eq!(r.effective("argb1"), Some(rainbow));
        // Inside the window the same settings are re-applies, in either order.
        let before = r.clone();
        assert_eq!(
            r.request(SYNC_ID, Request::Set(rainbow), true),
            Outcome::Reapply
        );
        assert_eq!(
            r.request("argb2", Request::Set(red), true),
            Outcome::Reapply
        );
        assert_eq!(r, before);
        // Outside it, clicking sync again is a change and wins everywhere.
        assert_eq!(
            r.request(SYNC_ID, Request::Set(rainbow), false),
            Outcome::Changed
        );
        assert_eq!(r.effective("argb2"), Some(rainbow));
        // A different setting inside the window is a change too.
        assert_eq!(
            r.request("argb2", Request::Set(s(Effect::Static, 9)), true),
            Outcome::Changed
        );
        assert_eq!(r.effective("argb2"), Some(s(Effect::Static, 9)));
    }

    #[test]
    fn reset_semantics() {
        let rainbow = s(Effect::Rainbow, 0);
        let red = s(Effect::Static, 255);
        let mut r = Records::default();
        r.request(SYNC_ID, Request::Set(rainbow), false);
        r.request("argb2", Request::Set(red), false);
        // Resetting a channel lets sync show through.
        assert_eq!(r.request("argb2", Request::Reset, false), Outcome::Dropped);
        assert_eq!(r.effective("argb2"), Some(rainbow));
        assert_eq!(r.request("argb2", Request::Reset, true), Outcome::Unchanged);
        // Resetting sync leaves only the channels' own settings.
        r.request("argb2", Request::Set(red), false);
        assert_eq!(r.request(SYNC_ID, Request::Reset, false), Outcome::Dropped);
        assert_eq!(r.effective("argb2"), Some(red));
        assert_eq!(r.effective("argb1"), None);
    }

    #[test]
    fn file_round_trip_and_bad_files() {
        let dir = tempfile::tempdir().unwrap();
        let key = ControllerKey {
            vendor: 0x0b05,
            product: 0x19af,
            usb_name: "3-2".into(),
        };
        let mut st = PrecedenceStore::in_dir(dir.path(), &key);
        assert!(st.path().unwrap().ends_with("0b05-19af_usb-3-2.precedence"));
        assert_eq!(st.load(), Records::default());
        let mut r = Records::default();
        r.request(SYNC_ID, Request::Set(s(Effect::Rainbow, 0)), false);
        r.request("argb2", Request::Set(s(Effect::Breathing, 7)), false);
        st.store(&r).unwrap();
        assert_eq!(PrecedenceStore::in_dir(dir.path(), &key).load(), r);
        assert_eq!(
            fs::read_to_string(st.path().unwrap()).unwrap(),
            "cc-asus-aura-argb precedence v1\nargb2 2 effect=02 color=070000\n\
             sync 1 effect=05 color=000000\n"
        );
        for bad in [
            "",
            "nope\n",
            "cc-asus-aura-argb precedence v1\nsync x effect=05 color=000000\n",
            "cc-asus-aura-argb precedence v1\nsync 1 effect=0e color=000000\n",
            "cc-asus-aura-argb precedence v1\nsync 1 effect=05 color=zz\n",
            "cc-asus-aura-argb precedence v1\n../x 1 effect=05 color=000000\n",
            "cc-asus-aura-argb precedence v1\nsync 1 effect=05 color=000000 extra\n",
        ] {
            fs::write(st.path().unwrap(), bad).unwrap();
            assert_eq!(st.load(), Records::default(), "{bad:?}");
        }
        // Memory-only store.
        let mut m = PrecedenceStore::in_memory();
        m.store(&r).unwrap();
        assert_eq!(m.load(), r);
    }
}
