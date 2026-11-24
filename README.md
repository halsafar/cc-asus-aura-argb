# cc-asus-aura-argb

A [CoolerControl](https://coolercontrol.org) plugin for the lighting controller built
into ASUS mainboards (USB "AURA LED Controller", `0b05:19af`). It gives every channel
of the controller its own effect: the board's own LEDs and each 5 V addressable (ARGB)
header can run a different mode and colour.

## Why use this instead of CoolerControl's built-in Aura device

CoolerControl already lists this controller as "ASUS Aura LED Controller", through
liquidctl's `AuraLed` driver. Use this plugin instead if any of these apply:

- **Your case or fans ignore the effect you pick.** The built-in driver selects the
  effect only on the first addressable header. On a board with two or three headers,
  the others receive colours but keep whatever effect they had (rainbow from the
  factory). Fans or a case hub cabled to header 2 or 3 stay on rainbow, or change
  colour but never switch to breathing, rainbow or any other effect. This plugin
  selects the effect on every header by its own index.
- **You want different lighting on different headers.** The built-in driver keeps all
  channels on one effect. Here the board's own LEDs and each header are separate
  channels.
- **You want fewer writes to the controller's memory.** The built-in driver saves to
  the controller on every change. This plugin saves once, 30 seconds after the last
  change, and only if something actually changed.

If your board has a single addressable header and one look everywhere is all you want,
the built-in device does the job and you don't need this.

There is still a `sync` channel ("All channels") that sets every channel at once, like
the built-in driver's. Whichever you changed last wins, per channel, and that survives
restarts and resume ([how](#the-sync-channel)).

What it does not do (yet): per-LED colours or patterns (direct mode), speed or
direction control (the controller has none for its effects), or brightness.

**Design choice: no polling, no software animation.** The plugin sends USB traffic only
when a setting is applied (and, debounced, to save it; see below). Every effect is run
by the controller itself. Nothing is sent periodically, so the plugin cannot add USB
traffic or input latency while you play.

## Supported hardware

| Controller | USB ID | Tested on |
|---|---|---|
| ASUS AURA LED Controller (mainboard) | `0b05:19af` | ASUS TUF GAMING Z890-PLUS WIFI, firmware `AULA3-AR32-0309`, bcdDevice `0100` |

Check yours with `lsusb -d 0b05:19af`. Other ASUS Aura USB controllers (liquidctl
also lists `0b05:1939` and `0b05:18f3`) are not enabled; their protocol may differ.

**Status** on the tested board (CoolerControl 5.1.2 in a container):

| Verified on hardware | Not yet run on hardware |
|---|---|
| `probe`: firmware `AULA3-AR32-0309`, config table, channels `mainboard argb1 argb2 argb3` | `argb1` / `argb3` from the plugin, and what `mainboard`, `argb1`, `argb3` physically light |
| `set argb2 static --color ff0000` (case red), `set argb2 rainbow` (case rainbow, colour report black as sent) | `sync` precedence across a CoolerControl restart and across suspend/resume |
| install with `make install` (manifest placed for the container), unit running under its sandbox | reboot |
| CoolerControl lists the device; from the UI: `mainboard` rainbow, `argb2` static green, `argb2` rainbow | whether the saved look shows at power-on |
| the `sync` channel from the UI: static red, then rainbow, on all four channels | the `save` and `set --save` commands |
| auto-save: one save report 30 s after the last change, no visible problem | |
| reads work while CoolerControl's liquidctl daemon has the device open | |

## Before you start

- **Wiring.** Case fans and strips with 3-pin 5 V ARGB connectors must be cabled to the
  board's 5 V addressable header (`ADD_GEN2` or similar, 3 pins: 5V, D, G), or to a hub
  that is. Never connect 5 V LEDs to a 12 V RGB header (4 pins: 12V, G, R, B): that
  destroys them. If the case has its own hub with a mode switch (for example be quiet!
  Light Base cases), put it in motherboard-sync / ARGB-in mode, so the board's header
  drives it.
- **Disable CoolerControl's built-in Aura device.** This is required, not optional.
  CoolerControl's liquidctl integration also writes to this controller; with both
  active, whichever wrote last wins and channels flip between settings. See
  [Disable the built-in device](#disable-the-built-in-aura-device).

## Install

The plugin runs as its own systemd service on the host; CoolerControl connects to it
through a socket. Releases ship a static x86-64 binary that runs on any Linux
distribution with systemd.

1. Download the archive and its checksum from the
   [latest release](https://github.com/halsafar/cc-asus-aura-argb/releases/latest),
   check it, and unpack it:

   ```sh
   V=0.1.0   # the release version
   base=https://github.com/halsafar/cc-asus-aura-argb/releases/download/v$V
   curl -LO $base/cc-asus-aura-argb-$V-x86_64-linux.tar.gz
   curl -LO $base/cc-asus-aura-argb-$V-x86_64-linux.tar.gz.sha256
   sha256sum -c cc-asus-aura-argb-$V-x86_64-linux.tar.gz.sha256
   tar xzf cc-asus-aura-argb-$V-x86_64-linux.tar.gz
   cd cc-asus-aura-argb-$V-x86_64-linux
   ```

2. Install, then restart CoolerControl:

   ```sh
   sudo ./install.sh
   sudo systemctl restart coolercontrold     # or your CoolerControl container's unit
   ```

3. **Disable CoolerControl's built-in "ASUS Aura LED Controller" device**
   ([how](#disable-the-built-in-aura-device)). Until you do, both drivers write to the
   controller. You can tell it is still active if:

   - *ASUS Aura LED Controller* (channels `led1`..`led4`, `sync`) still appears in
     CoolerControl's device list next to *ASUS Aura ARGB*, or
   - the daemon log (`journalctl -u coolercontrold`, or your container's log) shows
     `Successfully applied:: ASUS Aura LED Controller` lines.

   When it is disabled, the daemon logs `Skipping disabled device: ASUS Aura LED
   Controller` at start.

`install.sh` is idempotent; run it again to update. It installs:

| What | Where |
|---|---|
| binary | `/usr/local/bin/cc-asus-aura-argb` |
| systemd unit | `/etc/systemd/system/cc-asus-aura-argb.service` (enabled and started) |
| manifest | `<plugins dir>/asus-aura-argb/manifest.toml`; the script picks the plugins directory and prints which one and why |
| socket | `/var/lib/coolercontrol/plugins/asus-aura-argb/asus-aura-argb.sock` |
| save records | `/var/lib/cc-asus-aura-argb/` |

To put the manifest somewhere else, use `sudo ./install.sh --manifest-dir DIR`.

**From source** instead: `make build` as your user (needs only Podman and `make`), then
`sudo make install`, which runs the same `install.sh`. See [docs/build.md](docs/build.md).

### CoolerControl in a container

The official container image reads plugins from `/etc/coolercontrol/plugins`, not
`/var/lib/coolercontrol/plugins`. `install.sh` notices this: if
`/etc/coolercontrol/plugins` is a real directory, it puts the manifest there. A native
CoolerControl turns that path into a symlink, and the script follows it. What you need:

- `/var/lib/coolercontrol` and `/etc/coolercontrol` bind-mounted into the container at
  the same paths. The socket is under `/var/lib/coolercontrol`.
- Start the container once before installing, so `/etc/coolercontrol/plugins` exists.
  Otherwise pass `--manifest-dir /etc/coolercontrol/plugins`.

### Disable the built-in Aura device

Do one of these, then let CoolerControl restart:

- **Only this device (recommended):** in the CoolerControl UI open *Manage Devices and
  Sensors* (on the devices overview), switch off *ASUS Aura LED Controller*, and press
  *Apply & Restart*.
- **All liquidctl devices:** *Settings*, turn off *Liquidctl integration*. Only do this
  if you have no other liquidctl device you want CoolerControl to control.
- **By hand:** stop the daemon, and in `/etc/coolercontrol/config.toml` set
  `disable = true` in the Aura device's table `[settings."<device UID>"]`, then start
  the daemon. The table is keyed by CoolerControl's device UID and carries
  `name = "ASUS Aura LED Controller"` once CoolerControl has stored any setting for
  the device; the UI route above is simpler.

What this does, from the CoolerControl 5.1.2 source: a disabled liquidctl device is
skipped before CoolerControl initialises it, polls its status or sets its lighting.
CoolerControl's liquidctl helper may still keep the device open while other liquidctl
devices are enabled, but sends it nothing; with no enabled liquidctl device left, the
helper is shut down. Details and what was verified: [docs/design.md](docs/design.md#coexistence-with-coolercontrols-built-in-driver).

### Check it works

`journalctl -u cc-asus-aura-argb -b` should show `Found ASUS Aura LED Controller at usb
... firmware AULA3-..., channels: mainboard, argb1, ...` and `Listening on ...`. After
restarting CoolerControl, its log lists the device under `Initialized Service Plugin
Devices`, and *ASUS Aura ARGB* appears in the UI with one lighting channel per header plus
*All channels* (`sync`).

## Using it

Pick a mode (and colour, for modes that take one) for each channel in CoolerControl:

| Channel | What |
|---|---|
| `mainboard` (Mainboard LEDs) | the board's own LEDs: effect channel 0 |
| `argb1`, `argb2`, ... (ARGB header N) | the controller's N-th addressable header |
| `sync` (All channels) | every channel above at once |

The numbering is the controller's own order and need not match the silkscreen; try a
colour on each to find your case (on the tested board it is `argb2`).

Modes, on every channel: `off`, `static`, `breathing`, `flashing`, `chase_fade` and
`chase` (one colour each), and the colour-less `spectrum_cycle`, `rainbow`,
`spectrum_cycle_breathing`, `spectrum_cycle_chase_fade`, `spectrum_cycle_chase`,
`spectrum_cycle_wave`, `chase_rainbow_pulse`, `rainbow_flicker`, `gentle_transition`,
`wave_propagation`, `wave_propagation_pause`, `red_pulse`. The names are liquidctl's.

### The `sync` channel

Setting `sync` gives every channel that setting. Setting a single channel afterwards
changes just that channel. **The most recent change wins**, channel by channel:

- `sync` rainbow, then `argb2` red: everything rainbow except the case, which is red.
- Then `sync` rainbow again: everything rainbow, the case too.
- Clicking a setting again always rewrites it to the controller, so if something else
  changed the lights (for example CoolerControl's built-in Aura device still enabled),
  re-selecting the setting fixes them. Within the first 20 s after CoolerControl starts
  or the PC resumes, a repeat is treated as CoolerControl re-sending its settings.
- Resetting a channel in CoolerControl lets `sync` show through on it; resetting `sync`
  goes back to each channel's own setting (a channel that never had one keeps what it
  shows).

CoolerControl re-sends every saved channel setting, `sync` included, when it starts
and after resume, in no fixed order. The plugin remembers which change came last (in
`/var/lib/cc-asus-aura-argb/`), so that re-sending never changes the result, and the
board looks the same after a restart or resume as before. One limit: a CoolerControl
*Mode* that sets both `sync` and individual channels to different things applies them
at the same moment, and then their arrival order decides. Details:
[docs/design.md](docs/design.md#the-sync-channel-and-precedence).

### Command line

The binary also works on its own (as root), which is handy for testing:

```sh
sudo cc-asus-aura-argb probe                          # find the controller, dump its config table
sudo cc-asus-aura-argb modes                          # list the modes
sudo cc-asus-aura-argb set argb2 static --color ff0000
sudo cc-asus-aura-argb set argb2 rainbow
sudo cc-asus-aura-argb set mainboard off
sudo cc-asus-aura-argb set sync static --color 0000ff     # every channel
sudo cc-asus-aura-argb set argb2 rainbow --save       # apply, then save to the controller
sudo cc-asus-aura-argb save                           # save what the controller runs now
```

Add `--debug` to log every HID report written and read. `cc-asus-aura-argb --help`
lists all options. Stop the service (`sudo systemctl stop cc-asus-aura-argb`) or
disable the device in CoolerControl while experimenting, or CoolerControl may re-apply
its own settings over yours.

## Saving to the controller

The controller has a save command that stores its current effect settings; liquidctl's
guide reports that the settings then survive cold boot, warm boot and sleep. liquidctl
sends it after every change. This plugin does not, because how many writes the
controller's memory takes is unknown. Instead:

- it saves only after the settings have stayed unchanged for 30 seconds, so clicking
  through colours costs one save;
- only if they differ from what the plugin last saved. CoolerControl re-sends the same
  settings at every start and resume, and that costs nothing;
- not while a channel's state is unknown after a failed write.

To change the delay or turn saving off, add `--auto-save-delay <seconds>` or
`--no-auto-save` to `ExecStart=` with `sudo systemctl edit --full cc-asus-aura-argb`.
Don't run `save` or `set --save` from scripts or timers.

The controller's memory cannot be read back, so the plugin only remembers what it
saved. If something else changes it (another tool, firmware/BIOS update, CMOS reset),
the board may show another look at power-on until you change a setting or run
`sudo cc-asus-aura-argb save` once. Details: [docs/design.md](docs/design.md#persistence-and-auto-save).

## Troubleshooting

| Symptom | Fix |
|---|---|
| Service running, but CoolerControl logs nothing about the plugin | The daemon isn't reading the manifest; usually CoolerControl runs in a container. See [CoolerControl in a container](#coolercontrol-in-a-container). |
| `No supported controller found` | Check `lsusb -d 0b05:19af`. Run `probe` with `--debug`. Only `0b05:19af` is supported. |
| `cannot open /dev/hidrawN: Permission denied` | Run as root. |
| `reading the config table failed` | Another program may be talking to the controller. Disable the built-in Aura device and stop other RGB software, then restart the service. |
| A channel flips between your setting and another one | CoolerControl's built-in Aura device (or other RGB software) is still writing: check for `Successfully applied:: ASUS Aura LED Controller` in the daemon log. [Disable it](#disable-the-built-in-aura-device). |
| After a restart a channel shows `sync`'s setting instead of its own (or the reverse) | The plugin's precedence record was missing (first start, deleted state directory); the order CoolerControl re-sent them in decided once. Set the channel (or `sync`) again. |
| A colour-less mode (rainbow, ...) shows white or black instead of the effect | Please report it with `--debug` output; see the hardware notes in [docs/protocol.md](docs/protocol.md#colour-less-effects). |
| Case LEDs do nothing | Check the cabling (5 V ARGB header, hub in motherboard-sync mode) and try every `argbN` channel. |
| Service fails with `status=226/NAMESPACE` | `/var/lib/coolercontrol/plugins/asus-aura-argb` is missing. Re-run `sudo ./install.sh`. |
| Board shows another look at power-on | The setting changed less than 30 s before shutdown, or something else changed the controller. Run `sudo cc-asus-aura-argb save` once. |

To get back to a known state: re-enable CoolerControl's built-in device and apply a
setting there, or power the machine off completely (unplug for a few seconds), which
brings the controller back to whatever is saved in it.

## Uninstall

```sh
sudo ./install.sh --uninstall     # or: sudo make uninstall
```

This removes the binary, the unit, the plugin's own directories (wherever the manifest
was put) and the save records. Then restart CoolerControl, and re-enable its built-in
Aura device if you want it back.

## Documentation

- [docs/protocol.md](docs/protocol.md): the controller's USB HID protocol, where each
  fact comes from, and what was verified on hardware.
- [docs/design.md](docs/design.md): service behaviour, channel model, persistence,
  coexistence with CoolerControl's built-in driver, paths, systemd sandbox.
- [docs/build.md](docs/build.md): building, testing, releases, repository layout.

## Licence and credits

Copyright (C) the cc-asus-aura-argb contributors. Licensed under the GNU General Public
License, version 3 or (at your option) any later version; see [LICENSE](LICENSE).

liquidctl's
[`aura_led` driver and device guide](https://github.com/liquidctl/liquidctl)
(GPL-3.0-or-later) were a source of protocol knowledge, checked against tests on the
board above. The code is written from notes, not copied.

The service skeleton, build script and the vendored `proto/` files are derived from the
CoolerControl [device service template](https://gitlab.com/coolercontrol/cc-plugins)
(GPL-3.0-or-later, `cc-plugins` commit `47edcf3a`). The `proto/` files are unmodified.

Not affiliated with ASUS, liquidctl or CoolerControl.
