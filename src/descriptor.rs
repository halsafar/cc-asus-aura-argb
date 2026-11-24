// SPDX-License-Identifier: GPL-3.0-or-later
//
// Minimal HID report descriptor reader, used during discovery to make sure a hidraw
// node is the controller's lighting interface before it is opened. The descriptor is
// read from sysfs (kernel cache); this never talks to the device.

use std::collections::BTreeMap;
use std::fmt;

/// Vendor-defined usage page of the lighting interface (`06 72 FF`).
pub const LIGHTING_USAGE_PAGE: u32 = 0xFF72;

/// What the descriptor declares, per report id: input and output sizes in bits.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// Usage page in effect at the first main item (the application collection).
    pub usage_page: Option<u32>,
    pub input_bits: BTreeMap<u8, u32>,
    pub output_bits: BTreeMap<u8, u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DescriptorError {
    /// Item runs past the end, or long items (not used by this device).
    Malformed(usize),
    UsagePage(Option<u32>),
    /// Report `0xEC` missing, or not 64 bytes each way.
    Report {
        input_bits: u32,
        output_bits: u32,
    },
}

impl fmt::Display for DescriptorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DescriptorError::Malformed(at) => write!(f, "malformed descriptor at byte {at}"),
            DescriptorError::UsagePage(p) => match p {
                Some(p) => write!(
                    f,
                    "usage page 0x{p:04x}, expected 0x{LIGHTING_USAGE_PAGE:04x}"
                ),
                None => write!(f, "no usage page, expected 0x{LIGHTING_USAGE_PAGE:04x}"),
            },
            DescriptorError::Report {
                input_bits,
                output_bits,
            } => write!(
                f,
                "report 0xec has {input_bits} input and {output_bits} output bits, \
                 expected 512 each"
            ),
        }
    }
}

impl std::error::Error for DescriptorError {}

/// Walks the short items of a descriptor and adds up Input and Output sizes per
/// report id. Enough for the simple vendor interface this plugin talks to.
pub fn summarize(desc: &[u8]) -> Result<Summary, DescriptorError> {
    let mut s = Summary::default();
    let (mut page, mut size, mut count, mut id) = (None::<u32>, 0u32, 0u32, 0u8);
    let mut first_main = true;
    let mut i = 0;
    while i < desc.len() {
        let prefix = desc[i];
        if prefix == 0xFE {
            return Err(DescriptorError::Malformed(i));
        }
        let len = match prefix & 0x03 {
            3 => 4,
            n => usize::from(n),
        };
        if i + 1 + len > desc.len() {
            return Err(DescriptorError::Malformed(i));
        }
        let data = desc[i + 1..i + 1 + len]
            .iter()
            .rev()
            .fold(0u32, |acc, &b| (acc << 8) | u32::from(b));
        let kind = (prefix >> 2) & 0x03;
        let tag = prefix >> 4;
        match (kind, tag) {
            // Main items.
            (0, t) => {
                if first_main {
                    s.usage_page = page;
                    first_main = false;
                }
                let bits = size.saturating_mul(count);
                match t {
                    0x8 => *s.input_bits.entry(id).or_default() += bits,
                    0x9 => *s.output_bits.entry(id).or_default() += bits,
                    _ => {}
                }
            }
            // Global items.
            (1, 0x0) => page = Some(data),
            (1, 0x7) => size = data,
            (1, 0x8) => id = data as u8,
            (1, 0x9) => count = data,
            _ => {}
        }
        i += 1 + len;
    }
    Ok(s)
}

/// Accepts only the lighting interface: vendor usage page `0xFF72` and report `0xEC`
/// with 64-byte input and output reports.
pub fn check_lighting_interface(desc: &[u8]) -> Result<(), DescriptorError> {
    let s = summarize(desc)?;
    if s.usage_page != Some(LIGHTING_USAGE_PAGE) {
        return Err(DescriptorError::UsagePage(s.usage_page));
    }
    let input_bits = s.input_bits.get(&0xEC).copied().unwrap_or(0);
    let output_bits = s.output_bits.get(&0xEC).copied().unwrap_or(0);
    if input_bits != 512 || output_bits != 512 {
        return Err(DescriptorError::Report {
            input_bits,
            output_bits,
        });
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The 36-byte descriptor of the controller's interface 2 (0b05:19af).
    pub const AURA_19AF: [u8; 36] = [
        0x06, 0x72, 0xff, 0x09, 0xa1, 0xa1, 0x01, 0x85, 0xec, 0x09, 0x10, 0x15, 0x00, 0x26, 0xff,
        0x00, 0x75, 0x08, 0x95, 0x40, 0x81, 0x02, 0x09, 0x11, 0x15, 0x00, 0x26, 0xff, 0x00, 0x75,
        0x08, 0x95, 0x40, 0x91, 0x02, 0xc0,
    ];

    #[test]
    fn real_descriptor_is_accepted() {
        let s = summarize(&AURA_19AF).unwrap();
        assert_eq!(s.usage_page, Some(0xFF72));
        assert_eq!(s.input_bits.get(&0xEC), Some(&512));
        assert_eq!(s.output_bits.get(&0xEC), Some(&512));
        check_lighting_interface(&AURA_19AF).unwrap();
    }

    #[test]
    fn other_interfaces_are_rejected() {
        // A boot keyboard: generic desktop page.
        let kbd = [
            0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0xc0,
        ];
        assert_eq!(
            check_lighting_interface(&kbd),
            Err(DescriptorError::UsagePage(Some(1)))
        );
        // Right page, wrong report id.
        let mut d = AURA_19AF;
        d[8] = 0x5d;
        assert!(matches!(
            check_lighting_interface(&d),
            Err(DescriptorError::Report {
                input_bits: 0,
                output_bits: 0
            })
        ));
        // Right id, 32-byte output.
        let mut d = AURA_19AF;
        d[32] = 0x20;
        assert!(matches!(
            check_lighting_interface(&d),
            Err(DescriptorError::Report {
                input_bits: 512,
                output_bits: 256
            })
        ));
        // Truncated and empty.
        assert_eq!(
            check_lighting_interface(&AURA_19AF[..2]),
            Err(DescriptorError::Malformed(0))
        );
        assert_eq!(
            check_lighting_interface(&[]),
            Err(DescriptorError::UsagePage(None))
        );
        assert_eq!(
            check_lighting_interface(&[0xfe, 0x00]),
            Err(DescriptorError::Malformed(0))
        );
    }
}
