//! Talking to SANE through `scanimage`: configuration, device discovery, reading a scanner's
//! options (`scanimage -A`) and turning settings into arguments.

use std::ffi::OsString;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Result, bail, ensure};

use crate::profile::{self, Profile};

/// Listing can hang on network backends, and a wedged USB scanner makes SANE wait on 30 s timeouts.
const LIST_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
pub struct Device {
    pub name: String,
    pub label: String,
    pub profile: Option<&'static Profile>,
}

/// A usage error: invalid settings for this scanner (exit status 2).
#[derive(Debug)]
pub struct Invalid(pub String);

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Invalid {}

macro_rules! invalid {
    ($($arg:tt)*) => { return Err(Invalid(format!($($arg)*)).into()) };
}

/// A `scanimage` command. Profile devices get a private SANE configuration; generic devices use
/// the system's. It runs in its own process group: a terminal Ctrl-C must reach only appscan,
/// because interrupting some scanners at the wrong moment wedges them (see [`crate::scan::Job`]).
pub fn scanimage(profile: Option<&Profile>) -> Result<Command> {
    let mut cmd = Command::new("scanimage");
    cmd.process_group(0);
    if let Some(profile) = profile {
        let dir = profile::data_dir().join("sane.d");
        let dir_str = dir.to_string_lossy().into_owned();
        // ':' separates SANE_CONFIG_DIR entries and '"' would end the quoted firmware path.
        ensure!(
            !dir_str.contains(':') && !dir_str.contains('"'),
            "appscan can't keep its data in a path containing ':' or '\"': {dir_str}"
        );
        let mut conf = profile.conf.to_owned();
        if let Some(firmware) = &profile.firmware {
            let path = profile::firmware_path(firmware);
            ensure!(
                path.exists(),
                "the {} needs its firmware: run 'appscan firmware' first",
                profile.name
            );
            conf = conf.replace("{firmware}", &path.to_string_lossy());
        }
        fs::create_dir_all(dir.join("dll.d"))?; // empty, so /etc/sane.d/dll.d is skipped
        fs::write(dir.join("dll.conf"), format!("{}\n", profile.backend))?;
        fs::write(dir.join(format!("{}.conf", profile.backend)), conf)?;
        cmd.env("SANE_CONFIG_DIR", format!("{dir_str}:")); // trailing ':' falls back to /etc/sane.d
    }
    Ok(cmd)
}

/// Run a short scanimage command (listing) to completion, killing it after `timeout`.
fn capture(mut cmd: Command, timeout: Duration) -> Result<String> {
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| profile::missing_tool("scanimage", e))?;
    let pid = child.id() as libc::pid_t;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait_with_output()); // drains stdout and stderr together
    });
    match rx.recv_timeout(timeout) {
        Ok(output) => {
            let output = output?;
            ensure!(
                output.status.success(),
                "scanimage failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        }
        Err(_) => {
            // SAFETY: the child is not reaped until the thread's wait() returns, so the PID is still ours.
            unsafe { libc::kill(pid, libc::SIGKILL) };
            bail!("scanimage didn't answer within {} s", timeout.as_secs())
        }
    }
}

/// SANE names and descriptions of the devices a configuration sees.
fn list(profile: Option<&Profile>) -> Result<Vec<(String, String)>> {
    let mut cmd = scanimage(profile)?;
    cmd.args(["-f", "%d\t%v %m%n"]);
    let out = capture(cmd, LIST_TIMEOUT)?;
    Ok(out
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(name, label)| (name.to_owned(), label.trim().to_owned()))
        .collect())
}

/// Current SANE names of a profile's scanner (they change on replug or USB reset). The profile's
/// USB device is plugged in, so if SANE doesn't see it the scanner is wedged: reset it and look
/// again, unless `cancelled` says to stop.
pub fn profile_names(profile: &Profile, cancelled: &dyn Fn() -> bool) -> Result<Vec<String>> {
    let names = || -> Result<Vec<String>> {
        Ok(list(Some(profile))?
            .into_iter()
            .map(|(name, _)| name)
            .collect())
    };
    let first = names()?;
    if !first.is_empty() || cancelled() {
        return Ok(first);
    }
    profile::usb_reset(profile)?;
    thread::sleep(Duration::from_secs(3)); // ponytail: fixed wait for USB re-enumeration
    if cancelled() { Ok(first) } else { names() }
}

/// Scanners appscan can use, profile devices first. The system configuration (all backends,
/// slow with network scanners) is asked only with `all` or when no profile device answers.
pub fn discover(all: bool) -> Result<Vec<Device>> {
    let mut devices = vec![];
    for p in profile::connected() {
        for name in profile_names(p, &|| false)? {
            devices.push(Device {
                name,
                label: p.name.to_owned(),
                profile: Some(p),
            });
        }
    }
    if all || devices.is_empty() {
        for (name, label) in list(None)? {
            if !devices.iter().any(|d| d.name == name) {
                devices.push(named(&name, label));
            }
        }
    }
    Ok(devices)
}

pub fn named(name: &str, label: String) -> Device {
    Device {
        name: name.to_owned(),
        label,
        profile: profile::for_device(name),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    List(Vec<String>),
    Range { min: f64, max: f64 },
    Bool,
}

/// One backend option as `scanimage -A` describes it.
#[derive(Clone, Debug, PartialEq)]
pub struct Opt {
    pub name: String,
    pub kind: Kind,
    pub default: Option<String>,
    pub active: bool,
    pub unit: String,
}

#[cfg(test)]
impl Opt {
    pub fn values(&self) -> &[String] {
        match &self.kind {
            Kind::List(values) => values,
            _ => &[],
        }
    }
}

pub fn find<'a>(opts: &'a [Opt], name: &str) -> Option<&'a Opt> {
    opts.iter().find(|o| o.name == name)
}

/// Parse `scanimage -A`. Options without a usable constraint (buttons, strings, gamma tables)
/// are skipped: appscan never sets them.
pub fn parse_options(text: &str) -> Vec<Opt> {
    text.lines().filter_map(parse_option).collect()
}

fn parse_option(line: &str) -> Option<Opt> {
    // "    --resolution auto||50|75|...|2400dpi [300]": options are indented by 4 spaces,
    // their descriptions by 8.
    let line = line.strip_prefix("    ").filter(|l| l.starts_with('-'))?;
    let name_end = line.find([' ', '[']).unwrap_or(line.len());
    let (name, mut rest) = (line[..name_end].to_owned(), line[name_end..].trim());
    let (mut default, mut active) = (None, true);
    // Peel "[default] [inactive] [advanced]" off the end, stopping at the default.
    while default.is_none() && rest.ends_with(']') {
        let open = group_start(rest)?;
        let group = &rest[open + 1..rest.len() - 1];
        if group.starts_with('=') {
            break; // "[=(yes|no)]" belongs to a boolean's constraint
        }
        match group {
            // appscan can't set these either
            "inactive" | "read-only" | "hardware" => active = false,
            "advanced" | "software" | "emulated" | "automatic" => {}
            value => default = Some(value.to_owned()),
        }
        rest = rest[..open].trim_end();
    }
    let rest = rest.split(" (in steps of").next().unwrap_or(rest).trim();
    let mut unit = "";
    let kind = if rest.starts_with("[=(") {
        Kind::Bool
    } else {
        let mut items: Vec<&str> = rest
            .split('|')
            .filter(|i| !i.is_empty() && *i != "auto")
            .collect();
        let last = *items.last()?;
        if last.starts_with('<') {
            return None; // <string>, <int>, ...
        } else if items.len() == 1 && last.contains("..") {
            // "-400..400%"
            let (min, max) = last.split_once("..")?;
            let (max, max_unit) = split_unit(max)?;
            unit = max_unit;
            Kind::Range {
                min: min.parse().ok()?,
                max,
            }
        } else {
            // "8|16bit": when every item is a number, the unit is glued to the last one.
            if let Some((_, suffix)) = split_unit(last)
                && items[..items.len() - 1]
                    .iter()
                    .all(|i| i.parse::<f64>().is_ok())
            {
                unit = suffix;
                *items.last_mut()? = &last[..last.len() - suffix.len()];
            }
            Kind::List(items.iter().map(|i| i.to_string()).collect())
        }
    };
    Some(Opt {
        name,
        kind,
        default,
        active,
        unit: unit.to_owned(),
    })
}

/// Where the bracket group ending `text` starts; values may contain brackets themselves
/// (brscan's "Gray[Error Diffusion]").
fn group_start(text: &str) -> Option<usize> {
    let mut depth = 0;
    for (i, c) in text.char_indices().rev() {
        match c {
            ']' => depth += 1,
            '[' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// "216mm" -> (216.0, "mm"); None unless it is a number followed by a unit (letters or %).
fn split_unit(text: &str) -> Option<(f64, &str)> {
    let end = text.rfind(|c: char| c.is_ascii_digit())? + 1;
    let unit = &text[end..];
    unit.chars()
        .all(|c| c.is_ascii_alphabetic() || c == '%')
        .then_some(())?;
    Some((text[..end].parse().ok()?, unit))
}

/// What the user wants; `None` leaves the scanner's current value.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    pub source: Option<String>,
    pub mode: Option<String>,
    pub resolution: Option<u32>,
    pub depth: Option<u32>,
    pub brightness: Option<i32>,
    pub contrast: Option<i32>,
    /// Left, top, width, height in mm.
    pub area: Option<[f64; 4]>,
}

/// Arguments setting `settings` on a scanner with options `opts`. Source and mode come first
/// because they change the other options' ranges; the scan area comes last. Options the
/// scanner has but that are inactive right now (the Epson's brightness in Lineart) are skipped.
pub fn option_args(opts: &[Opt], settings: &Settings) -> Result<Vec<String>> {
    let s = settings;
    let mut args = vec![];
    let area = s.area.map(|a| a.map(|v| v.to_string()));
    let wanted = [
        ("--source", s.source.clone()),
        ("--mode", s.mode.clone()),
        ("--resolution", s.resolution.map(|v| v.to_string())),
        ("--depth", s.depth.map(|v| v.to_string())),
        ("--brightness", s.brightness.map(|v| v.to_string())),
        ("--contrast", s.contrast.map(|v| v.to_string())),
        ("-l", area.as_ref().map(|a| a[0].clone())),
        ("-t", area.as_ref().map(|a| a[1].clone())),
        ("-x", area.as_ref().map(|a| a[2].clone())),
        ("-y", area.as_ref().map(|a| a[3].clone())),
    ];
    for (name, value) in wanted {
        let Some(value) = value else { continue };
        let Some(opt) = find(opts, name) else {
            invalid!("this scanner has no {name} option")
        };
        if !opt.active {
            continue;
        }
        match &opt.kind {
            Kind::List(values) if !values.iter().any(|v| same(v, &value)) => {
                invalid!("{name} must be one of: {}", values.join(", "))
            }
            Kind::Range { min, max } => match value.parse::<f64>() {
                Ok(v) if (*min..=*max).contains(&v) => {}
                _ => invalid!("{name} must be between {min} and {max}{}", opt.unit),
            },
            _ => {}
        }
        args.extend([name.to_owned(), value]);
    }
    Ok(args)
}

fn same(a: &str, b: &str) -> bool {
    match (a.parse::<f64>(), b.parse::<f64>()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

/// How appscan writes a file: scanimage's format, or a PDF it assembles from PNM pages.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Format {
    Image(&'static str),
    Pdf,
}

pub fn format_for(path: &Path) -> Result<Format> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase();
    Ok(match ext.as_str() {
        "png" => Format::Image("png"),
        "jpg" | "jpeg" => Format::Image("jpeg"),
        "tif" | "tiff" => Format::Image("tiff"),
        "pnm" => Format::Image("pnm"),
        "pdf" => Format::Pdf,
        _ => invalid!(
            "unsupported file type {:?}; use .png .jpg .tif .pnm or .pdf",
            format!(".{ext}")
        ),
    })
}

/// Full arguments for scanning to `output` (an image format scanimage writes itself).
pub fn scan_args(
    device: &str,
    opts: &[Opt],
    settings: &Settings,
    output: &Path,
    format: &str,
) -> Result<Vec<OsString>> {
    if settings.depth == Some(16) && !matches!(format, "png" | "tiff" | "pnm") {
        invalid!("16-bit scans need a .png, .tif or .pnm file");
    }
    let mut args: Vec<OsString> = vec!["-d".into(), device.into()];
    args.extend(option_args(opts, settings)?.into_iter().map(OsString::from));
    args.extend([
        "--format".into(),
        format.into(),
        "-o".into(),
        output.into(),
        "-p".into(),
    ]);
    Ok(args)
}

/// The resolution a scan will use: the chosen one or the scanner's default.
pub fn effective_resolution(opts: &[Opt], settings: &Settings) -> u32 {
    settings
        .resolution
        .or_else(|| {
            find(opts, "--resolution")?
                .default
                .as_deref()?
                .parse::<f64>()
                .ok()
                .map(|v| v as u32)
        })
        .unwrap_or(300)
}

/// The lowest resolution at least `floor` dpi (or the lowest available), for previews and warm-up.
pub fn low_resolution(opts: &[Opt], floor: u32) -> Option<u32> {
    let opt = find(opts, "--resolution")?;
    let mut values: Vec<u32> = match &opt.kind {
        Kind::List(values) => values
            .iter()
            .filter_map(|v| v.parse::<f64>().ok())
            .map(|v| v as u32)
            .collect(),
        Kind::Range { min, max } => vec![(*min).max(floor as f64).min(*max) as u32],
        Kind::Bool => return None,
    };
    values.sort_unstable();
    values
        .iter()
        .copied()
        .find(|v| *v >= floor)
        .or(values.first().copied())
}

/// The scan area of the current source in mm (width, height).
pub fn bed_size(opts: &[Opt]) -> (f64, f64) {
    let max = |name, fallback| match find(opts, name).map(|o| &o.kind) {
        Some(Kind::Range { max, .. }) => *max,
        _ => fallback,
    };
    (max("-x", 216.0), max("-y", 297.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPSON_2480: &str = include_str!("../tests/fixtures/epson-2480-flatbed.txt");
    const EPSON_2480_TPU: &str = include_str!("../tests/fixtures/epson-2480-transparency.txt");

    fn opt<'a>(opts: &'a [Opt], name: &str) -> &'a Opt {
        find(opts, name).unwrap_or_else(|| panic!("{name} missing"))
    }

    #[test]
    fn parses_epson_options() {
        let opts = parse_options(EPSON_2480);
        let resolution = opt(&opts, "--resolution");
        assert_eq!(resolution.values().len(), 10);
        assert_eq!(resolution.values()[0], "50");
        assert_eq!(resolution.values()[9], "2400");
        assert_eq!(
            (resolution.unit.as_str(), resolution.default.as_deref()),
            ("dpi", Some("300"))
        );
        assert_eq!(
            opt(&opts, "--mode").values(),
            ["Color", "Halftone", "Gray", "Lineart"]
        );
        assert_eq!(
            opt(&opts, "--preview-mode").default.as_deref(),
            Some("Auto")
        ); // not "advanced"
        assert_eq!(
            opt(&opts, "--source").values(),
            ["Flatbed", "Transparency Adapter"]
        );
        assert_eq!(opt(&opts, "--preview").kind, Kind::Bool);
        let frame = opt(&opts, "--Frame");
        assert_eq!(
            (frame.kind.clone(), frame.active),
            (Kind::Range { min: 1.0, max: 6.0 }, false)
        );
        assert_eq!(
            opt(&opts, "-x").kind,
            Kind::Range {
                min: 0.0,
                max: 216.0
            }
        );
        assert_eq!(opt(&opts, "-x").unit, "mm");
        let depth = opt(&opts, "--depth");
        assert_eq!(
            (depth.values(), depth.unit.as_str()),
            (&["8".to_owned(), "16".to_owned()][..], "bit")
        );
        assert_eq!(
            opt(&opts, "--brightness").kind,
            Kind::Range {
                min: -400.0,
                max: 400.0
            }
        );
        assert_eq!(opt(&opts, "--predef-window").values()[1], "6x4 (inch)");
        assert!(find(&opts, "--gamma-table").is_none());
        assert_eq!(bed_size(&opts), (216.0, 297.0));
        assert_eq!(bed_size(&parse_options(EPSON_2480_TPU)), (55.0, 125.0));
        assert_eq!(
            (
                low_resolution(&opts, 50),
                effective_resolution(&opts, &Settings::default())
            ),
            (Some(50), 300)
        );
    }

    #[test]
    fn parses_other_backends() {
        let opts = parse_options(
            "    --mode 24bit Color[Fast]|Black & White|Gray[Error Diffusion] [24bit Color[Fast]]\n\
             \x20   --bool-soft-detect[=(yes|no)] [no] [read-only]\n",
        );
        assert_eq!(
            opts[0].values(),
            [
                "24bit Color[Fast]",
                "Black & White",
                "Gray[Error Diffusion]"
            ]
        );
        assert_eq!(opts[0].default.as_deref(), Some("24bit Color[Fast]"));
        assert_eq!(
            (
                opts[1].kind.clone(),
                opts[1].active,
                opts[1].default.as_deref()
            ),
            (Kind::Bool, false, Some("no"))
        );
    }

    #[test]
    fn builds_arguments() {
        let opts = parse_options(EPSON_2480);
        let settings = Settings {
            source: Some("Transparency Adapter".into()),
            mode: Some("Gray".into()),
            resolution: Some(600),
            brightness: Some(-50),
            area: Some([1.0, 2.5, 30.0, 40.0]),
            ..Default::default()
        };
        let args = scan_args(
            "snapscan:libusb:001:011",
            &opts,
            &settings,
            Path::new("out.png"),
            "png",
        )
        .unwrap();
        let args: Vec<_> = args.iter().map(|a| a.to_string_lossy()).collect();
        assert_eq!(
            args.join(" "),
            "-d snapscan:libusb:001:011 --source Transparency Adapter --mode Gray --resolution 600 \
             --brightness -50 -l 1 -t 2.5 -x 30 -y 40 --format png -o out.png -p"
        );
        assert_eq!(
            scan_args("d", &opts, &Settings::default(), Path::new("o.pnm"), "pnm")
                .unwrap()
                .len(),
            7
        );

        // Lineart: brightness is inactive, so it is dropped instead of failing.
        let mut lineart = opts.clone();
        lineart
            .iter_mut()
            .filter(|o| o.name == "--brightness")
            .for_each(|o| o.active = false);
        let s = Settings {
            brightness: Some(30),
            ..Default::default()
        };
        assert!(option_args(&lineart, &s).unwrap().is_empty());

        let bad = |s: Settings| option_args(&opts, &s).unwrap_err().is::<Invalid>();
        assert!(bad(Settings {
            resolution: Some(123),
            ..Default::default()
        }));
        assert!(bad(Settings {
            brightness: Some(401),
            ..Default::default()
        }));
        assert!(bad(Settings {
            area: Some([0.0, 0.0, 300.0, 10.0]),
            ..Default::default()
        }));
        let no_brightness: Vec<Opt> = opts
            .iter()
            .filter(|o| o.name != "--brightness")
            .cloned()
            .collect();
        assert!(
            option_args(
                &no_brightness,
                &Settings {
                    brightness: Some(1),
                    ..Default::default()
                }
            )
            .is_err()
        );
        let deep = Settings {
            depth: Some(16),
            ..Default::default()
        };
        assert!(
            scan_args("d", &opts, &deep, Path::new("o.jpg"), "jpeg")
                .unwrap_err()
                .is::<Invalid>()
        );
    }

    #[test]
    fn picks_formats() {
        assert_eq!(
            format_for(Path::new("a.JPG")).unwrap(),
            Format::Image("jpeg")
        );
        assert_eq!(format_for(Path::new("a.pdf")).unwrap(), Format::Pdf);
        assert!(format_for(Path::new("a.bmp")).unwrap_err().is::<Invalid>());
    }
}
