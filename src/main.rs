// SPDX-License-Identifier: GPL-3.0-or-later
//
// cc-asus-aura-argb command line.
// Logging and signal handling follow the CoolerControl device-service template
// (GPL-3.0-or-later).

use anyhow::{Context, Result, anyhow, bail};
use cc_asus_aura_argb::autosave::{self, DEFAULT_STATE_DIR};
use cc_asus_aura_argb::device::AuraDevice;
use cc_asus_aura_argb::device::Setting;
use cc_asus_aura_argb::discovery::{self, DEFAULT_SYSFS_ROOT, FoundController};
use cc_asus_aura_argb::hid::QueryPolicy;
use cc_asus_aura_argb::precedence::{PrecedenceStore, Request as PRequest, SYNC_ID};
use cc_asus_aura_argb::protocol::{self, Effect, Rgb};
use cc_asus_aura_argb::service::{AuraService, DEFAULT_REAPPLY_WINDOW};
use cc_asus_aura_argb::{
    DEFAULT_SOCKET, SERVICE_NAME, VERSION, build_device, detect_devices, open_bus, server,
};
use clap::{Args as ClapArgs, Parser, Subcommand};
use log::{LevelFilter, info, warn};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::str::FromStr;
use std::time::Duration;
use tokio::signal::unix::{SignalKind, signal};

/// Log level passed by the CoolerControl daemon to plugins it starts.
const ENV_CC_LOG: &str = "CC_LOG";
/// Set by systemd for `StateDirectory=`.
const ENV_STATE_DIRECTORY: &str = "STATE_DIRECTORY";
/// Upper bound for flushing a pending save at shutdown.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_DEVICE_WAIT: u64 = 10;

/// CoolerControl device-service plugin for ASUS Aura USB mainboard lighting:
/// an independent effect on every channel.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Debug logging, including every HID report written and read, as hex
    #[arg(short, long, global = true)]
    debug: bool,

    /// sysfs mount point (for testing)
    #[arg(long, global = true, value_name = "DIR", default_value = DEFAULT_SYSFS_ROOT)]
    sysfs_root: PathBuf,

    /// Directory with the record of what is saved on the controller. `serve`
    /// defaults to $STATE_DIRECTORY (set by systemd); `set --save` and `save` to
    /// /var/lib/cc-asus-aura-argb
    #[arg(long, global = true, value_name = "DIR")]
    state_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the gRPC device service on a Unix socket (default)
    Serve(ServeArgs),
    /// Find the controller, read its firmware string and config table, and print
    /// the raw and decoded table and the channels. Writes only the two read requests
    Probe,
    /// List the lighting modes (no device access)
    Modes,
    /// Apply one effect to one channel, without CoolerControl
    Set {
        /// Channel id: mainboard, argb1, argb2, ... or sync (all channels); see `probe`
        channel: String,
        /// Mode name (see `modes`)
        mode: String,
        /// Colour as RRGGBB (for modes that take one)
        #[arg(long, short = 'c', alias = "colour", value_name = "RRGGBB")]
        color: Option<String>,
        /// Accepted for symmetry with other tools; the controller has no speed setting
        #[arg(long, short = 's')]
        speed: Option<String>,
        /// Send only the effect report, not the colour report (diagnostic)
        #[arg(long)]
        effect_only: bool,
        /// Also save the effect settings in the controller's memory
        #[arg(long)]
        save: bool,
    },
    /// Save the effect settings the controller runs now in its own memory, so it
    /// shows them from power-on. Always writes; forgets the auto-save record
    Save,
}

#[derive(ClapArgs, Debug)]
struct ServeArgs {
    /// Unix socket path; must match `address` in manifest.toml
    #[arg(long, value_name = "PATH", default_value = DEFAULT_SOCKET)]
    socket: PathBuf,
    /// Save to the controller once changed settings have been unchanged this long
    #[arg(long, value_name = "SECONDS", default_value_t = autosave::DEFAULT_DELAY.as_secs())]
    auto_save_delay: u64,
    /// Never save to the controller's memory automatically
    #[arg(long)]
    no_auto_save: bool,
    /// Wait up to this long for the controller to appear at startup
    #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_DEVICE_WAIT)]
    device_wait: u64,
    /// After CoolerControl initialises the device or the system resumes, unchanged
    /// re-sent settings this long count as re-applies, not changes
    #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_REAPPLY_WINDOW.as_secs())]
    reapply_window: u64,
}

impl Default for ServeArgs {
    fn default() -> Self {
        ServeArgs {
            socket: PathBuf::from(DEFAULT_SOCKET),
            auto_save_delay: autosave::DEFAULT_DELAY.as_secs(),
            no_auto_save: false,
            device_wait: DEFAULT_DEVICE_WAIT,
            reapply_window: DEFAULT_REAPPLY_WINDOW.as_secs(),
        }
    }
}

fn main() -> ExitCode {
    let args = Args::parse();
    setup_logging(args.debug);
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> Result<()> {
    let cli_state_dir = args
        .state_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_STATE_DIR));
    match &args.command {
        None => serve(args, &ServeArgs::default()),
        Some(Command::Serve(s)) => serve(args, s),
        Some(Command::Probe) => probe(args),
        Some(Command::Modes) => {
            print_modes();
            Ok(())
        }
        Some(Command::Set {
            channel,
            mode,
            color,
            speed,
            effect_only,
            save,
        }) => {
            if speed.is_some() {
                warn!("--speed ignored: the controller has no speed setting");
            }
            set(
                args,
                &cli_state_dir,
                channel,
                mode,
                color.as_deref(),
                *effect_only,
                *save,
            )
        }
        Some(Command::Save) => save(args, &cli_state_dir),
    }
}

fn serve(args: &Args, s: &ServeArgs) -> Result<()> {
    info!("Starting {SERVICE_NAME} v{VERSION}");
    let devices = detect_devices(&args.sysfs_root, Duration::from_secs(s.device_wait));
    info!("Serving {} device(s)", devices.len());
    let env_state = std::env::var_os(ENV_STATE_DIRECTORY);
    let precedence_dir = autosave::state_dir(args.state_dir.as_deref(), env_state.as_deref()).ok();
    let auto_save = autosave::configure(
        s.no_auto_save,
        args.state_dir.as_deref(),
        std::env::var_os(ENV_STATE_DIRECTORY).as_deref(),
        Duration::from_secs(s.auto_save_delay),
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("cannot start async runtime")?;
    rt.block_on(async {
        // Timers are spawned on this runtime, so the service is built inside it.
        let service = AuraService::new(devices)
            .with_auto_save(auto_save)
            .with_precedence_dir(precedence_dir.as_deref())
            .with_reapply_window(Duration::from_secs(s.reapply_window));
        let flusher = service.flusher();
        let shutdown = shutdown_signal()?;
        let result = server::serve_uds(service, &s.socket, shutdown).await;
        flusher.flush(FLUSH_TIMEOUT).await;
        result
    })
}

/// Resolves on SIGTERM or SIGINT. Must be called inside the runtime.
fn shutdown_signal() -> Result<impl std::future::Future<Output = ()>> {
    let mut term = signal(SignalKind::terminate()).context("cannot install SIGTERM handler")?;
    let mut int = signal(SignalKind::interrupt()).context("cannot install SIGINT handler")?;
    Ok(async move {
        tokio::select! {
            _ = term.recv() => info!("SIGTERM received, shutting down"),
            _ = int.recv() => info!("SIGINT received, shutting down"),
        }
    })
}

/// Finds exactly one supported controller in sysfs.
fn find_single(sysfs_root: &Path) -> Result<FoundController> {
    let mut found = discovery::discover(sysfs_root)
        .with_context(|| format!("cannot scan {}/class/hidraw", sysfs_root.display()))?;
    let f = match found.len() {
        0 => bail!("no supported controller found (USB 0b05:19af, lighting interface)"),
        1 => found.remove(0),
        n => {
            warn!("{n} supported controllers found; using the first");
            found.remove(0)
        }
    };
    println!(
        "{} at usb {} interface {}: {}",
        f.model.name,
        f.usb_name,
        f.interface.map_or("?".to_string(), |i| i.to_string()),
        f.dev_path().display()
    );
    Ok(f)
}

/// Discovers, opens and probes the controller (startup reads only).
fn open_probed(args: &Args) -> Result<(FoundController, AuraDevice)> {
    let f = find_single(&args.sysfs_root)?;
    let mut bus = open_bus(&f, &args.sysfs_root)?;
    let result = cc_asus_aura_argb::probe(&mut bus, QueryPolicy::default())
        .context("reading the config table failed")?;
    let board = discovery::board_name(&args.sysfs_root);
    let dev = build_device(&f, board.as_deref(), &result, Box::new(bus));
    Ok((f, dev))
}

fn probe(args: &Args) -> Result<()> {
    let f = find_single(&args.sysfs_root)?;
    let board = discovery::board_name(&args.sysfs_root);
    if let Some(b) = &board {
        println!("board:    {b}");
    }
    println!(
        "usb:      {} bcdDevice {}  ({})",
        f.key(),
        f.bcd_device.as_deref().unwrap_or("?"),
        f.usb_path.display()
    );
    let mut bus = open_bus(&f, &args.sysfs_root)?;
    let r = cc_asus_aura_argb::probe(&mut bus, QueryPolicy::default())
        .context("reading the config table failed")?;
    match (&r.firmware, &r.firmware_reply) {
        (Some(fw), Some(raw)) => {
            println!("firmware: {fw}");
            println!("  reply:  {}", protocol::hex(raw));
        }
        _ => println!("firmware: (no reply)"),
    }
    println!("config table reply:");
    println!("  {}", protocol::hex(&r.config_reply));
    println!("config table (reply bytes 4..64), rows of six:");
    for (i, row) in r.table.rows().enumerate() {
        println!("  {:02x}: {}", i * 6, protocol::hex(row));
    }
    let t = &r.table;
    println!("decoded:");
    println!("  0x02 addressable (ARGB) headers: {}", t.argb_headers());
    println!("  0x03 'RGB channels' (liquidctl):  {}", t.rgb_channels());
    println!("  0x1b mainboard colour slots:     {}", t.mainboard_leds());
    println!("  0x1d (meaning uncertain):        {}", t.byte_1d());
    let dev = build_device(&f, board.as_deref(), &r, Box::new(bus));
    println!("channels:");
    if dev.channels.is_empty() {
        println!("  (none)");
    }
    for c in &dev.channels {
        let l = &c.layout;
        println!(
            "  {:<10} {:<16} effect channel {}, colour slot(s) {}..{}",
            c.id,
            c.label,
            l.effect_channel,
            l.first_slot,
            l.first_slot + l.slots - 1
        );
    }
    Ok(())
}

fn print_modes() {
    println!("Effects, available on every channel (run by the controller; no speed setting):");
    for e in Effect::ALL {
        println!(
            "  {:<28} 0x{:02x}  {}",
            e.name(),
            e.value(),
            if e.takes_color() {
                "1 colour"
            } else {
                "no colour"
            }
        );
    }
}

fn set(
    args: &Args,
    state_dir: &Path,
    channel: &str,
    mode: &str,
    color: Option<&str>,
    effect_only: bool,
    save: bool,
) -> Result<()> {
    // Everything that can be checked without the device is checked first, so bad
    // arguments never cause I/O.
    let colors: Vec<Rgb> = color
        .map(|c| Rgb::parse_hex(c).ok_or_else(|| anyhow!("invalid colour '{c}', expected RRGGBB")))
        .transpose()?
        .into_iter()
        .collect();
    let effect = Effect::from_name(mode)
        .ok_or_else(|| anyhow!("unknown mode '{mode}'; run `{SERVICE_NAME} modes`"))?;
    if effect.takes_color() && colors.is_empty() {
        bail!("mode '{mode}' needs --color RRGGBB");
    }

    let (f, dev) = open_probed(args)?;
    let covered: Vec<usize> = if channel == SYNC_ID {
        (0..dev.channels.len()).collect()
    } else {
        dev.index_of(channel).into_iter().collect()
    };
    if covered.is_empty() {
        let ids: Vec<&str> = dev.channels.iter().map(|c| c.id.as_str()).collect();
        bail!(
            "no channel '{channel}'; this controller has: {}, {SYNC_ID}",
            ids.join(", ")
        );
    }
    let setting = dev.channels[covered[0]]
        .setting(mode, &colors)
        .map_err(|e| anyhow!(e))?;
    let targets: Vec<(usize, Setting)> = covered.iter().map(|&i| (i, setting)).collect();
    dev.apply_many(&targets, true, effect_only)?;
    record_precedence(state_dir, &f, channel, setting);
    let what = if effect_only {
        " (effect report only)"
    } else {
        ""
    };
    println!("{channel}: applied {setting}{what}");
    if save {
        dev.save_now().context("writing the save report failed")?;
        println!("saved the effect settings to the controller's memory");
        forget(state_dir, &f);
    }
    Ok(())
}

fn save(args: &Args, state_dir: &Path) -> Result<()> {
    let (f, dev) = open_probed(args)?;
    dev.save_now().context("writing the save report failed")?;
    println!("saved the effect settings the controller runs now to its memory");
    forget(state_dir, &f);
    Ok(())
}

/// A CLI `set` is a user change: give it a new sequence number in the service's
/// precedence record, if the state directory exists.
fn record_precedence(state_dir: &Path, f: &FoundController, channel: &str, setting: Setting) {
    if !state_dir.is_dir() {
        return;
    }
    let mut store = PrecedenceStore::in_dir(state_dir, &f.key());
    let mut records = store.load();
    records.request(channel, PRequest::Set(setting), false);
    match store.store(&records) {
        Ok(()) => info!(
            "Recorded {channel} as the latest change in {}",
            state_dir.display()
        ),
        Err(e) => warn!(
            "Cannot update the precedence record in {}: {e}",
            state_dir.display()
        ),
    }
}

/// After a CLI save the service's record no longer matches; make it "unknown".
fn forget(state_dir: &Path, f: &FoundController) {
    match autosave::cli_forget(state_dir, &f.key()) {
        Ok(true) => info!("Cleared the auto-save record in {}", state_dir.display()),
        Ok(false) => {}
        Err(e) => warn!(
            "Cannot clear the auto-save record in {}: {e}",
            state_dir.display()
        ),
    }
}

/// `--debug` wins; otherwise `CC_LOG` (as set by CoolerControl) or `RUST_LOG`
/// style levels; default info. Under journald no timestamps are added.
fn setup_logging(debug: bool) {
    let level = if debug {
        LevelFilter::Debug
    } else {
        std::env::var(ENV_CC_LOG)
            .ok()
            .and_then(|l| LevelFilter::from_str(&l).ok())
            .unwrap_or(LevelFilter::Info)
    };
    let mut builder = env_logger::Builder::new();
    builder
        .filter_level(LevelFilter::Warn)
        .filter_module("cc_asus_aura_argb", level);
    if std::env::var_os("JOURNAL_STREAM").is_some() {
        builder.format_timestamp(None).format_target(false);
    }
    builder.init();
}
