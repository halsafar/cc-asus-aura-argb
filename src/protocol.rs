// SPDX-License-Identifier: GPL-3.0-or-later
//
// ASUS Aura USB mainboard lighting controller protocol (USB 0b05:19af).
//
// Everything in this module is pure: it builds the HID reports to write and decodes
// the replies. No I/O happens here. See docs/protocol.md for the full description
// and what has been checked on hardware.
//
// liquidctl's `aura_led` driver and its device guide (GPL-3.0-or-later) were a source
// of protocol knowledge, checked by hand tests on an ASUS TUF GAMING Z890-PLUS WIFI;
// the code here is written from notes, not copied.

use std::fmt;

/// Every report the controller exchanges is report id `0xEC` plus 64 data bytes.
/// On hidraw a write and a read both carry the report id as their first byte.
pub const REPORT_LEN: usize = 65;
pub const REPORT_ID: u8 = 0xEC;

pub type Report = [u8; REPORT_LEN];

// Second byte of a report: the command, or for replies the reply id.
const REQ_FIRMWARE: u8 = 0x82;
const REQ_CONFIG: u8 = 0xB0;
const SET_EFFECT: u8 = 0x35;
const SET_EFFECT_COLORS: u8 = 0x36;
const SAVE: u8 = 0x3F;
const SAVE_MAGIC: u8 = 0x55;
const SET_DIRECT: u8 = 0x40;

/// Reply id of the firmware-version request.
pub const REPLY_FIRMWARE: u8 = 0x02;
/// Reply id of the config-table request.
pub const REPLY_CONFIG: u8 = 0x30;

/// Effect-mode value that hands a channel over to direct (per-LED) control.
pub const EFFECT_DIRECT: u8 = 0xFF;

/// Direct-mode flag in the header byte: last report of a frame, show it now.
pub const DIRECT_APPLY: u8 = 0x80;
/// Direct-mode flag in the header byte: the start offset is 256 + offset byte.
pub const DIRECT_HIGH_OFFSET: u8 = 0x10;
/// LEDs per direct-mode report: 5 header bytes + 20 * 3 colour bytes = 65.
pub const LEDS_PER_REPORT: usize = 20;
/// The start offset is one byte plus one high bit, so LED indices 0..=511.
pub const MAX_DIRECT_LEDS: u16 = 512;
/// Direct ids 0..=3 address addressable headers; 4 is the mainboard's own LEDs.
pub const DIRECT_ID_MAINBOARD: u8 = 0x04;

/// The effect colour report has a 16-bit slot mask, and the colours of slot `n`
/// sit at byte `5 + 3n`, so at most 16 slots can be addressed.
pub const MAX_COLOR_SLOTS: u8 = 16;

/// Builds a report: `REPORT_ID` followed by `body`, zero padded.
fn report(body: &[u8]) -> Report {
    debug_assert!(body.len() < REPORT_LEN);
    let mut r = [0u8; REPORT_LEN];
    r[0] = REPORT_ID;
    r[1..=body.len()].copy_from_slice(body);
    r
}

/// Asks for the firmware string. Reply id [`REPLY_FIRMWARE`].
pub fn firmware_request() -> Report {
    report(&[REQ_FIRMWARE])
}

/// Asks for the 60-byte configuration table. Reply id [`REPLY_CONFIG`].
pub fn config_request() -> Report {
    report(&[REQ_CONFIG])
}

/// Stores the current effect-mode settings in the controller, so it shows them from
/// power-on. Write endurance of that memory is unknown; see docs/design.md.
pub fn save_report() -> Report {
    report(&[SAVE, SAVE_MAGIC])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplyError {
    /// The first byte is not the report id `0xEC`.
    ReportId(u8),
    /// Report id is right but this is the reply to another request.
    ReplyId { expected: u8, got: u8 },
}

impl fmt::Display for ReplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReplyError::ReportId(id) => write!(f, "report id 0x{id:02x}, expected 0xec"),
            ReplyError::ReplyId { expected, got } => {
                write!(f, "reply id 0x{got:02x}, expected 0x{expected:02x}")
            }
        }
    }
}

impl std::error::Error for ReplyError {}

/// Checks that `reply` is the answer with reply id `expected`.
pub fn check_reply(reply: &Report, expected: u8) -> Result<(), ReplyError> {
    if reply[0] != REPORT_ID {
        return Err(ReplyError::ReportId(reply[0]));
    }
    if reply[1] != expected {
        return Err(ReplyError::ReplyId {
            expected,
            got: reply[1],
        });
    }
    Ok(())
}

/// Firmware string from the reply: bytes 2..18, up to the first NUL, non-printable
/// bytes replaced by `?`. Example from liquidctl's guide: `AULA3-AR32-0207`.
pub fn parse_firmware_reply(reply: &Report) -> Result<String, ReplyError> {
    check_reply(reply, REPLY_FIRMWARE)?;
    let raw = &reply[2..18];
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    Ok(raw[..end]
        .iter()
        .map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '?'
            }
        })
        .collect::<String>()
        .trim()
        .to_string())
}

/// The 60-byte configuration table, reply bytes 4..64.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfigTable(pub [u8; 60]);

/// Table offsets with a known meaning.
pub const TABLE_ARGB_HEADERS: usize = 0x02;
pub const TABLE_RGB_CHANNELS: usize = 0x03;
pub const TABLE_MAINBOARD_LEDS: usize = 0x1B;
pub const TABLE_RGB_HEADERS: usize = 0x1D;

pub fn parse_config_reply(reply: &Report) -> Result<ConfigTable, ReplyError> {
    check_reply(reply, REPLY_CONFIG)?;
    let mut t = [0u8; 60];
    t.copy_from_slice(&reply[4..64]);
    Ok(ConfigTable(t))
}

impl ConfigTable {
    /// Number of 5 V addressable (ARGB) headers. 3 on the tested board, matching the
    /// three effect channels that responded.
    pub fn argb_headers(&self) -> u8 {
        self.0[TABLE_ARGB_HEADERS]
    }

    /// What liquidctl reports as "RGB channels". The plugin does not use this byte.
    pub fn rgb_channels(&self) -> u8 {
        self.0[TABLE_RGB_CHANNELS]
    }

    /// Colour slots of the mainboard's own effect channel.
    /// 1 on the tested board, where effect channel 0 / slot 0 is the mainboard.
    pub fn mainboard_leds(&self) -> u8 {
        self.0[TABLE_MAINBOARD_LEDS]
    }

    /// Raw byte 0x1D. Possibly the number of 12 V RGB headers among the fixed LEDs,
    /// but the tested board reports 4 with 1 fixed LED ([`Self::mainboard_leds`]),
    /// which does not fit; its meaning is uncertain and the plugin does not use it.
    pub fn byte_1d(&self) -> u8 {
        self.0[TABLE_RGB_HEADERS]
    }

    /// The table in rows of six bytes, as liquidctl's guide prints it.
    pub fn rows(&self) -> impl Iterator<Item = &[u8]> {
        self.0.chunks(6)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelKind {
    /// The board's own LEDs (and probably any 12 V RGB headers, not verified on the
    /// tested board): one effect channel.
    Mainboard { leds: u8 },
    /// A 5 V addressable header, 1-based as presented to the user.
    Addressable { number: u8 },
}

/// How one controllable channel is addressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelLayout {
    pub kind: ChannelKind,
    /// Byte 2 of the effect report.
    pub effect_channel: u8,
    /// First colour slot in the effect colour report.
    pub first_slot: u8,
    /// Number of colour slots (the mainboard's LED count, 1 for an addressable header).
    pub slots: u8,
    /// Header id in direct-mode reports (not used by v1; see [`encode_direct`]).
    pub direct_id: Option<u8>,
}

/// Builds the channel list from the config table.
///
/// The mainboard channel comes first if the table lists any fixed LEDs; it takes
/// effect channel 0 and colour slots `0..leds`. Each addressable header follows with
/// the next effect channel and one colour slot, and direct id = header index.
/// Channels whose colour slots would not fit in the 16-bit mask are left out, and so
/// is direct mode for header indices that would collide with the mainboard's direct
/// id. Each such case adds a warning.
pub fn layout(table: &ConfigTable) -> (Vec<ChannelLayout>, Vec<String>) {
    let mut channels = Vec::new();
    let mut warnings = Vec::new();
    let mut effect_channel: u8 = 0;
    let mut slot: u8 = 0;

    let leds = table.mainboard_leds();
    if leds > 0 {
        if leds > MAX_COLOR_SLOTS {
            warnings.push(format!(
                "config table lists {leds} mainboard LEDs, more than the {MAX_COLOR_SLOTS} \
                 colour slots an effect report can address; mainboard channel left out"
            ));
        } else {
            channels.push(ChannelLayout {
                kind: ChannelKind::Mainboard { leds },
                effect_channel,
                first_slot: slot,
                slots: leds,
                direct_id: None,
            });
        }
        // The slots and the effect channel are taken whether or not it is exposed.
        effect_channel = 1;
        slot = leds;
    }

    for index in 0..table.argb_headers() {
        let number = index + 1;
        if u16::from(slot) >= u16::from(MAX_COLOR_SLOTS) {
            warnings.push(format!(
                "ARGB header {number}: colour slot {slot} does not fit in the effect report; \
                 left out"
            ));
        } else {
            let direct_id = if index < DIRECT_ID_MAINBOARD {
                Some(index)
            } else {
                warnings.push(format!(
                    "ARGB header {number}: direct id {index} would address the mainboard; \
                     no direct id"
                ));
                None
            };
            channels.push(ChannelLayout {
                kind: ChannelKind::Addressable { number },
                effect_channel,
                first_slot: slot,
                slots: 1,
                direct_id,
            });
        }
        effect_channel = effect_channel.saturating_add(1);
        slot = slot.saturating_add(1);
    }
    (channels, warnings)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const BLACK: Rgb = Rgb { r: 0, g: 0, b: 0 };

    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Rgb { r, g, b }
    }

    /// Parses `RRGGBB` or `#RRGGBB`.
    pub fn parse_hex(s: &str) -> Option<Rgb> {
        let s = s.strip_prefix('#').unwrap_or(s);
        if s.len() != 6 || !s.bytes().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let byte = |i: usize| u8::from_str_radix(&s[i..i + 2], 16).ok();
        Some(Rgb::new(byte(0)?, byte(2)?, byte(4)?))
    }
}

impl fmt::Display for Rgb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

/// The controller's built-in effects (effect-mode values). The values and names are
/// liquidctl's. 0x0E (possibly a music mode, not in liquidctl's list) is left out;
/// see docs/protocol.md.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Effect {
    Off,
    Static,
    Breathing,
    Flashing,
    SpectrumCycle,
    Rainbow,
    SpectrumCycleBreathing,
    ChaseFade,
    SpectrumCycleChaseFade,
    Chase,
    SpectrumCycleChase,
    SpectrumCycleWave,
    ChaseRainbowPulse,
    RainbowFlicker,
    GentleTransition,
    WavePropagation,
    WavePropagationPause,
    RedPulse,
}

impl Effect {
    pub const ALL: [Effect; 18] = [
        Effect::Off,
        Effect::Static,
        Effect::Breathing,
        Effect::Flashing,
        Effect::SpectrumCycle,
        Effect::Rainbow,
        Effect::SpectrumCycleBreathing,
        Effect::ChaseFade,
        Effect::SpectrumCycleChaseFade,
        Effect::Chase,
        Effect::SpectrumCycleChase,
        Effect::SpectrumCycleWave,
        Effect::ChaseRainbowPulse,
        Effect::RainbowFlicker,
        Effect::GentleTransition,
        Effect::WavePropagation,
        Effect::WavePropagationPause,
        Effect::RedPulse,
    ];

    /// Effect-mode byte.
    pub fn value(self) -> u8 {
        match self {
            Effect::Off => 0x00,
            Effect::Static => 0x01,
            Effect::Breathing => 0x02,
            Effect::Flashing => 0x03,
            Effect::SpectrumCycle => 0x04,
            Effect::Rainbow => 0x05,
            Effect::SpectrumCycleBreathing => 0x06,
            Effect::ChaseFade => 0x07,
            Effect::SpectrumCycleChaseFade => 0x08,
            Effect::Chase => 0x09,
            Effect::SpectrumCycleChase => 0x0A,
            Effect::SpectrumCycleWave => 0x0B,
            Effect::ChaseRainbowPulse => 0x0C,
            Effect::RainbowFlicker => 0x0D,
            Effect::GentleTransition => 0x10,
            Effect::WavePropagation => 0x11,
            Effect::WavePropagationPause => 0x12,
            Effect::RedPulse => 0x13,
        }
    }

    /// Mode id on the gRPC API and the CLI (liquidctl's names).
    pub fn name(self) -> &'static str {
        match self {
            Effect::Off => "off",
            Effect::Static => "static",
            Effect::Breathing => "breathing",
            Effect::Flashing => "flashing",
            Effect::SpectrumCycle => "spectrum_cycle",
            Effect::Rainbow => "rainbow",
            Effect::SpectrumCycleBreathing => "spectrum_cycle_breathing",
            Effect::ChaseFade => "chase_fade",
            Effect::SpectrumCycleChaseFade => "spectrum_cycle_chase_fade",
            Effect::Chase => "chase",
            Effect::SpectrumCycleChase => "spectrum_cycle_chase",
            Effect::SpectrumCycleWave => "spectrum_cycle_wave",
            Effect::ChaseRainbowPulse => "chase_rainbow_pulse",
            Effect::RainbowFlicker => "rainbow_flicker",
            Effect::GentleTransition => "gentle_transition",
            Effect::WavePropagation => "wave_propagation",
            Effect::WavePropagationPause => "wave_propagation_pause",
            Effect::RedPulse => "red_pulse",
        }
    }

    pub fn frontend_name(self) -> &'static str {
        match self {
            Effect::Off => "Off",
            Effect::Static => "Static",
            Effect::Breathing => "Breathing",
            Effect::Flashing => "Flashing",
            Effect::SpectrumCycle => "Spectrum Cycle",
            Effect::Rainbow => "Rainbow",
            Effect::SpectrumCycleBreathing => "Spectrum Cycle Breathing",
            Effect::ChaseFade => "Chase Fade",
            Effect::SpectrumCycleChaseFade => "Spectrum Cycle Chase Fade",
            Effect::Chase => "Chase",
            Effect::SpectrumCycleChase => "Spectrum Cycle Chase",
            Effect::SpectrumCycleWave => "Spectrum Cycle Wave",
            Effect::ChaseRainbowPulse => "Chase Rainbow Pulse",
            Effect::RainbowFlicker => "Rainbow Flicker",
            Effect::GentleTransition => "Gentle Transition",
            Effect::WavePropagation => "Wave Propagation",
            Effect::WavePropagationPause => "Wave Propagation Pause",
            Effect::RedPulse => "Red Pulse",
        }
    }

    /// Whether the effect uses the user's colour (liquidctl's list).
    pub fn takes_color(self) -> bool {
        matches!(
            self,
            Effect::Static
                | Effect::Breathing
                | Effect::Flashing
                | Effect::ChaseFade
                | Effect::Chase
        )
    }

    pub fn from_name(name: &str) -> Option<Effect> {
        Effect::ALL.into_iter().find(|e| e.name() == name)
    }

    pub fn from_value(value: u8) -> Option<Effect> {
        Effect::ALL.into_iter().find(|e| e.value() == value)
    }
}

impl fmt::Display for Effect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Effect report: `EC 35 <effect channel> 00 00 <mode>`.
pub fn effect_report(effect_channel: u8, mode: u8) -> Report {
    report(&[SET_EFFECT, effect_channel, 0x00, 0x00, mode])
}

/// Effect colour report: `EC 36 <mask hi> <mask lo> 00`, then three bytes R G B per
/// slot starting at byte `5 + 3 * first_slot`. Only the slots in the mask change.
pub fn effect_color_report(first_slot: u8, slots: u8, color: Rgb) -> Report {
    debug_assert!(slots >= 1 && u16::from(first_slot) + u16::from(slots) <= 16);
    let mask: u16 = (((1u32 << slots) - 1) << first_slot) as u16;
    let mut r = report(&[
        SET_EFFECT_COLORS,
        (mask >> 8) as u8,
        (mask & 0xFF) as u8,
        0x00,
    ]);
    for s in first_slot..first_slot + slots {
        let i = 5 + 3 * usize::from(s);
        r[i..i + 3].copy_from_slice(&[color.r, color.g, color.b]);
    }
    r
}

/// The two reports that put one channel into a built-in effect. Effects that do not
/// take a colour are sent with black, as liquidctl does.
pub fn encode_effect(ch: &ChannelLayout, effect: Effect, color: Rgb) -> [Report; 2] {
    let color = if effect.takes_color() {
        color
    } else {
        Rgb::BLACK
    };
    [
        effect_report(ch.effect_channel, effect.value()),
        effect_color_report(ch.first_slot, ch.slots, color),
    ]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectError {
    /// The channel has no direct id.
    NotAddressable,
    /// LED count 0 or above [`MAX_DIRECT_LEDS`].
    LedCount(usize),
}

impl fmt::Display for DirectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DirectError::NotAddressable => {
                f.write_str("direct mode is not available on this channel")
            }
            DirectError::LedCount(n) => {
                write!(f, "{n} LEDs: direct mode takes 1 to {MAX_DIRECT_LEDS}")
            }
        }
    }
}

impl std::error::Error for DirectError {}

/// Not used by v1 (direct mode is deferred); kept because it is complete and tested.
///
/// Direct-mode frame for one addressable header: the effect report that hands the
/// channel to direct control, then the LED colours in reports of up to 20 LEDs:
///
/// `EC 40 <header byte> <start offset> <count> R G B ...`
///
/// header byte = direct id, `| 0x10` when the start index is 256 or more (the offset
/// byte then holds start - 256), `| 0x80` on the last report of the frame.
pub fn encode_direct(ch: &ChannelLayout, colors: &[Rgb]) -> Result<Vec<Report>, DirectError> {
    let id = ch.direct_id.ok_or(DirectError::NotAddressable)?;
    let n = colors.len();
    if n == 0 || n > usize::from(MAX_DIRECT_LEDS) {
        return Err(DirectError::LedCount(n));
    }
    let mut reports = Vec::with_capacity(1 + n.div_ceil(LEDS_PER_REPORT));
    reports.push(effect_report(ch.effect_channel, EFFECT_DIRECT));
    for (k, chunk) in colors.chunks(LEDS_PER_REPORT).enumerate() {
        let start = k * LEDS_PER_REPORT;
        let mut header = id;
        if start >= 256 {
            header |= DIRECT_HIGH_OFFSET;
        }
        if start + chunk.len() == n {
            header |= DIRECT_APPLY;
        }
        let mut r = report(&[SET_DIRECT, header, (start % 256) as u8, chunk.len() as u8]);
        for (i, c) in chunk.iter().enumerate() {
            r[5 + 3 * i..8 + 3 * i].copy_from_slice(&[c.r, c.g, c.b]);
        }
        reports.push(r);
    }
    Ok(reports)
}

/// Formats bytes as space separated hex, e.g. `ec 35 00`.
pub fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Like [`hex`], but drops trailing zero bytes (shown as `..`), for logs.
pub fn hex_trimmed(bytes: &[u8]) -> String {
    let end = bytes.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    if end == bytes.len() {
        hex(bytes)
    } else {
        let mut s = hex(&bytes[..end]);
        if !s.is_empty() {
            s.push(' ');
        }
        s.push_str("..");
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A report written out by hand: the given leading bytes, then zeros.
    fn r(lead: &[u8]) -> Report {
        let mut x = [0u8; REPORT_LEN];
        x[..lead.len()].copy_from_slice(lead);
        x
    }

    /// Table from liquidctl's guide (ProArt Z690-Creator WiFi), 6-byte rows.
    const PROART: [u8; 60] = [
        0x1e, 0x9f, 0x02, 0x01, 0x00, 0x00, //
        0x78, 0x3c, 0x00, 0x01, 0x00, 0x00, //
        0x78, 0x3c, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x01, 0x03, 0x02, //
        0x01, 0xf4, 0x00, 0x00, 0x00, 0x00, //
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ];

    /// Table from liquidctl's guide for a Z490 board (0x18F3): six fixed LEDs.
    fn z490() -> [u8; 60] {
        let mut t = PROART;
        t[2] = 0x01;
        t[9] = 0x00;
        t[0x1B] = 0x06;
        t[0x1C] = 0x07;
        t
    }

    fn config_reply(table: &[u8; 60]) -> Report {
        let mut x = r(&[0xEC, 0x30]);
        x[4..64].copy_from_slice(table);
        x
    }

    #[test]
    fn requests() {
        assert_eq!(firmware_request(), r(&[0xEC, 0x82]));
        assert_eq!(config_request(), r(&[0xEC, 0xB0]));
        assert_eq!(save_report(), r(&[0xEC, 0x3F, 0x55]));
        for x in [firmware_request(), config_request(), save_report()] {
            assert_eq!(x.len(), 65);
        }
    }

    #[test]
    fn firmware_reply() {
        let mut x = r(&[0xEC, 0x02]);
        x[2..17].copy_from_slice(b"AULA3-AR32-0207");
        assert_eq!(parse_firmware_reply(&x).unwrap(), "AULA3-AR32-0207");
        // All 16 bytes used, non-printable replaced.
        x[2..18].copy_from_slice(b"AULA3-AR32-0207\x01");
        assert_eq!(parse_firmware_reply(&x).unwrap(), "AULA3-AR32-0207?");
        // Byte 18 onwards is not part of the string.
        x[17] = 0;
        x[18] = b'Z';
        assert_eq!(parse_firmware_reply(&x).unwrap(), "AULA3-AR32-0207");
        assert_eq!(
            parse_firmware_reply(&r(&[0xEC, 0x30])),
            Err(ReplyError::ReplyId {
                expected: 0x02,
                got: 0x30
            })
        );
        assert_eq!(
            parse_firmware_reply(&r(&[0x00, 0x02])),
            Err(ReplyError::ReportId(0))
        );
    }

    #[test]
    fn config_reply_and_fields() {
        let t = parse_config_reply(&config_reply(&PROART)).unwrap();
        assert_eq!(t.0, PROART);
        assert_eq!(t.argb_headers(), 2);
        assert_eq!(t.rgb_channels(), 1);
        assert_eq!(t.mainboard_leds(), 1);
        assert_eq!(t.byte_1d(), 2);
        assert_eq!(t.rows().count(), 10);
        assert_eq!(t.rows().nth(4).unwrap(), &[0, 0, 0, 1, 3, 2]);

        let z = parse_config_reply(&config_reply(&z490())).unwrap();
        assert_eq!(z.mainboard_leds(), 6);
        assert_eq!(z.byte_1d(), 2);
        assert_eq!(z.argb_headers(), 1);

        assert_eq!(
            parse_config_reply(&r(&[0xEC, 0x02])),
            Err(ReplyError::ReplyId {
                expected: 0x30,
                got: 0x02
            })
        );
        // Bytes 2 and 3 of the reply are not part of the table.
        let mut x = config_reply(&PROART);
        x[2] = 0xAA;
        x[3] = 0xBB;
        x[64] = 0xCC;
        assert_eq!(parse_config_reply(&x).unwrap().0, PROART);
    }

    #[test]
    fn layout_proart() {
        let (ch, warn) = layout(&ConfigTable(PROART));
        assert!(warn.is_empty());
        assert_eq!(
            ch,
            vec![
                ChannelLayout {
                    kind: ChannelKind::Mainboard { leds: 1 },
                    effect_channel: 0,
                    first_slot: 0,
                    slots: 1,
                    direct_id: None,
                },
                ChannelLayout {
                    kind: ChannelKind::Addressable { number: 1 },
                    effect_channel: 1,
                    first_slot: 1,
                    slots: 1,
                    direct_id: Some(0),
                },
                ChannelLayout {
                    kind: ChannelKind::Addressable { number: 2 },
                    effect_channel: 2,
                    first_slot: 2,
                    slots: 1,
                    direct_id: Some(1),
                },
            ]
        );
    }

    #[test]
    fn layout_six_fixed_leds() {
        let (ch, warn) = layout(&ConfigTable(z490()));
        assert!(warn.is_empty());
        assert_eq!(ch.len(), 2);
        assert_eq!(ch[0].kind, ChannelKind::Mainboard { leds: 6 });
        assert_eq!((ch[0].first_slot, ch[0].slots), (0, 6));
        // The header's colour slot comes after all six fixed LEDs.
        assert_eq!(
            (ch[1].effect_channel, ch[1].first_slot, ch[1].direct_id),
            (1, 6, Some(0))
        );
    }

    #[test]
    fn layout_without_fixed_leds() {
        let mut t = [0u8; 60];
        t[2] = 3;
        let (ch, warn) = layout(&ConfigTable(t));
        assert!(warn.is_empty());
        let got: Vec<(u8, u8, Option<u8>)> = ch
            .iter()
            .map(|c| (c.effect_channel, c.first_slot, c.direct_id))
            .collect();
        assert_eq!(got, vec![(0, 0, Some(0)), (1, 1, Some(1)), (2, 2, Some(2))]);
        // Empty table: nothing.
        assert!(layout(&ConfigTable([0; 60])).0.is_empty());
    }

    #[test]
    fn layout_limits() {
        // Five headers: the fifth would collide with the mainboard's direct id.
        let mut t = [0u8; 60];
        t[2] = 5;
        t[0x1B] = 1;
        let (ch, warn) = layout(&ConfigTable(t));
        assert_eq!(ch.len(), 6);
        assert_eq!(ch[5].direct_id, None);
        assert_eq!(ch[4].direct_id, Some(3));
        assert_eq!(warn.len(), 1);
        // 14 fixed LEDs + 3 headers: the third header's slot (16) does not fit.
        let mut t = [0u8; 60];
        t[2] = 3;
        t[0x1B] = 14;
        let (ch, warn) = layout(&ConfigTable(t));
        assert_eq!(ch.len(), 3);
        assert_eq!(warn.len(), 1);
        // 17 fixed LEDs: mainboard left out, headers too.
        let mut t = [0u8; 60];
        t[2] = 1;
        t[0x1B] = 17;
        let (ch, warn) = layout(&ConfigTable(t));
        assert!(ch.is_empty());
        assert_eq!(warn.len(), 2);
    }

    #[test]
    fn effect_reports() {
        let (ch, _) = layout(&ConfigTable(PROART));
        let red = Rgb::new(0xff, 0x00, 0x10);
        // Mainboard: effect channel 0, slot 0.
        assert_eq!(
            encode_effect(&ch[0], Effect::Static, red),
            [
                r(&[0xEC, 0x35, 0x00, 0x00, 0x00, 0x01]),
                r(&[0xEC, 0x36, 0x00, 0x01, 0x00, 0xff, 0x00, 0x10]),
            ]
        );
        // Second ARGB header: effect channel 2, slot 2 at bytes 11..14.
        assert_eq!(
            encode_effect(&ch[2], Effect::Breathing, red),
            [
                r(&[0xEC, 0x35, 0x02, 0x00, 0x00, 0x02]),
                r(&[
                    0xEC, 0x36, 0x00, 0x04, 0x00, 0, 0, 0, 0, 0, 0, 0xff, 0x00, 0x10
                ]),
            ]
        );
        // No-colour effect: black, regardless of the colour passed.
        assert_eq!(
            encode_effect(&ch[1], Effect::Rainbow, red),
            [
                r(&[0xEC, 0x35, 0x01, 0x00, 0x00, 0x05]),
                r(&[0xEC, 0x36, 0x00, 0x02, 0x00]),
            ]
        );
        assert_eq!(
            encode_effect(&ch[1], Effect::Off, red)[0],
            r(&[0xEC, 0x35, 0x01, 0x00, 0x00, 0x00])
        );
    }

    #[test]
    fn effect_color_report_multi_slot_mask() {
        // Six fixed LEDs: mask 0x003f, colour repeated in slots 0..6.
        let c = Rgb::new(1, 2, 3);
        let x = effect_color_report(0, 6, c);
        let mut want = r(&[0xEC, 0x36, 0x00, 0x3f, 0x00]);
        for s in 0..6 {
            want[5 + 3 * s..8 + 3 * s].copy_from_slice(&[1, 2, 3]);
        }
        assert_eq!(x, want);
        // Slot 15: mask high byte, colour at bytes 50..53.
        let x = effect_color_report(15, 1, c);
        assert_eq!(&x[..5], &[0xEC, 0x36, 0x80, 0x00, 0x00]);
        assert_eq!(&x[50..53], &[1, 2, 3]);
        assert!(x[53..].iter().all(|&b| b == 0));
        // Slots 8..10 straddle the mask bytes.
        let x = effect_color_report(8, 3, c);
        assert_eq!(&x[1..4], &[0x36, 0x07, 0x00]);
    }

    #[test]
    fn effect_table() {
        let rows: Vec<(&str, u8, bool)> = Effect::ALL
            .iter()
            .map(|e| (e.name(), e.value(), e.takes_color()))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("off", 0x00, false),
                ("static", 0x01, true),
                ("breathing", 0x02, true),
                ("flashing", 0x03, true),
                ("spectrum_cycle", 0x04, false),
                ("rainbow", 0x05, false),
                ("spectrum_cycle_breathing", 0x06, false),
                ("chase_fade", 0x07, true),
                ("spectrum_cycle_chase_fade", 0x08, false),
                ("chase", 0x09, true),
                ("spectrum_cycle_chase", 0x0A, false),
                ("spectrum_cycle_wave", 0x0B, false),
                ("chase_rainbow_pulse", 0x0C, false),
                ("rainbow_flicker", 0x0D, false),
                ("gentle_transition", 0x10, false),
                ("wave_propagation", 0x11, false),
                ("wave_propagation_pause", 0x12, false),
                ("red_pulse", 0x13, false),
            ]
        );
        for e in Effect::ALL {
            assert_eq!(Effect::from_name(e.name()), Some(e));
            assert_eq!(Effect::from_value(e.value()), Some(e));
            assert_ne!(e.value(), EFFECT_DIRECT);
        }
        assert_eq!(Effect::from_name("music"), None);
        assert_eq!(Effect::from_name("Static"), None);
    }

    fn ramp(n: usize) -> Vec<Rgb> {
        (0..n)
            .map(|i| Rgb::new(i as u8, (i >> 8) as u8, 0xAB))
            .collect()
    }

    #[test]
    fn direct_single_report() {
        let (ch, _) = layout(&ConfigTable(PROART));
        let x = encode_direct(&ch[2], &[Rgb::new(1, 2, 3), Rgb::new(4, 5, 6)]).unwrap();
        assert_eq!(
            x,
            vec![
                r(&[0xEC, 0x35, 0x02, 0x00, 0x00, 0xFF]),
                r(&[0xEC, 0x40, 0x81, 0x00, 0x02, 1, 2, 3, 4, 5, 6]),
            ]
        );
    }

    #[test]
    fn direct_chunking_and_apply_flag() {
        let (ch, _) = layout(&ConfigTable(PROART));
        let colors = ramp(45);
        let x = encode_direct(&ch[1], &colors).unwrap();
        assert_eq!(x.len(), 4);
        assert_eq!(x[0], r(&[0xEC, 0x35, 0x01, 0x00, 0x00, 0xFF]));
        let heads: Vec<[u8; 5]> = x[1..].iter().map(|p| p[..5].try_into().unwrap()).collect();
        assert_eq!(
            heads,
            vec![
                [0xEC, 0x40, 0x00, 0, 20],
                [0xEC, 0x40, 0x00, 20, 20],
                [0xEC, 0x40, 0x80, 40, 5],
            ]
        );
        // A full report: 20 LEDs fill bytes 5..65 exactly.
        assert_eq!(&x[1][5..8], &[0, 0, 0xAB]);
        assert_eq!(&x[1][62..65], &[19, 0, 0xAB]);
        assert_eq!(&x[2][5..8], &[20, 0, 0xAB]);
        // Partial last report: zero padded after LED 44.
        assert_eq!(&x[3][5..8], &[40, 0, 0xAB]);
        assert_eq!(&x[3][17..20], &[44, 0, 0xAB]);
        assert!(x[3][20..].iter().all(|&b| b == 0));
        // Exactly 20: one report, with the apply flag.
        let x = encode_direct(&ch[1], &ramp(20)).unwrap();
        assert_eq!(x.len(), 2);
        assert_eq!(&x[1][..5], &[0xEC, 0x40, 0x80, 0, 20]);
    }

    /// The table read from the tested board (TUF GAMING Z890-PLUS WIFI, firmware
    /// AULA3-AR32-0309).
    pub(crate) const Z890: [u8; 60] = [
        0x1e, 0x9f, 0x03, 0x01, 0x00, 0x00, //
        0x78, 0x3c, 0x00, 0x01, 0x00, 0x00, //
        0x78, 0x3c, 0x00, 0x01, 0x00, 0x00, //
        0x78, 0x3c, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x01, 0x04, 0x04, //
        0x01, 0xf4, 0x00, 0x00, 0x00, 0x00, //
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ];

    #[test]
    fn layout_tested_board() {
        let t = ConfigTable(Z890);
        assert_eq!(
            (t.argb_headers(), t.mainboard_leds(), t.byte_1d()),
            (3, 1, 4)
        );
        let (ch, warn) = layout(&t);
        assert!(warn.is_empty());
        let got: Vec<(ChannelKind, u8, u8, u8)> = ch
            .iter()
            .map(|c| (c.kind, c.effect_channel, c.first_slot, c.slots))
            .collect();
        assert_eq!(
            got,
            vec![
                (ChannelKind::Mainboard { leds: 1 }, 0, 0, 1),
                (ChannelKind::Addressable { number: 1 }, 1, 1, 1),
                (ChannelKind::Addressable { number: 2 }, 2, 2, 1),
                (ChannelKind::Addressable { number: 3 }, 3, 3, 1),
            ]
        );
    }

    /// Exactly the reports that turned the case (header 2) red in the manual test:
    /// effect channel 2 static, colour at slot 2 (bytes 11..14), mask bit 2.
    #[test]
    fn hand_test_header_2_red() {
        let (ch, _) = layout(&ConfigTable(Z890));
        assert_eq!(
            encode_effect(&ch[2], Effect::Static, Rgb::new(0xff, 0, 0)),
            [
                r(&[0xEC, 0x35, 0x02, 0x00, 0x00, 0x01]),
                r(&[
                    0xEC, 0x36, 0x00, 0x04, 0x00, 0, 0, 0, 0, 0, 0, 0xff, 0x00, 0x00
                ]),
            ]
        );
        // Header 1 green and header 3 blue, as sent alongside.
        assert_eq!(
            encode_effect(&ch[1], Effect::Static, Rgb::new(0, 0xff, 0))[1],
            r(&[0xEC, 0x36, 0x00, 0x02, 0x00, 0, 0, 0, 0, 0xff, 0])
        );
        assert_eq!(
            encode_effect(&ch[3], Effect::Static, Rgb::new(0, 0, 0xff))[1],
            r(&[
                0xEC, 0x36, 0x00, 0x08, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff
            ])
        );
    }

    #[test]
    fn direct_high_offsets() {
        let (ch, _) = layout(&ConfigTable(PROART));
        let x = encode_direct(&ch[2], &ramp(300)).unwrap();
        assert_eq!(x.len(), 1 + 15);
        let heads: Vec<(u8, u8, u8)> = x[1..].iter().map(|p| (p[2], p[3], p[4])).collect();
        let mut want = Vec::new();
        for k in 0..15u16 {
            let start = k * 20;
            let mut h = 0x01;
            if start >= 256 {
                h |= 0x10;
            }
            if k == 14 {
                h |= 0x80;
            }
            want.push((h, (start % 256) as u8, 20));
        }
        assert_eq!(heads, want);
        // LED 260 (start of report 13) is at offset byte 4 with the high flag.
        assert_eq!((x[14][2], x[14][3]), (0x11, 4));
        assert_eq!(&x[14][5..8], &[4, 1, 0xAB]);
        // The maximum is accepted, one more is not.
        let x = encode_direct(&ch[1], &ramp(512)).unwrap();
        let last = x.last().unwrap();
        assert_eq!((last[2], last[3], last[4]), (0x90, (500 - 256) as u8, 12));
        assert_eq!(
            encode_direct(&ch[1], &ramp(513)),
            Err(DirectError::LedCount(513))
        );
        assert_eq!(encode_direct(&ch[1], &[]), Err(DirectError::LedCount(0)));
        // The mainboard has no direct mode here.
        assert_eq!(
            encode_direct(&ch[0], &ramp(1)),
            Err(DirectError::NotAddressable)
        );
    }

    #[test]
    fn nothing_but_save_report_saves() {
        let (ch, _) = layout(&ConfigTable(PROART));
        let mut all: Vec<Report> = Vec::new();
        for c in &ch {
            for e in Effect::ALL {
                all.extend(encode_effect(c, e, Rgb::new(9, 9, 9)));
            }
            if c.direct_id.is_some() {
                all.extend(encode_direct(c, &ramp(512)).unwrap());
            }
        }
        assert!(all.iter().all(|x| x[0] == 0xEC && x[1] != 0x3F));
    }

    #[test]
    fn hex_and_colors() {
        assert_eq!(hex(&[0xec, 0x01, 0xff]), "ec 01 ff");
        assert_eq!(hex_trimmed(&[0xec, 0x35, 0, 0]), "ec 35 ..");
        assert_eq!(hex_trimmed(&[0xec, 0x35]), "ec 35");
        assert_eq!(hex_trimmed(&[0, 0]), "..");
        assert_eq!(Rgb::parse_hex("ff0010"), Some(Rgb::new(0xff, 0, 0x10)));
        assert_eq!(Rgb::parse_hex("#ABCDEF"), Some(Rgb::new(0xab, 0xcd, 0xef)));
        assert_eq!(Rgb::parse_hex("fff"), None);
        assert_eq!(Rgb::parse_hex("gg0000"), None);
        assert_eq!(Rgb::parse_hex("+f0000"), None);
        assert_eq!(Rgb::new(1, 2, 3).to_string(), "010203");
    }
}
