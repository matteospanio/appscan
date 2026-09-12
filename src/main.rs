//! appscan: scan with SANE scanners from a GTK app or the command line.

mod gui;
mod pdf;
mod profile;
mod sane;
mod scan;

use std::ffi::CString;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::{env, fs};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use sane::{Device, Invalid, Settings};
use scan::{Cancelled, Job, TempDir, WARMUP_HINT};

const APP_ID: &str = "io.github.matteospanio.appscan";
const DESKTOP: &str = include_str!("../share/applications/io.github.matteospanio.appscan.desktop");
const ICON: &[u8] = include_bytes!("../share/icons/hicolor/scalable/apps/appscan.svg");
const MAN_PAGE: &[u8] = include_bytes!("../share/man/man1/appscan.1");

#[derive(Parser)]
#[command(
    name = "appscan",
    version,
    about = "Scan with SANE scanners from a GTK app or the command line"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Open the graphical interface (the default)
    Gui {
        #[arg(short, long, value_name = "NAME")]
        device: Option<String>,
    },
    /// Scan to a file (.png .jpg .tif .pnm .pdf)
    Scan(ScanArgs),
    /// List scanners: SANE name, a tab, a description
    List,
    /// Warm up the lamp so the next scan starts right away
    Warmup {
        #[arg(short, long, value_name = "NAME")]
        device: Option<String>,
    },
    /// Reset the USB connection of scanners with a quirk profile that stopped responding
    Reset,
    /// Download and install the firmware scanners with a quirk profile need
    Firmware,
    /// Install the desktop entry, icon and man page (or remove them and appscan's data)
    Setup {
        #[arg(long)]
        remove: bool,
    },
}

#[derive(Args)]
struct ScanArgs {
    output: PathBuf,
    /// SANE device, as printed by `appscan list` (default: the first one)
    #[arg(short, long, value_name = "NAME")]
    device: Option<String>,
    /// Scan mode, e.g. Color, Gray or Lineart
    #[arg(long)]
    mode: Option<String>,
    /// Resolution in dpi
    #[arg(short, long, value_name = "DPI")]
    resolution: Option<u32>,
    /// Bits per channel (16 needs .png, .tif or .pnm)
    #[arg(long)]
    depth: Option<u32>,
    /// Scan source, e.g. Flatbed or "Transparency Adapter"
    #[arg(long)]
    source: Option<String>,
    /// Scan area in mm (default: the whole bed)
    #[arg(long, num_args = 4, value_names = ["LEFT", "TOP", "WIDTH", "HEIGHT"])]
    area: Option<Vec<f64>>,
    #[arg(long, value_name = "PERCENT", allow_negative_numbers = true)]
    brightness: Option<i32>,
    #[arg(long, value_name = "PERCENT", allow_negative_numbers = true)]
    contrast: Option<i32>,
    /// Scan N pages into one PDF, pressing Enter between pages
    #[arg(long, value_name = "N", default_value_t = 1)]
    pages: u32,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command.unwrap_or(Cmd::Gui { device: None }) {
        Cmd::Gui { device } => return gui::run(device),
        Cmd::Scan(args) => scan_command(args),
        Cmd::List => sane::discover(true).map(|devices| {
            for device in devices {
                println!("{}\t{}", device.name, device.label);
            }
        }),
        Cmd::Warmup { device } => pick_device(device).and_then(|device| {
            let job = Job::new(device.profile);
            eprintln!("{WARMUP_HINT}");
            interruptible(&job, || {
                scan::warmup(
                    &job,
                    &device,
                    &scan::options(&job, &device, &Settings::default())?,
                )
            })?;
            eprintln!("Scanner ready");
            Ok(())
        }),
        Cmd::Reset => reset(),
        Cmd::Firmware => firmware(),
        Cmd::Setup { remove } => setup(remove),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if e.is::<Cancelled>() => {
            eprintln!("\nCancelled");
            ExitCode::FAILURE
        }
        Err(e) if e.is::<Invalid>() => {
            eprintln!("appscan: error: {e}");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("\nerror: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn pick_device(name: Option<String>) -> Result<Device> {
    match name {
        Some(name) => Ok(sane::named(&name, String::new())),
        None => sane::discover(false)?
            .into_iter()
            .next()
            .context("no scanner found"),
    }
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigint(_: libc::c_int) {
    INTERRUPTED.store(true, SeqCst);
}

/// Run `work` while Ctrl-C cancels `job` (once; a second Ctrl-C can't cut the cancel short).
fn interruptible<T: Send>(job: &Job, work: impl FnOnce() -> Result<T> + Send) -> Result<T> {
    // Termination and a closed terminal cancel like Ctrl-C: scanimage runs in its own process
    // group, so it would outlive us otherwise.
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
        unsafe { libc::signal(signal, on_sigint as libc::sighandler_t) };
    }
    thread::scope(|scope| {
        let worker = scope.spawn(work);
        while !worker.is_finished() {
            if INTERRUPTED.swap(false, SeqCst) {
                eprint!("\nCancelling…");
                job.cancel();
            }
            thread::sleep(Duration::from_millis(100));
        }
        worker.join().expect("scan thread panicked")
    })
}

fn progress(percent: f64) {
    eprint!("\rScanning {percent:5.1}%");
}

fn scan_command(a: ScanArgs) -> Result<()> {
    let pdf = sane::format_for(&a.output)? == sane::Format::Pdf;
    if a.pages != 1 && (a.pages == 0 || !pdf) {
        bail!(Invalid(
            "--pages needs a positive number and a .pdf output".into()
        ));
    }
    let folder = a
        .output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let folder_c = CString::new(folder.as_os_str().as_bytes())?;
    // SAFETY: access() only reads the NUL-terminated path.
    if unsafe { libc::access(folder_c.as_ptr(), libc::W_OK) } != 0 {
        bail!(Invalid(format!("can't write to {}", folder.display())));
    }
    let settings = Settings {
        source: a.source,
        mode: a.mode,
        resolution: a.resolution,
        depth: a.depth,
        brightness: a.brightness,
        contrast: a.contrast,
        area: a.area.map(|v| [v[0], v[1], v[2], v[3]]),
    };
    let device = pick_device(a.device)?;
    if device.profile.is_some_and(|p| p.warmup) {
        eprintln!("{WARMUP_HINT}");
    }
    let job = Job::new(device.profile);
    interruptible(&job, || {
        let opts = scan::options(&job, &device, &settings)?;
        if a.pages > 1 {
            return scan_pages(&job, &device, &opts, &settings, &a.output, a.pages);
        }
        scan::scan_to(&job, &device, &opts, &settings, &a.output, &mut progress)?;
        eprintln!("\nSaved {}", a.output.display());
        Ok(())
    })
}

/// Scan `count` pages into one PDF, waiting for Enter between them. Stopping early (Ctrl-C or
/// end of input) or a failing page still saves the pages scanned so far.
fn scan_pages(
    job: &Job,
    device: &Device,
    opts: &[sane::Opt],
    settings: &Settings,
    output: &Path,
    count: u32,
) -> Result<()> {
    let tmp = TempDir::new()?;
    let mut pages = vec![];
    let result = (|| -> Result<()> {
        for n in 1..=count {
            if n > 1 {
                eprint!("\nPlace page {n} of {count} on the glass and press Enter… ");
                if !wait_for_enter(job)? {
                    return Ok(()); // end of input: stop early
                }
            }
            let page = tmp.path().join(format!("page{n}.pnm"));
            let dpi = scan::scan_pdf_page(job, device, opts, settings, &page, &mut progress)?;
            pages.push((page, dpi));
        }
        Ok(())
    })();
    match result {
        Err(e) if pages.is_empty() => return Err(e),
        Err(e) if e.is::<Cancelled>() => eprintln!("\nStopped early after {} page(s)", pages.len()),
        Err(e) => {
            pdf::write(&pages, output)?;
            eprintln!(
                "\nSaved the {} page(s) scanned before the error to {}",
                pages.len(),
                output.display()
            );
            return Err(e);
        }
        Ok(()) if pages.len() < count as usize => {
            eprintln!("\nStopped early after {} page(s)", pages.len())
        }
        Ok(()) => {}
    }
    pdf::write(&pages, output)?;
    eprintln!("\nSaved {} page(s) to {}", pages.len(), output.display());
    Ok(())
}

/// True on Enter, false at end of input, Cancelled on Ctrl-C. Stdin is read on its own thread so
/// that a Ctrl-C at the prompt isn't stuck behind a blocking read.
fn wait_for_enter(job: &Job) -> Result<bool> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut line = String::new();
        let _ = tx.send(
            io::stdin()
                .read_line(&mut line)
                .map(|n| n > 0)
                .unwrap_or(false),
        );
    });
    loop {
        if job.is_cancelled() {
            bail!(Cancelled);
        }
        if let Ok(entered) = rx.recv_timeout(Duration::from_millis(100)) {
            return Ok(entered);
        }
    }
}

fn reset() -> Result<()> {
    let profiles = profile::connected();
    if profiles.is_empty() {
        bail!("no scanner with a quirk profile is connected; unplug and replug other scanners");
    }
    for p in profiles {
        profile::usb_reset(p)?;
        println!(
            "{} reset; its lamp needs to warm up again before the next scan",
            p.name
        );
    }
    Ok(())
}

fn firmware() -> Result<()> {
    for p in profile::PROFILES.iter().filter(|p| p.firmware.is_some()) {
        if let Some(path) = profile::install_firmware(p)? {
            println!("Firmware for the {} installed: {}", p.name, path.display());
        }
    }
    Ok(())
}

/// The desktop entry with an absolute Exec, quoted as the desktop entry spec requires.
fn desktop_entry(exe: &Path) -> String {
    let mut quoted = String::new();
    for c in exe.to_string_lossy().chars() {
        match c {
            '"' | '`' | '$' => quoted.extend(['\\', c]),
            '\\' => quoted.push_str("\\\\\\\\"), // escaped for the quoting, then for the string value
            '%' => quoted.push_str("%%"),
            c => quoted.push(c),
        }
    }
    DESKTOP.replace("Exec=appscan gui", &format!("Exec=\"{quoted}\" gui"))
}

fn setup(remove: bool) -> Result<()> {
    let data_home = profile::data_dir()
        .parent()
        .expect("data dir has a parent")
        .to_owned();
    let files: [(PathBuf, &[u8]); 3] = [
        (
            data_home.join(format!("applications/{APP_ID}.desktop")),
            &[],
        ),
        (
            data_home.join("icons/hicolor/scalable/apps/appscan.svg"),
            ICON,
        ),
        (data_home.join("man/man1/appscan.1"), MAN_PAGE),
    ];
    let _ = fs::remove_file(data_home.join("applications/appscan.desktop")); // from appscan 0.1
    if remove {
        for (path, _) in &files {
            if let Err(e) = fs::remove_file(path)
                && e.kind() != io::ErrorKind::NotFound
            {
                return Err(e).with_context(|| format!("can't remove {}", path.display()));
            }
        }
        match fs::remove_dir_all(profile::data_dir()) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        println!(
            "Removed the desktop entry, icon, man page and {}",
            profile::data_dir().display()
        );
    } else {
        let exe = env::current_exe().context("can't find the appscan binary")?;
        for (path, contents) in &files {
            fs::create_dir_all(path.parent().expect("has a parent"))?;
            let desktop;
            let contents = if contents.is_empty() {
                desktop = desktop_entry(&exe);
                desktop.as_bytes()
            } else {
                contents
            };
            fs::write(path, contents).with_context(|| format!("can't write {}", path.display()))?;
        }
        println!(
            "Installed the desktop entry, icon and man page under {}",
            data_home.display()
        );
    }
    // Refresh only an icon cache that already exists: GTK trusts a user cache over the
    // directory, so creating one would hide icons other apps add later.
    let hicolor = data_home.join("icons/hicolor");
    if hicolor.join("icon-theme.cache").exists() {
        quietly(
            Command::new("gtk-update-icon-cache")
                .args(["-f", "-t"])
                .arg(hicolor),
        );
    }
    quietly(Command::new("mandb").args(["--user-db", "--quiet"]));
    let _ = io::stdout().flush();
    Ok(())
}

fn quietly(cmd: &mut Command) {
    let _ = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_is_consistent() {
        Cli::command().debug_assert();
        let cli = Cli::try_parse_from([
            "appscan",
            "scan",
            "o.png",
            "--brightness",
            "-30",
            "--area",
            "0",
            "0",
            "10.5",
            "20",
        ])
        .unwrap();
        let Some(Cmd::Scan(a)) = cli.command else {
            panic!("not a scan")
        };
        assert_eq!(
            (a.brightness, a.area),
            (Some(-30), Some(vec![0.0, 0.0, 10.5, 20.0]))
        );
    }

    #[test]
    fn quotes_exec_paths() {
        let entry = desktop_entry(Path::new("/home/a b/100%/$bin/appscan"));
        assert!(
            entry.contains("Exec=\"/home/a b/100%%/\\$bin/appscan\" gui\n"),
            "{entry}"
        );
        assert!(entry.contains("Icon=appscan"));
    }
}
