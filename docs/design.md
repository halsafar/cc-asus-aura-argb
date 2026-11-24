# Design and internals

How the plugin is put together and why. For the bytes on the wire see
[protocol.md](protocol.md); for building and the code layout see [build.md](build.md).

## Overview

- One binary, `cc-asus-aura-argb`, run as a host systemd service. CoolerControl does
  not start it; the manifest has no `executable`, only an `address`. That is how a
  CoolerControl plugin says "managed by other means". It also lets the plugin work when
  the CoolerControl daemon runs in a container, which cannot create host systemd units.
- It serves CoolerControl's `coolercontrol.device_service.v1.DeviceService` gRPC API
  on a Unix socket. The proto files are vendored, unmodified, in [`proto/`](../proto).
- It opens the controller's hidraw node once. It reads from it only at startup
  (firmware string and config table) and writes only when a setting is applied or
  saved. Nothing is polled and nothing is animated in software: every effect runs on
  the controller, so there is no periodic USB traffic.

## gRPC service

| RPC | Behaviour | Touches the device |
|---|---|---|
| `Health` | name, version, uptime | no |
| `ListDevices` | one device per controller (see below) | no |
| `InitializeDevice` | opens the re-apply window and forgets what the controller shows; `NOT_FOUND` for an unknown device | no |
| `Status` | always an empty list (no sensors) | no |
| `ResetChannel` | drops that logical channel's precedence record ([below](#the-sync-channel-and-precedence)) and writes only what changes | only if a channel's effective setting changes |
| `Shutdown` | OK, logs, does **not** exit | no |
| `Lighting` | validates, records precedence, writes the covered channels' effective settings (a re-apply may skip channels the controller already shows); schedules auto-save | writes |
| fan, LCD, custom function | `UNIMPLEMENTED` | no |

`Shutdown` does not exit, because the service is run by systemd and must still be there
when the daemon starts again. A device with lighting channels, no sensors and an empty
`Status` is accepted by CoolerControl 5.1.2.

When CoolerControl calls what, from its 5.1.2 source:

| Event | What the daemon sends | Where |
|---|---|---|
| daemon start / plugin connect | `InitializeDevice` once per device, then (if `apply_on_boot`, the default) every saved channel setting: `Lighting`, or `ResetChannel` for a channel saved as reset | `repositories/service_plugin/service_plugin_repo.rs` (`init_single_device`), `modes.rs` (`handle_settings_at_boot`, `apply_all_saved_device_settings`) |
| resume | after a pause (`startup_delay`, at least 1 s), every saved channel setting again. **No `InitializeDevice`**: `Engine::reinitialize_devices` only reinitialises liquidctl devices; the plugin repository's own `reinitialize_devices` is never called | `main_loop.rs` (`wake_from_sleep`), `engine/main.rs` |
| a Mode is activated | the Mode's settings for this device, one task per channel (concurrent), and `ResetChannel` for channels the Mode does not contain | `modes.rs` (`activate_mode`, `apply_mode_channel_settings`, `reset_unset_mode_channels`) |
| user resets a channel | `ResetChannel` (API `/devices/{uid}/settings/{channel}/reset`) | `api/router.rs`, `api/actor/device.rs` |

Saved settings of one device are applied one after another, in the order of the
device's table in CoolerControl's `config.toml`, which this plugin cannot see or rely on.

`ListDevices` reports, per controller:

- `id`: `aura-usb-<USB name>`, e.g. `aura-usb-3-2` (bus 3, port 2: stable for a device
  soldered to the board). `uid_info`: `0b05:19af at usb 3-2`.
- `name`: `ASUS Aura ARGB` (different from the built-in `ASUS Aura LED Controller`, so
  the two can be told apart). `info.model`: board name from DMI, USB product string and
  firmware.
- `info.channels`: one lighting channel per effect channel (below) plus `sync`, each
  with all 18 effects. `min_colors` = `max_colors` = 0 or 1; `speed_enabled` and
  `backward_enabled` false.
- `info.lighting_speeds`: empty (the controller has no speed setting).
- `info.driver_info`: plugin name, version, the hidraw node and the USB sysfs path.

`Lighting` errors:

| Request | gRPC code |
|---|---|
| unknown device or channel | `NOT_FOUND` |
| missing setting, unknown mode, missing colour, colour value over 255 | `INVALID_ARGUMENT` |
| HID write failed | `UNAVAILABLE` |

Speed and `backward` are ignored. Extra colours are ignored, and so are colours sent
with an effect that takes none (black is sent). Rejected requests never reach the
device. Writes run in `spawn_blocking`, behind a per-device mutex.

## Channel model

| Channel id | Label | Effect channel | Colour slots |
|---|---|---|---|
| `mainboard` | Mainboard LEDs | 0 | `0 .. table[0x1B]-1` |
| `argb1` .. `argbN` | ARGB header 1 .. N | 1 .. N | one each, after the mainboard's |

Why this naming: the protocol addresses channels by effect-channel number, and that
is all the controller tells us. Fixed short ids keep CoolerControl's saved settings
stable across restarts and are easy to type in the CLI. The number is the
controller's own order, which need not match the board's silkscreen labels (no
mapping is known), so the label says "ARGB header N" rather than guessing a
silkscreen name. On the tested board the case is `argb2`. The 12 V RGB header(s), if
any, probably share effect channel 0 with the board's LEDs (not verified), and the byte
that would count them reads oddly on the tested firmware, so they are not split out.

### The sync channel and precedence

`sync` ("All channels") is a fifth, logical channel: setting it sets every physical
channel. Each physical channel then shows **whichever of its own setting and the
`sync` setting was changed last**. Doing that naively would break on CoolerControl's
re-apply storms, which re-send every saved channel setting (including `sync`) in an
order the plugin cannot rely on. So the plugin keeps its own record
([`src/precedence.rs`](../src/precedence.rs)):

- For each logical channel (`mainboard`, `argbN`, `sync`): the last requested setting
  and a sequence number from one counter that only grows.
- Effective setting of a physical channel = its own record or the `sync` record,
  whichever has the higher number; `None` if neither exists.
- A `Lighting` request that **equals the stored setting** for its logical channel and
  arrives **inside the re-apply window** is a re-apply: no new number. The plugin only
  makes sure the controller shows the effective settings of the channels it covers.
- Any other request (different setting, or outside the window) is a user change: it
  gets the next number and so wins on the channels it covers. Clicking `sync` = rainbow
  again after setting `argb2` = red gives rainbow everywhere.
- `ResetChannel` drops the logical channel's record (idempotent, so a reset re-sent in
  a storm changes nothing). Resetting a channel lets `sync` show through; resetting
  `sync` shows each channel's own setting. A channel left with neither keeps what it
  shows: the protocol has no known "default" to go back to.

Because a storm only re-sends what is already recorded, each re-sent request writes
the *effective* settings, and the result after the storm is the same in every arrival
order. The tests in `tests/sync.rs` check both orders for "sync newer", "channel
newer" and a mixed case, the "click sync again" case, a differing request inside the
window, restarts, resume, resets and corrupt or missing records.

**The window.** It opens when `InitializeDevice` arrives (daemon start) and when the
plugin notices that the system was suspended. CoolerControl 5.1.2 sends no
`InitializeDevice` after resume (table above), so the plugin detects it itself: at
every request it compares `CLOCK_BOOTTIME - CLOCK_MONOTONIC` (time spent suspended)
with the previous value; growth of more than 1 s means a suspend happened, and the
first request after it (the start of the resume storm) opens the window. The window
lasts 20 s (`--reapply-window`). Reasoning: at daemon start the storm follows
`InitializeDevice` after only the custom-sensor repository and Mode setup, and our
device's settings are sent one after another with at most CoolerControl's per-device
command delay (capped at 250 ms) between them, so a few seconds suffice; at resume the
window starts with the storm itself. 20 s leaves a wide margin for slow machines while
keeping short the time in which a user's click that repeats a stored setting is taken
as a re-apply.

**When writes are skipped.** A user change (a new or different setting, a repeat of
the stored setting outside the re-apply window, or a reset that changes what a channel
should show) always writes every channel it covers. The plugin cannot read the
controller, so its idea of what is shown can be wrong (CoolerControl's built-in driver
or another tool still writing, a controller reset, a resume with `apply_on_boot` off),
and clicking the same setting again must fix that. Only a re-apply inside the window
may skip a channel the device last wrote that same setting to, so a storm normally
costs little USB traffic. Opening the window (daemon start, detected resume) also
forgets that knowledge, because the controller may have gone back to its saved
settings: the first re-apply after either always writes.

**Persistence.** The records are a small file next to the auto-save record,
`/var/lib/cc-asus-aura-argb/0b05-19af_usb-3-2.precedence`, written atomically and read
at every request (so CLI changes are seen):

```
cc-asus-aura-argb precedence v1
argb2 2 effect=01 color=ff0000
sync 3 effect=05 color=000000
```

A missing or invalid file is logged and treated as empty; it is replaced at the next
change. Without a state directory the records live in memory: precedence still works
while the service runs (also across CoolerControl restarts), which is logged once at
start; after a service restart, the first storm's arrival order decides once.

**Auto-save** works on the physical channels' effective state as before. A `sync`
change is one apply (one generation) however many channels it writes, so it costs at
most one save. A rewrite of an unchanged setting (a repeated click) schedules a save
attempt, and the change-only comparison then writes no save packet; skipped writes
schedule nothing.

**Limits.**

- A Mode that sets `sync` and individual channels to different settings applies them
  concurrently, and their arrival order decides (both are user changes).
- Inside the window, a user click that repeats a channel's stored setting is a
  re-apply, not a change: it gains no precedence and may be skipped. Outside the
  window a repeated click always rewrites the channel. This only matters in the 20 s
  after a daemon start or resume.
- A resume with `apply_on_boot` turned off sends nothing; the controller then shows its
  saved settings until the next change. The plugin cannot read the controller, so it
  cannot notice that.
- The CLI (`set`) records its change in the same file, but holds no lock against the
  running service.

## Startup and failure handling

1. Discover the controller in sysfs (below). If none is found, retry every 0.5 s for up
   to `--device-wait` seconds (default 10), because USB enumeration can finish after
   the service starts.
2. Open `/dev/hidrawN` and check its USB ids with `HIDIOCGRAWINFO`.
3. Read the firmware string and the config table (bounded retries, below). A missing
   firmware string is logged and not fatal; a missing config table means the device is
   not listed.
4. Build the channels and serve. On any failure the reason is logged and the service
   keeps running with zero devices, so it does not crash-loop.

After that, the node is only written to. If a write fails because the node has gone
(`ENODEV` and similar, e.g. the device was re-enumerated), the service finds the
node again by USB ids and port in sysfs, reopens it, and retries that apply once. It
does not re-read anything.

At startup a stale socket from an unclean exit is removed; a path that exists but is
not a socket is left alone and the service refuses to start. The socket has mode
`0600`, so only root (the CoolerControl daemon, also in a rootful container) can
connect. On `SIGTERM` or `SIGINT` the server stops, a pending auto-save is flushed
(bounded to 5 s), and the socket is removed.

Log level: `--debug` (which also logs every HID report written and read, as hex),
otherwise `CC_LOG`, default `info`. Under journald, timestamps are left out.

## Command line

```
cc-asus-aura-argb [--debug] [--sysfs-root DIR] [--state-dir DIR] [serve [--socket PATH]
                  [--auto-save-delay SECONDS] [--no-auto-save] [--device-wait SECONDS]
                  [--reapply-window SECONDS]]
cc-asus-aura-argb [--debug] probe
cc-asus-aura-argb modes
cc-asus-aura-argb [--debug] set <channel> <mode> [--color RRGGBB] [--effect-only] [--save]
cc-asus-aura-argb [--debug] [--state-dir DIR] save
```

| Command | What it does | Writes |
|---|---|---|
| (none) / `serve` | the gRPC service; default socket `/var/lib/coolercontrol/plugins/asus-aura-argb/asus-aura-argb.sock` | on `Lighting` |
| `probe` | finds the controller, prints USB details, the firmware string and reply, the raw config reply, the table in rows of six, the decoded fields and the channels | only the two read requests |
| `modes` | lists the modes | nothing (no device access) |
| `set` | checks mode and colour first (bad arguments never cause I/O), then reads the config table and writes the two effect reports (for `sync`: to every channel), and records the change as the newest in the precedence file if the state directory exists. `--effect-only` sends only the effect report (diagnostic). `--save` also writes the save report. `--speed` is accepted and ignored | effect reports; save with `--save` |
| `save` | reads the config table, writes the save report | save |

`set` and `save` forget the auto-save record (it no longer matches what is stored).
They use the first controller if several are found. `--sysfs-root` exists for testing
against a fake sysfs tree.

## Discovery

hidraw numbers are not stable, so the node is found through sysfs (`discovery.rs`,
sysfs root injectable for tests), without opening anything:

1. For each `/sys/class/hidraw/hidrawN`, resolve `device` to the HID device directory,
   named `BBBB:VVVV:PPPP.NNNN`. Keep it if the bus is USB (`0003`) and the ids are in
   the supported table (`0b05:19af`).
2. Read its `report_descriptor` (a kernel cache) and require the lighting interface:
   usage page `0xFF72`, report `0xEC` with 64-byte input and output. Other interfaces
   of the same device are skipped.
3. The parent directory is the USB interface (`bInterfaceNumber`), its parent the USB
   device: `idVendor`/`idProduct` must match again; `bcdDevice`, `manufacturer` and
   `product` are read for display. The USB device's name (`3-2`) identifies the
   controller.
4. Board vendor and name come from `/sys/class/dmi/id/board_{vendor,name}`.

## Coexistence with CoolerControl's built-in driver

CoolerControl's liquidctl helper (`coolercontrol-liqctld`) drives this controller with
liquidctl's `AuraLed` driver. Two consequences:

**Reads.** hidraw gives every open file its own copy of every input report. If the
helper sends requests (its status poll for this device sends `EC B0` each time), this
plugin sees those replies too. So the plugin reads only at startup; before each request
it drains queued input, then waits up to 500 ms for a reply with the expected id
(`02` or `30`), skipping anything else (at most 64 per attempt), and makes at most 3
attempts. Nothing after startup depends on a read. On the tested board both requests
worked by hand while the helper had the device open.

**Writes.** Both would write lighting, and the last writer wins per report. The user
must disable the built-in device (README). From the CoolerControl 5.1.2 source:

| Fact | Where | Status |
|---|---|---|
| Device settings `disable = true` makes the daemon skip a liquidctl device before attaching it: no initialize, no status polling, no lighting | `coolercontrold/daemon/src/repositories/liquidctl/liquidctl_repo.rs` (`get_devices`) | verified in source |
| The setting is `[settings."<device UID>"] disable = true` in `config.toml` | `coolercontrold/daemon/src/config.rs` (`get_cc_settings_for_device`) | verified in source |
| The UI toggles it in *Manage Devices and Sensors* and restarts the daemon (*Apply & Restart*) | `coolercontrol-ui/src/shell/devices/ManageSensorsView.vue`, `composables/useDeviceActions.ts`, `i18n/locales/en.ts` | read in source; the click path is inferred from the strings, not tried |
| Turning off *Liquidctl integration* (`liquidctl_integration = false`) deactivates all liquidctl devices | `config.rs`, UI strings | verified that the setting exists; effect taken from its UI description |
| The helper still finds and connects (opens) every liquidctl device, disabled ones included | `coolercontrold/daemon/resources/liqctld/main.py` (`get_devices`) | verified in source |
| liquidctl's `AuraLed` defines no `connect`, so connecting sends nothing to the controller | liquidctl `aura_led.py` | inferred: the base class's `connect` (only opening the device) was not read |
| With no enabled liquidctl device left, the helper is shut down | `liquidctl_repo.rs` | verified in source |

So with the built-in device disabled, the helper may hold the node open but sends it
nothing. Other RGB software that writes to the controller conflicts the same way.

## Persistence and auto-save

What is known about what survives: liquidctl's guide says its effect settings remain
on cold boot, warm boot and after sleep, and liquidctl sends the save report `EC 3F 55`
after every change. On the tested board an effect applies immediately without the save
report. What survives power-off or sleep *without* it is not known, and neither is how
many writes the controller's memory takes.

Decision: the plugin saves, but carefully, reusing the design of its sibling plugin's
auto-save:

- After a successful `Lighting` apply, a save is scheduled. It fires only after the
  settings have stayed unchanged for the delay (default 30 s, `--auto-save-delay`);
  every apply on any channel restarts the wait.
- When it fires, it builds a record of every channel's effect, colour and addressing,
  and compares it with the record of what was last saved on this controller. Equal:
  nothing is written. Different or unknown: it writes the save report, then the record.
- Channels not set since the service started are recorded as `unknown` (the save keeps
  whatever the controller has for them). A channel whose last write failed blocks
  saving until it is set again; a failed apply also cancels the pending save.
- A failed save is logged and not retried on its own; the next user change, also
  clicking the same setting again, schedules another attempt.
- At shutdown a pending save is flushed with the same compare-then-write, bounded to
  5 s. `--no-auto-save` turns it off; without a state directory it is off, with a
  warning.

Sending the save on every apply, as liquidctl does, would cost a write for every colour
clicked and for every CoolerControl start and resume; the debounce and change detection
cost at most one write per real change. Not saving at all would be safe for the memory
but would leave the board in its factory rainbow at power-on and possibly after sleep;
since CoolerControl re-applies at start and resume this is a cosmetic gap, so
`--no-auto-save` is a reasonable choice too.

Implementation: one mutex per device over the bus, a generation counter, each
channel's state (`Untouched`, set to an effect, `Failed`) and a pending flag
(`src/device.rs`). Every apply bumps the generation and spawns a timer
(`AutoSave::schedule` in `src/autosave.rs`); a timer whose generation is no longer
current does nothing, which gives a debounce without cancellation. The record is read
from disk at every attempt.

Record file, one per controller in the state directory (`/var/lib/cc-asus-aura-argb`):

```
0b05-19af_usb-3-2.saved:
cc-asus-aura-argb saved-state v1
mainboard unknown
argb1 effect=05 channel=01 slots=1+1 color=000000
argb2 effect=01 channel=02 slots=2+1 color=ff0000
argb3 effect=04 channel=03 slots=3+1 color=000000
```

Writes are atomic (temp file, fsync, rename, fsync of the directory). A missing or
invalid record means "unknown", which leads to one save, never a crash.

Known limits: the memory cannot be read back, so something else rewriting it (other
software, a firmware update, CMOS reset) makes the record wrong until the next change
or a manual `save`. The CLI and the service share no lock. The first time a setup is
seen costs one write even if the controller already holds it. A change made less than
the delay before a power cut is not saved. None of this has run on hardware.

## Files and paths

| Path | What | Fixed in |
|---|---|---|
| `/usr/local/bin/cc-asus-aura-argb` | binary | unit `ExecStart=` |
| `/etc/systemd/system/cc-asus-aura-argb.service` | unit | |
| `/var/lib/coolercontrol/plugins/asus-aura-argb/asus-aura-argb.sock` | socket | `dist/manifest.toml` (`address`), unit (`--socket`, `ReadWritePaths=`), `DEFAULT_SOCKET` in `src/lib.rs`, `SOCKET_DIR` in `install.sh` |
| `<plugins dir>/asus-aura-argb/manifest.toml` | manifest | chosen by `install.sh` (below); `--manifest-dir` / `MANIFEST_PLUGINS_DIR` override |
| `/var/lib/cc-asus-aura-argb/` | auto-save records | unit `StateDirectory=`, `DEFAULT_STATE_DIR` in `src/autosave.rs`, `STATE_DIR` in `install.sh` |

The binary is not put in the plugins directory. On SELinux systems files there are
labelled `var_lib_t`, which systemd cannot execute (`status=203/EXEC`).
`/usr/local/bin` gets `bin_t`.

**Manifest and socket are separate on purpose.** The daemon scans
`<data dir>/plugins` for manifests, unless `CC_PLUGINS_DIR` is set. Native packages use
`/var/lib/coolercontrol`. The official container image sets
`CC_DATA_DIR=/etc/coolercontrol`, so a containerised daemon scans
`/etc/coolercontrol/plugins`. The socket stays under `/var/lib/coolercontrol`, which has
to be bind-mounted into the container at the same path so that `address` resolves
there. A path-based Unix socket works across network namespaces, so the plugin's
`PrivateNetwork=yes` does not get in the way.

### How install.sh picks the manifest directory

Checked against the CoolerControl daemon source at tag 5.1.2
(`coolercontrold/daemon/src/paths.rs`, `main.rs`, and the Docker Hub image files
`.gitlab/images/dockerhub/{debian,alpine}/Dockerfile`):

- config dir = `$CC_CONFIG_DIR` or `/etc/coolercontrol`; data dir = `$CC_DATA_DIR` or
  `/var/lib/coolercontrol`.
- plugins dir (scanned) = `$CC_PLUGINS_DIR` or `<data dir>/plugins`. The "legacy" dir
  is always `<config dir>/plugins`.
- At startup `ensure_plugins_dir` → `migrate_plugins_dir(canonical, legacy)`:
  1. It creates the plugins dir.
  2. If the two paths are equal, it stops there.
  3. Otherwise, if the legacy path is a symlink to the plugins dir, nothing more
     happens. A stale symlink is replaced. A real legacy directory has its entries
     moved into the plugins dir and is removed (if removal fails, it is left in place
     and no symlink is made).
  4. It then creates the symlink `legacy -> plugins dir`.
- The official images set `ENV CC_DATA_DIR=/etc/coolercontrol`, so plugins dir ==
  legacy == `/etc/coolercontrol/plugins`, a real directory, and no symlink is made.

So, once the daemon has run, `/etc/coolercontrol/plugins` is a symlink to the scanned
directory on a native install, and the scanned directory itself in the official
container. `install.sh` uses:

1. `--manifest-dir` / `MANIFEST_PLUGINS_DIR` if given;
2. else, if `/etc/coolercontrol/plugins` is a symlink, its target (resolved one level,
   and kept inside `DESTDIR` when staging);
3. else, if it is a real directory, `/etc/coolercontrol/plugins` (warning when a native
   `coolercontrold` is on `PATH`, since that could also be a native install whose
   migration left the directory behind);
4. else `/var/lib/coolercontrol/plugins`.

It prints the choice and the reason. Where it can be wrong:

- The daemon has never run, so the path does not exist yet: a container install then
  gets `/var/lib`. Start the daemon once first, or pass `--manifest-dir`.
- The container's `/etc/coolercontrol` comes from a different host directory.
- `CC_PLUGINS_DIR` or `CC_CONFIG_DIR` point somewhere custom.
- A native install's legacy directory could not be removed by the migration.

In all of these, use `--manifest-dir`. A manifest left by an earlier install in the
socket directory is removed when the manifest now goes elsewhere. Uninstall removes the
`asus-aura-argb` subdirectory from the override, `/etc/coolercontrol/plugins`
(resolved) and `/var/lib/coolercontrol/plugins`, and nothing else.

## systemd unit

[`dist/cc-asus-aura-argb.service`](../dist/cc-asus-aura-argb.service) runs the service
as root, because the hidraw node and the plugin directory belong to root, but takes
almost everything else away. Reviewed option by option for what this plugin needs:

| Setting | Why |
|---|---|
| `CapabilityBoundingSet=` (empty), `NoNewPrivileges=yes` | root owns the node, the socket directory and the state directory, so no capabilities are needed. The plugin directory must therefore be owned by root. |
| `DevicePolicy=closed`, `DeviceAllow=char-hidraw rw` | only hidraw character devices. `char-hidraw` is resolved from `/proc/devices` at start; the HID core registers it at boot, so no module needs loading. |
| no `PrivateDevices=` | it would hide `/dev/hidraw*` |
| no ordering on udev | the node may appear after the service starts; `--device-wait` (10 s) covers that instead of a deprecated `udev-settle` dependency |
| `ProtectSystem=strict`, `ReadWritePaths=` plugin directory, `StateDirectory=` | read-only system; writable socket directory and state directory only. `ReadWritePaths=` fails with `226/NAMESPACE` if the directory is missing. |
| `ProtectKernelTunables=yes` | makes `/sys` read-only; discovery only reads it |
| `PrivateNetwork=yes`, `RestrictAddressFamilies=AF_UNIX` | no network; Unix sockets only (hidraw needs no socket family) |
| `ProtectHome`, `PrivateTmp`, `ProtectKernelModules`, `ProtectKernelLogs`, `ProtectControlGroups`, `ProtectClock`, `ProtectHostname`, `ProtectProc=invisible` | general hardening, nothing here needs them |
| `SystemCallFilter=@system-service ~@privileged`, `MemoryDenyWriteExecute`, `LockPersonality`, `RestrictNamespaces`, `RestrictRealtime`, `RestrictSUIDSGID`, `SystemCallArchitectures=native` | general hardening; `ioctl` (`HIDIOCGRAWINFO`) and `poll` are in `@system-service` |
| `UMask=0077` | socket and records private to root |
| `Before=coolercontrold.service coolercontrol.service` | the socket is up before the daemon connects (native unit and usual container unit names) |
| `TimeoutStopSec=15` | room for the bounded (5 s) auto-save flush |

Not yet run on hardware.
