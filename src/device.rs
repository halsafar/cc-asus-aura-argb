// SPDX-License-Identifier: GPL-3.0-or-later
//
// One controller as presented to CoolerControl: its channels, what each one was
// last set to, and the only code paths that write to it (apply and save).

use crate::autosave::StateStore;
use crate::discovery::ControllerKey;
use crate::hid::{self, HidBus};
use crate::protocol::{self, ChannelKind, ChannelLayout, ConfigTable, Effect, Report, Rgb};
use log::{debug, info, warn};
use std::fmt;
use std::io;
use std::sync::{Mutex, MutexGuard};

/// What one channel should show: a built-in effect, run by the controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Setting {
    pub effect: Effect,
    /// Black for effects that take no colour.
    pub color: Rgb,
}

impl fmt::Display for Setting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.effect.takes_color() {
            write!(f, "{} #{}", self.effect, self.color)
        } else {
            write!(f, "{}", self.effect)
        }
    }
}

/// One controllable channel = one effect channel of the controller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Channel {
    /// `mainboard`, `argb1`, `argb2`, ...
    pub id: String,
    pub label: String,
    pub layout: ChannelLayout,
}

impl Channel {
    /// Validates a mode name and colours. Colours beyond the first are ignored, and
    /// so are colours sent with an effect that takes none.
    pub fn setting(&self, mode: &str, colors: &[Rgb]) -> Result<Setting, String> {
        let effect = Effect::from_name(mode).ok_or_else(|| format!("unknown mode '{mode}'"))?;
        let color = if effect.takes_color() {
            *colors
                .first()
                .ok_or_else(|| format!("mode '{mode}' needs one colour"))?
        } else {
            Rgb::BLACK
        };
        Ok(Setting { effect, color })
    }

    /// The two reports for `setting`. Pure. With `effect_only`, just the effect
    /// report (a diagnostic for the CLI; see docs/protocol.md).
    pub fn encode(&self, setting: &Setting, effect_only: bool) -> Vec<Report> {
        let [effect, colors] = protocol::encode_effect(&self.layout, setting.effect, setting.color);
        if effect_only {
            vec![effect]
        } else {
            vec![effect, colors]
        }
    }
}

/// Turns the decoded layout into channels with ids and labels.
///
/// Ids are a fixed word for the board's own LEDs and `argbN` for the controller's
/// N-th addressable header (effect channel N when the board has its own LEDs).
/// The controller's order need not match the silkscreen numbering; on the tested
/// board the case on one header turned out to be `argb2`.
pub fn build_channels(layouts: &[ChannelLayout]) -> Vec<Channel> {
    layouts
        .iter()
        .map(|l| match l.kind {
            ChannelKind::Mainboard { .. } => Channel {
                id: "mainboard".to_string(),
                label: "Mainboard LEDs".to_string(),
                layout: *l,
            },
            ChannelKind::Addressable { number } => Channel {
                id: format!("argb{number}"),
                label: format!("ARGB header {number}"),
                layout: *l,
            },
        })
        .collect()
}

/// Layout and channels from a config table, logging layout warnings.
pub fn channels_from_table(table: &ConfigTable) -> Vec<Channel> {
    let (layouts, warnings) = protocol::layout(table);
    for w in warnings {
        warn!("{w}");
    }
    build_channels(&layouts)
}

/// What the plugin knows a channel is showing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChannelState {
    /// Not written since the service started: whatever the controller had.
    Untouched,
    Set(Setting),
    /// The last write failed part-way; unknown.
    Failed,
}

/// Result of one compare-then-save attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveOutcome {
    /// A newer apply happened since this attempt was scheduled.
    Superseded,
    /// No successful apply is waiting to be saved.
    NothingPending,
    /// A channel is in an unknown state after a failed write: not saved.
    Blocked,
    /// The record says the controller already holds this. Nothing written.
    Unchanged,
    Saved,
    /// Writing the save report failed. Not retried until the next apply.
    Failed,
}

#[derive(Debug)]
pub enum ApplyError {
    UnknownChannel(String),
    Io(io::Error),
}

impl fmt::Display for ApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApplyError::UnknownChannel(c) => write!(f, "unknown channel '{c}'"),
            ApplyError::Io(e) => write!(f, "HID write failed: {e}"),
        }
    }
}

impl std::error::Error for ApplyError {}

struct Inner {
    bus: Box<dyn HidBus + Send>,
    /// Incremented on every apply attempt, successful or not.
    generation: u64,
    states: Vec<ChannelState>,
    /// Per channel: the controller is known to show `states[i]`, because this process
    /// wrote it and nothing since may have changed it. Cleared by [`AuraDevice::invalidate`].
    shown: Vec<bool>,
    /// An apply succeeded and no save attempt has handled it yet.
    pending: bool,
}

/// Static facts about a probed controller.
#[derive(Clone, Debug)]
pub struct DeviceInfo {
    /// Device id on the gRPC API, e.g. `aura-usb-3-2`.
    pub id: String,
    pub name: String,
    pub model: String,
    pub key: ControllerKey,
    pub firmware: String,
    /// Shown in CoolerControl's device details (node, sysfs path).
    pub locations: Vec<String>,
}

pub struct AuraDevice {
    pub info: DeviceInfo,
    pub channels: Vec<Channel>,
    inner: Mutex<Inner>,
}

impl AuraDevice {
    pub fn new(info: DeviceInfo, channels: Vec<Channel>, bus: Box<dyn HidBus + Send>) -> Self {
        let n = channels.len();
        AuraDevice {
            info,
            channels,
            inner: Mutex::new(Inner {
                bus,
                generation: 0,
                states: vec![ChannelState::Untouched; n],
                shown: vec![false; n],
                pending: false,
            }),
        }
    }

    pub fn id(&self) -> &str {
        &self.info.id
    }

    pub fn channel(&self, id: &str) -> Option<&Channel> {
        self.channels.iter().find(|c| c.id == id)
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Writes `reports` in order. If the node has gone away (USB re-enumeration,
    /// e.g. after resume), reopens it once and starts over.
    fn write(&self, inner: &mut Inner, reports: &[Report]) -> io::Result<()> {
        let once = |inner: &mut Inner| reports.iter().try_for_each(|r| inner.bus.write(r));
        match once(inner) {
            Err(e) if hid::is_gone(&e) => {
                warn!("{}: controller node gone ({e}); reopening", self.info.id);
                inner.bus.reopen()?;
                once(inner)
            }
            other => other,
        }
    }

    /// Index of the physical channel `id`.
    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.channels.iter().position(|c| c.id == id)
    }

    /// Forgets what the controller shows (CoolerControl initialised the device, or
    /// the system was suspended: the controller may have gone back to its saved
    /// settings). The next apply to each channel is written even if unchanged.
    pub fn invalidate(&self) {
        let mut inner = self.lock();
        inner.shown.iter_mut().for_each(|s| *s = false);
    }

    /// Applies `setting` to one channel, always writing. Blocking. Returns the
    /// apply's generation.
    pub fn apply(&self, channel_id: &str, setting: &Setting) -> Result<u64, ApplyError> {
        self.apply_with(channel_id, setting, false)
    }

    /// [`Self::apply`], optionally sending only the effect report (CLI diagnostic).
    pub fn apply_with(
        &self,
        channel_id: &str,
        setting: &Setting,
        effect_only: bool,
    ) -> Result<u64, ApplyError> {
        let index = self
            .index_of(channel_id)
            .ok_or_else(|| ApplyError::UnknownChannel(channel_id.to_string()))?;
        let generation = self.apply_many(&[(index, *setting)], true, effect_only)?;
        Ok(generation.expect("a forced apply always writes"))
    }

    /// Applies settings to several channels, in order, as one apply (one generation,
    /// so one auto-save at most). Unless `force`, a channel the controller is known
    /// to show already is skipped. Returns the generation, or `None` if nothing
    /// needed writing. Blocking.
    pub fn apply_many(
        &self,
        targets: &[(usize, Setting)],
        force: bool,
        effect_only: bool,
    ) -> Result<Option<u64>, ApplyError> {
        let mut inner = self.lock();
        let todo: Vec<(usize, Setting)> = targets
            .iter()
            .copied()
            .filter(|(i, s)| force || !inner.shown[*i] || inner.states[*i] != ChannelState::Set(*s))
            .collect();
        if todo.is_empty() {
            debug!(
                "{}: controller already shows this; nothing written",
                self.info.id
            );
            return Ok(None);
        }
        inner.generation += 1;
        for (index, setting) in todo {
            let reports = self.channels[index].encode(&setting, effect_only);
            match self.write(&mut inner, &reports) {
                Ok(()) => {
                    inner.states[index] = ChannelState::Set(setting);
                    inner.shown[index] = !effect_only;
                    inner.pending = true;
                }
                Err(e) => {
                    // Part of the setting may have been written.
                    inner.states[index] = ChannelState::Failed;
                    inner.shown[index] = false;
                    inner.pending = false;
                    return Err(ApplyError::Io(e));
                }
            }
        }
        Ok(Some(inner.generation))
    }

    /// The record lines for the current state, or `None` if a channel is unknown.
    fn snapshot(&self, states: &[ChannelState]) -> Option<Vec<String>> {
        self.channels
            .iter()
            .zip(states)
            .map(|(c, s)| match s {
                ChannelState::Untouched => Some(format!("{} unknown", c.id)),
                ChannelState::Set(Setting { effect, color }) => Some(format!(
                    "{} effect={:02x} channel={:02x} slots={}+{} color={color}",
                    c.id,
                    effect.value(),
                    c.layout.effect_channel,
                    c.layout.first_slot,
                    c.layout.slots,
                )),
                ChannelState::Failed => None,
            })
            .collect()
    }

    /// Compare-then-save, under the same lock as `apply`. `generation` is the apply
    /// that scheduled it (`None`: whatever is pending). Blocking.
    pub fn try_save(&self, store: &StateStore, generation: Option<u64>) -> SaveOutcome {
        let mut inner = self.lock();
        if generation.is_some_and(|g| g != inner.generation) {
            return SaveOutcome::Superseded;
        }
        if !inner.pending {
            return SaveOutcome::NothingPending;
        }
        inner.pending = false;
        let Some(snapshot) = self.snapshot(&inner.states) else {
            info!(
                "{}: not saving to the controller: a channel is in an unknown state after a \
                 failed write",
                self.info.id
            );
            return SaveOutcome::Blocked;
        };
        if store.load(&self.info.key).as_deref() == Some(snapshot.as_slice()) {
            debug!("{}: settings already saved on the controller", self.info.id);
            return SaveOutcome::Unchanged;
        }
        if let Err(e) = self.write(&mut inner, &[protocol::save_report()]) {
            warn!("{}: saving to the controller failed: {e}", self.info.id);
            return SaveOutcome::Failed;
        }
        info!(
            "{}: saved the effect settings to the controller's memory",
            self.info.id
        );
        if let Err(e) = store.store(&self.info.key, &snapshot) {
            warn!(
                "{}: saved, but cannot record it in {}: {e}",
                self.info.id,
                store.dir().display()
            );
        }
        SaveOutcome::Saved
    }

    /// Writes the save report unconditionally (CLI `save`, `set --save`).
    pub fn save_now(&self) -> io::Result<()> {
        let mut inner = self.lock();
        self.write(&mut inner, &[protocol::save_report()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hid::mock::{MockBus, Op};
    use crate::protocol::ConfigTable;

    fn device(bus: &MockBus) -> AuraDevice {
        let mut t = [0u8; 60];
        t[2] = 3;
        t[0x1B] = 1;
        AuraDevice::new(
            DeviceInfo {
                id: "aura-usb-3-2".into(),
                name: "test".into(),
                model: "test".into(),
                key: ControllerKey {
                    vendor: 0x0b05,
                    product: 0x19af,
                    usb_name: "3-2".into(),
                },
                firmware: "AULA3-TEST".into(),
                locations: vec![],
            },
            channels_from_table(&ConfigTable(t)),
            Box::new(bus.clone()),
        )
    }

    const RED: Rgb = Rgb::new(255, 0, 0);

    #[test]
    fn channel_model() {
        let d = device(&MockBus::new());
        let ids: Vec<(&str, &str, u8)> = d
            .channels
            .iter()
            .map(|c| (c.id.as_str(), c.label.as_str(), c.layout.effect_channel))
            .collect();
        assert_eq!(
            ids,
            vec![
                ("mainboard", "Mainboard LEDs", 0),
                ("argb1", "ARGB header 1", 1),
                ("argb2", "ARGB header 2", 2),
                ("argb3", "ARGB header 3", 3),
            ]
        );
    }

    #[test]
    fn setting_validation() {
        let d = device(&MockBus::new());
        let c = &d.channels[2];
        assert_eq!(
            c.setting("static", &[RED, Rgb::BLACK]).unwrap(),
            Setting {
                effect: Effect::Static,
                color: RED
            }
        );
        assert_eq!(
            c.setting("rainbow", &[RED]).unwrap(),
            Setting {
                effect: Effect::Rainbow,
                color: Rgb::BLACK
            }
        );
        assert!(c.setting("static", &[]).is_err());
        assert!(c.setting("music", &[]).is_err());
        assert!(c.setting("direct_fill", &[RED]).is_err());
    }

    #[test]
    fn apply_writes_effect_then_colours() {
        let bus = MockBus::new();
        let d = device(&bus);
        d.apply("argb2", &d.channels[2].setting("static", &[RED]).unwrap())
            .unwrap();
        assert_eq!(
            bus.writes_trimmed(),
            vec![
                vec![0xEC, 0x35, 0x02, 0, 0, 0x01],
                vec![0xEC, 0x36, 0, 0x04, 0, 0, 0, 0, 0, 0, 0, 0xff],
            ]
        );
        bus.clear();
        let rainbow = d.channels[2].setting("rainbow", &[]).unwrap();
        d.apply_with("argb2", &rainbow, true).unwrap();
        assert_eq!(
            bus.writes_trimmed(),
            vec![vec![0xEC, 0x35, 0x02, 0, 0, 0x05]]
        );
        assert!(matches!(
            d.apply("argb9", &rainbow),
            Err(ApplyError::UnknownChannel(_))
        ));
    }

    #[test]
    fn apply_many_skips_what_is_shown_until_invalidated() {
        let bus = MockBus::new();
        let d = device(&bus);
        let red = d.channels[0].setting("static", &[RED]).unwrap();
        let all: Vec<(usize, Setting)> = (0..4).map(|i| (i, red)).collect();
        assert_eq!(d.apply_many(&all, false, false).unwrap(), Some(1));
        assert_eq!(bus.writes().len(), 8);
        // Same again: nothing written, no new generation.
        assert_eq!(d.apply_many(&all, false, false).unwrap(), None);
        assert_eq!(bus.writes().len(), 8);
        // One channel differs: only it is written.
        let rainbow = d.channels[0].setting("rainbow", &[]).unwrap();
        assert_eq!(
            d.apply_many(&[(2, rainbow), (1, red)], false, false)
                .unwrap(),
            Some(2)
        );
        assert_eq!(bus.writes().len(), 10);
        // After invalidate everything is written again.
        d.invalidate();
        assert_eq!(d.apply_many(&all, false, false).unwrap(), Some(3));
        assert_eq!(bus.writes().len(), 18);
    }

    #[test]
    fn reopens_once_when_the_node_is_gone() {
        let bus = MockBus::new();
        let d = device(&bus);
        let s = d.channels[1].setting("static", &[RED]).unwrap();
        bus.gone_for(1);
        d.apply("argb1", &s).unwrap();
        let ops = bus.ops();
        assert_eq!(ops[0], Op::Reopen);
        assert_eq!(bus.writes().len(), 2);
        bus.gone_for(1);
        bus.reopen_fails(true);
        assert!(matches!(d.apply("argb1", &s), Err(ApplyError::Io(_))));
    }
}
