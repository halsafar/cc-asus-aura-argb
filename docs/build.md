# Building and contributing

## Requirements

- With the default container build: Podman (rootless is fine) and `make`. Nothing else
  on the host.
- With a local toolchain (`CONTAINER=0`): Rust 1.86 or newer and `protoc`
  (protobuf-compiler). `build.rs` generates the gRPC code from [`proto/`](../proto) at
  build time.

## Make targets

| Target | What it does |
|---|---|
| `make build` | `cargo build --locked --release` (glibc, for local use), copies the binary to `build/cc-asus-aura-argb` |
| `make dist` | static musl release archive in `build/dist/` (below) |
| `make test` | `cargo test --locked` and `tests/install_sh.sh` |
| `make lint` | `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, `shellcheck` on the shell scripts |
| `make check` | `test` + `lint` |
| `make fmt` | `cargo fmt` |
| `make image` / `make dist-image` | build the container images (done by the targets above) |
| `make clean` | removes `build/` and `target/`, and the target-cache volume |
| `sudo make install` / `sudo make uninstall` | runs `install.sh` with `build/cc-asus-aura-argb` |

By default every cargo command runs in a container built from
[`Containerfile.build`](../Containerfile.build): `fedora:45` with cargo, rust, clippy,
rustfmt, gcc, protobuf-compiler and ShellCheck. The repo is mounted at `/src` with `:z`
(SELinux). The cargo registry and the target directory live in the named volumes
`cc-asus-aura-argb-cargo` and `cc-asus-aura-argb-target`, so rebuilds are fast.
Variables: `PODMAN`, `IMAGE`, `CARGO_VOL`, `TARGET_VOL`.

The `make build` binary is linked against the container's glibc (Fedora 45). It runs on
Fedora 45 or anything with a glibc at least as new. For a binary that runs anywhere, use
`make dist`.

Without a container:

```sh
make CONTAINER=0 build      # or: cargo build --locked --release
make CONTAINER=0 check
make CONTAINER=0 dist       # needs rustup with the musl target (rust-toolchain.toml)
```

`Cargo.lock` is committed and every target uses `--locked`. To change dependencies, run
`cargo update` (or a build without `--locked`) once and commit the new lock file.

## Release archive (`make dist`)

`make dist` builds `--target x86_64-unknown-linux-musl`. That gives a static-pie
binary with no glibc or libgcc dependency, so it runs on any x86-64 Linux. All
dependencies are pure Rust plus libc. The build runs in a second container,
[`Containerfile.dist`](../Containerfile.dist) (`fedora:45` with rustup, because Fedora's
Rust packages lack the musl target), using the toolchain pinned in
[`rust-toolchain.toml`](../rust-toolchain.toml), the same one CI uses. Its target cache
is the volume `cc-asus-aura-argb-dist-target`.

Output, for version `V` from `Cargo.toml`:

```
build/dist/cc-asus-aura-argb-V-x86_64-linux.tar.gz
build/dist/cc-asus-aura-argb-V-x86_64-linux.tar.gz.sha256
```

The archive contains one directory `cc-asus-aura-argb-V-x86_64-linux/` with the binary,
`install.sh`, `dist/` (unit, manifest), `README.md`, `LICENSE` and
`docs/`. It is reproducible: sorted entries, owner 0/0, mtime from the last commit
(`SOURCE_DATE_EPOCH`), `gzip -n`. Two runs from the same commit give the same checksum.

## Installing

`install.sh` is the only install implementation. The release archive runs it directly
(`sudo ./install.sh`). The Makefile calls it with `BINARY=build/cc-asus-aura-argb`:

```sh
make build              # as your user, not as root
sudo make install       # never builds, never starts Podman as root
```

Options and environment: `--uninstall`; `--manifest-dir DIR` or `MANIFEST_PLUGINS_DIR`
(must be absolute); `DESTDIR` stages into a directory without root or systemd; `BINARY`
sets the binary to install.
How the manifest directory is chosen is described in
[design.md](design.md#how-installsh-picks-the-manifest-directory).

Staging by hand:

```sh
make install DESTDIR=/tmp/stage
make install DESTDIR=/tmp/stage MANIFEST_PLUGINS_DIR=/etc/coolercontrol/plugins
make uninstall DESTDIR=/tmp/stage
```

`tests/install_sh.sh` runs these cases automatically against fake trees:

- native default
- container (real `/etc/coolercontrol/plugins`, plus a stale manifest)
- native symlink (an absolute link that must not escape `DESTDIR`)
- override
- refusals

It runs in `make test` and in CI.

## CI and releases

[`.github/workflows/ci.yml`](../.github/workflows/ci.yml) runs on pushes to `main`, on
pull requests and on tags `v*`:

- **check** (every run):
  1. install protoc (apt) and the pinned toolchain (rustup is preinstalled on the
     runner)
  2. `cargo fmt --check`, clippy with `-D warnings`, `cargo test`, all `--locked`
  3. shellcheck and `tests/install_sh.sh`
  4. `make dist CONTAINER=0`, a check that the binary is static
  5. upload the archive and checksum as the workflow artifact `release-archive`, so
     every build can be downloaded
- **release** (tags only, after check):
  1. fails unless the tag minus `v` equals the version in `Cargo.toml` *and* in
     `dist/manifest.toml`
  2. downloads the artifact, verifies the checksum
  3. runs `gh release create <tag> --verify-tag --generate-notes` with both files. Only
     this job has `contents: write`; everything else is read-only.

Superseded runs on the same branch are cancelled; tag runs are never cancelled. Cargo
downloads and `target/` are cached with `actions/cache`, keyed on `Cargo.lock` and
`rust-toolchain.toml`.

Actions are pinned by full commit SHA, with the version in a comment. Tags can be moved
by whoever controls the action's repository; a SHA cannot. Only GitHub's own actions are
used (checkout, cache, upload-artifact, download-artifact).
[`.github/dependabot.yml`](../.github/dependabot.yml) proposes monthly updates.

### Cutting a release

1. Bump `version` in `Cargo.toml` **and** `dist/manifest.toml` (the release job
   checks both). Update the lock file with `cargo update -p cc-asus-aura-argb --offline`
   (or one build without `--locked`), run `make check`, and commit.
2. Optionally `make dist` and try the archive.
3. Tag and push:

   ```sh
   git tag -a v0.2.0 -m "v0.2.0"
   git push origin main v0.2.0
   ```

4. The workflow publishes the release with generated notes. Edit the notes on GitHub if
   needed.

To change the Rust version, edit `rust-toolchain.toml` (CI and `make dist`). The Fedora
container used by `make build`/`make check` has its own toolchain.

## Repository layout

| Path | Contents |
|---|---|
| `src/protocol.rs` | pure report encoding and decoding: requests, reply checks, config table, channel layout, effects, effect and colour reports, save report, direct-mode encoder (unused in v1). No I/O. |
| `src/descriptor.rs` | minimal HID report-descriptor reader; accepts only the lighting interface |
| `src/hid.rs` | the `HidBus` trait; `LinuxHidraw` (non-blocking node, `poll` reads, `HIDIOCGRAWINFO` id check, reopen, `--debug` hex logging); `query` (drain, request, match reply id, bounded retries); `hid::mock::MockBus` for tests |
| `src/discovery.rs` | finds the controller's hidraw node in sysfs (sysfs root injectable), supported-device table, DMI board name; `discovery::fake::FakeSysfs` for tests |
| `src/device.rs` | channel model, `Setting`, `AuraDevice` (per-device lock, `apply_many` with optional skip-if-shown, `invalidate`, reopen-on-gone, `try_save`, `save_now`) |
| `src/precedence.rs` | the `sync` precedence records: sequence numbers, re-apply classification, effective settings, the atomic record file |
| `src/autosave.rs` | state-directory records (`StateStore`), debounced auto-save (`AutoSave`), `configure`, CLI record helper |
| `src/service.rs` | the gRPC `DeviceService`, the `sync` channel, re-apply window and suspend detection, request validation, shutdown `Flusher` |
| `src/server.rs` | Unix-socket serving, stale-socket removal, socket mode |
| `src/lib.rs` | generated proto modules, `probe` (startup reads), `build_device`, `open_device`, `detect_devices`, `DEFAULT_SOCKET` |
| `src/main.rs` | CLI (`serve`, `probe`, `modes`, `set`, `save`), logging, signals |
| `install.sh` | installer / uninstaller (POSIX sh), used by the release archive and `make install` |
| `tests/install_sh.sh` | `install.sh` tests in `DESTDIR` trees |
| `tests/grpc.rs` | end-to-end tests over a real Unix socket with a tonic client and the mock bus |
| `tests/autosave.rs` | auto-save behaviour with the mock bus and paused tokio time |
| `tests/startup.rs` | discovery in a fake sysfs plus startup reads against a scripted mock |
| `tests/sync.rs` | the `sync` channel and precedence, with paused time and a fake suspend clock |
| `tests/common/` | shared fixtures (the tested board's config table) |
| `proto/` | CoolerControl gRPC API, vendored unmodified from `cc-plugins` commit `47edcf3a` |
| `dist/` | systemd unit, plugin manifest |
| `Containerfile.build`, `Containerfile.dist`, `rust-toolchain.toml` | build environments (local; release/CI toolchain) |
| `.github/` | CI and release workflow, Dependabot config |
| `docs/` | this file, [protocol.md](protocol.md), [design.md](design.md) |
| `build.rs` | proto code generation (server and client; the client is only used by tests) |

## Tests

Everything runs without hardware; nothing opens a real hidraw node.

- **Unit tests** (in each `src/` module):
  - exact bytes of every request, the save report, effect and colour reports (masks,
    slot offsets, multi-slot mainboard), and the reports of the manual hardware test
  - firmware and config-table reply parsing, wrong report and reply ids
  - channel layout for the tested board's table, liquidctl's example tables, boards
    without fixed LEDs, and the mask and direct-id limits
  - the effect table, and that nothing but the save report is a save
  - direct-mode chunking, apply flag and high-offset flag (encoder unused in v1)
  - the real report descriptor accepted, other interfaces rejected
  - `query`: matching reply, skipping another process's reports, draining stale
    input, bounded retries, I/O errors; `HIDIOCGRAWINFO` number and struct size
  - sysfs discovery against fake trees, stale-socket handling, request validation,
    record files
- **`tests/grpc.rs`**: `ListDevices` shape (four channels, all modes), read-only RPCs
  never touch the device, per-channel reports over the wire, invalid requests rejected
  without I/O, write failure gives `UNAVAILABLE`, zero devices, auto-save only on
  change.
- **`tests/autosave.rs`**, with paused time: one save after the delay, several channels
  cost one save, no save when re-applying (also after a restart), colour ignored for
  colour-less effects, failed save not retried, failed apply blocks saving, corrupt
  record, auto-save off, shutdown flush, non-lighting RPCs silent.
- **`tests/sync.rs`**, with paused time and a fake suspend clock: `sync` listed;
  both arrival orders of a re-apply storm give the same result for "sync newer",
  "channel newer" and a mixed case, and leave the records unchanged; clicking `sync`
  again wins; the same setting after the window is a change; a differing request
  inside the window is a change and survives a restart; user changes always write,
  re-applies skip what is shown but not after `InitializeDevice` or a detected resume;
  a repeated click outside the window rewrites without a second save packet; reset semantics; no extra saves;
  corrupt or deleted precedence file; memory-only precedence without a state
  directory.
- **`tests/startup.rs`**: discovery plus startup reads like the tested board (only the
  two requests are written), missing firmware not fatal, missing table fatal, bounded
  wait with no controller.

## Smoke testing the binary without the controller

Use a fake (or empty) sysfs root so nothing real is touched:

```sh
mkdir -p /tmp/fakesys/class/hidraw
build/cc-asus-aura-argb --sysfs-root /tmp/fakesys probe          # "no supported controller found"
build/cc-asus-aura-argb modes                                     # no device access
build/cc-asus-aura-argb --sysfs-root /tmp/fakesys --debug serve \
    --socket ./test.sock --no-auto-save --device-wait 0
```

The last command serves with zero devices. Unix socket paths are limited to about 108
bytes, so use a short or relative path. A raw gRPC call with curl:

```sh
printf '\0\0\0\0\0' > empty
curl -s --http2-prior-knowledge --unix-socket ./test.sock \
  -H 'content-type: application/grpc' -H 'te: trailers' --data-binary @empty \
  -D - -o /dev/null \
  http://localhost/coolercontrol.device_service.v1.DeviceService/Health
```

`grpc-status: 0` in the headers means success.

Check the unit with `systemd-analyze verify dist/cc-asus-aura-argb.service`. It
complains if `/usr/local/bin/cc-asus-aura-argb` does not exist; point `ExecStart=` at
`build/` in a copy to check it before installing.

## Licence of contributions

GPL-3.0-or-later. Keep the SPDX header on new files.
