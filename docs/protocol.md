# ASUS Aura USB mainboard controller protocol

How the plugin talks to the ASUS "AURA LED Controller" (USB `0b05:19af`), and
what has been checked on hardware. The encoders and decoders are
pure functions in [`src/protocol.rs`](../src/protocol.rs), with unit tests for the
exact bytes.

What this description is checked against:

- **liquidctl**: the `aura_led` driver and its device guide (GPL-3.0-or-later). Its
  driver lists this exact USB id.
- **hardware**: requests sent by hand over raw hidraw on an ASUS TUF GAMING Z890-PLUS
  WIFI, firmware `AULA3-AR32-0309`, while CoolerControl's liquidctl daemon had the
  device open. Not with this plugin's binary.

## Transport

- Interface 2 of the USB device is a HID interface with a 36-byte report descriptor:
  vendor usage page `0xFF72`, usage `0xA1`, report id `0xEC`, 64-byte input and
  64-byte output reports. The plugin checks the descriptor (from sysfs) before opening the
  node, and the USB ids with `HIDIOCGRAWINFO` after opening it.
- Every report is 65 bytes on hidraw: `EC` (the report id) and 64 data bytes, zero
  padded. Reads return the same 65-byte layout. Below, byte 0 is always `EC`.
- Plain `write()`/`read()` on `/dev/hidrawN`. No feature reports, no libusb or hidapi.

## Reports

| Report | Bytes | Reply | Hardware |
|---|---|---|---|
| firmware request | `EC 82` | id `02`: bytes 2..18 = ASCII string | verified: `AULA3-AR32-0309` |
| config request | `EC B0` | id `30`: bytes 4..64 = 60-byte table | verified |
| effect select | `EC 35 <effect ch> 00 00 <mode>` | none | verified on ch 1, 2, 3 with mode `01` (liquidctl sends this report with a different byte 2, see below) |
| effect colours | `EC 36 <mask hi> <mask lo> 00 <RGB at 5+3·slot ...>` | none | verified on slots 1, 2, 3 |
| save | `EC 3F 55` | none | sent once (auto-save), no visible problem; whether the saved look shows at power-on is not verified |
| direct mode | `EC 40 <header byte> <offset> <count> <RGB × count>` | none | not used by v1, not verified on the tested board |

Firmware string: liquidctl takes bytes 2..17 (15 characters). The plugin takes 2..18
(16) and stops at the first NUL; on the tested board that gives the same string as
liquidctl.

Replies are matched by their id (byte 1). Anything else read while waiting is skipped
(see [design.md](design.md#coexistence-with-coolercontrols-built-in-driver)).

## Config table

Offsets are into the 60-byte table (reply bytes 4..64). Tested board:

```
00: 1e 9f 03 01 00 00
06: 78 3c 00 01 00 00
0c: 78 3c 00 01 00 00
12: 78 3c 00 00 00 00
18: 00 00 00 01 04 04
1e: 01 f4 00 00 00 00
(remaining rows zero)
```

| Offset | Meaning | Tested board |
|---|---|---|
| `0x02` | number of addressable (ARGB) headers | 3 (matches the three effect channels that responded) |
| `0x03` | "RGB channels" (liquidctl reports it; the plugin does not use it) | 1 |
| `0x1B` | colour slots of the mainboard's own effect channel ("mainboard LEDs") | 1 (effect channel 0 / slot 0 responded) |
| `0x1D` | possibly the number of 12 V RGB headers among those LEDs | 4, larger than `0x1B`, which does not fit that reading. **Meaning uncertain** on this firmware; the plugin does not use this byte. |
| others | unknown (the rows starting `78 3c` repeat once per header) | |

liquidctl's guide prints the same layout for its boards (`02 01` at `0x02`, and
`00 00 00 01 03 02` in the fifth row on a two-header board).

### Channels built from it

How the plugin numbers channels and colour slots (checked on hardware only for the
tested board's layout below):

1. If `0x1B` > 0: the mainboard channel, **effect channel 0**, colour slots
   `0 .. 0x1B-1`.
2. For each addressable header `i` (0-based, `0x02` of them): **effect channel**
   `i + 1` (or `i` without mainboard LEDs), **one colour slot** right after the
   previous channel's, direct id `i`.

On the tested board: `mainboard` = ch 0 / slot 0, `argb1` = ch 1 / slot 1, `argb2` = ch
2 / slot 2, `argb3` = ch 3 / slot 3. The hand test used exactly these numbers, and the
case (which hangs off one header) responded to ch 2 / slot 2.

The colour mask is 16 bits, so channels whose slots would go past 15 are left out (a
warning is logged). Not reached on any known board.

## Effect mode

To put one channel into an effect, two reports:

1. `EC 35 <effect channel> 00 00 <mode>`
2. `EC 36 <mask hi> <mask lo> 00`, then three bytes R G B for each slot `s` of the
   channel at bytes `5+3s .. 8+3s`; mask = one bit per slot (`1 << s`). Slots outside
   the mask are left alone, so channels are independent.

Example from the hand test, case red on header 2 (unit test `hand_test_header_2_red`):

```
EC 35 02 00 00 01
EC 36 00 04 00 00 00 00 00 00 00 FF 00 00
```

The change shows immediately; no save report is needed for it.

Byte 4 of both reports: `01` probably makes the reports set the mainboard's
"shutdown effect" (what its own LEDs show while the PC is off) instead of the running
one; not verified on the tested board. The plugin always sends `00`.

### Why liquidctl's driver keeps headers in sync

liquidctl sends the same effect report, but puts a "channel type" in byte 2: `00` for
its `led1`, `01` for `led2`..`led4`. Read as an effect channel, that selects only
channels 0 and 1. Colours go to the right slots (`led1`..`led4` = slots 0..3), so on a
board with three headers, headers 2 and 3 get colours but keep their previous effect
(rainbow from the factory). Confirmed on the tested board: after the hand test set
header 2 to static, CoolerControl's built-in `sync` static colour reached the case, but
its other effects did not. That is the problem this plugin fixes.

liquidctl's driver also ends every change with `EC 35 00 00 01 05` and `EC 3F 55`:
that is effect channel 0 with byte 4 = `01`, mode `05` (if byte 4 means what is
described above, the mainboard's shutdown effect set to rainbow; not verified),
followed by a save. The plugin sends neither on apply.

### Modes

Effect-mode values and names are liquidctl's. On the tested board `static` and
`rainbow` were verified; the other modes are not.

| Mode | Value | Colour |
|---|---|---|
| `off` | `00` | none |
| `static` | `01` | 1 |
| `breathing` | `02` | 1 |
| `flashing` | `03` | 1 |
| `spectrum_cycle` | `04` | none |
| `rainbow` | `05` | none |
| `spectrum_cycle_breathing` | `06` | none |
| `chase_fade` | `07` | 1 |
| `spectrum_cycle_chase_fade` | `08` | none |
| `chase` | `09` | 1 |
| `spectrum_cycle_chase` | `0A` | none |
| `spectrum_cycle_wave` | `0B` | none |
| `chase_rainbow_pulse` | `0C` | none |
| `rainbow_flicker` | `0D` | none |
| `gentle_transition` | `10` | none |
| `wave_propagation` | `11` | none |
| `wave_propagation_pause` | `12` | none |
| `red_pulse` | `13` | none |

Left out: `0E` (possibly a music mode; not in liquidctl's list and not verified on the
tested board) and `FF` (hands the channel to direct mode). Which modes take a colour
is liquidctl's list. No speed or direction byte is known for this controller's
effects, so CoolerControl shows no speed control.

### Colour-less effects

liquidctl sends the colour report for every effect, with black for effects without a
colour, and the plugin does the same. On the tested board `rainbow` with a black
colour report shows a rainbow; the other colour-less modes are not verified. Earlier,
through the built-in driver, the owner saw the case go white with colour-less modes,
but at that time the case's effect channel was never selected (see above). If a
colour-less effect shows black or white instead, try `set <channel> <mode>
--effect-only`, which sends only the effect report.

## Save

`EC 3F 55` stores the current effect settings in the controller. liquidctl sends it
after every change, and its guide says the settings then survive cold boot, warm boot
and sleep. What happens without it at power-off, and how many writes the memory takes,
is not known. On the tested board it was sent once with no visible problem; whether
the saved look shows at power-on is not verified. What the plugin does with it:
[design.md](design.md#persistence-and-auto-save).

## Direct mode (not used by v1)

`encode_direct` is implemented and unit-tested but not reachable from the service or
the CLI. For reference: the header's effect channel is set to mode `FF`, then the LED
colours go out in reports of up to 20 LEDs: `EC 40 <id | flags> <offset> <count>`
followed by R G B per LED. Flag `80` marks the last report of a frame (apply), flag
`10` means the start index is 256 + offset. Direct ids are the header index; `04` is
the mainboard. None of this is verified on the tested board. The hand test needed no
initialisation report before the effect reports. Per-LED data is not stored by the
save report.
