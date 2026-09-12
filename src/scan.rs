//! Running scanimage with appscan's recovery rules, and the scans built on top of it.

use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::pdf;
use crate::profile::{self, Profile};
use crate::sane::{self, Device, Format, Opt, Settings};

/// Returned (inside anyhow) when a job was cancelled.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

pub const WARMUP_HINT: &str = "Starting the scanner (a cold lamp needs up to 40 s to warm up)…";

/// A cancellable sequence of scanimage runs on one device. Share it between the thread doing
/// the work and the one that may cancel it.
pub struct Job {
    profile: Option<&'static Profile>,
    child: Mutex<Option<Child>>,
    /// Image data is flowing: a "Progress:" line was seen.
    started: AtomicBool,
    cancelled: AtomicBool,
}

impl Job {
    pub fn new(profile: Option<&'static Profile>) -> Self {
        Self {
            profile,
            child: Mutex::new(None),
            started: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(SeqCst)
    }

    /// Run scanimage with `args(current device name)` and return its stdout. Retries once when
    /// the open fails with an I/O error: right after the power-on firmware upload SANE 1.1.1
    /// often reports one although the scanner is ready.
    pub fn run(
        &self,
        device: &Device,
        args: &dyn Fn(&str) -> Result<Vec<OsString>>,
        on_progress: &mut dyn FnMut(f64),
    ) -> Result<String> {
        for attempt in 0.. {
            let name = self.resolve(device)?;
            let mut cmd = sane::scanimage(device.profile)?;
            cmd.args(args(&name)?)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let (stderr, mut stdout) = {
                let mut guard = self.child.lock().unwrap();
                if self.is_cancelled() {
                    bail!(Cancelled); // cancel() found nothing to stop: never start
                }
                let mut child = cmd
                    .spawn()
                    .map_err(|e| profile::missing_tool("scanimage", e))?;
                self.started.store(false, SeqCst);
                let pipes = (child.stderr.take().unwrap(), child.stdout.take().unwrap());
                *guard = Some(child);
                pipes
            };
            let reader = thread::spawn(move || {
                let mut out = String::new();
                let _ = stdout.read_to_string(&mut out);
                out
            });
            let mut errors = String::new();
            for_each_line(stderr, |line| match parse_progress(line) {
                Some(percent) => {
                    self.started.store(true, SeqCst);
                    on_progress(percent);
                }
                None if !line.trim().is_empty() => {
                    errors.push_str(line.trim());
                    errors.push('\n');
                }
                None => {}
            });
            let stdout = reader.join().unwrap_or_default();
            let status = self.wait();
            if status.success() {
                return Ok(stdout);
            }
            if self.is_cancelled() {
                bail!(Cancelled);
            }
            if attempt == 0 && is_open_io_error(&errors) {
                continue;
            }
            match errors.trim() {
                "" => bail!("scanimage failed ({status})"),
                errors => bail!("{errors}"),
            }
        }
        unreachable!()
    }

    /// The device name to use now. A profile device's `libusb:BUS:DEV` name changes when it is
    /// replugged or reset, so it is looked up every time; if the scanner is plugged in but
    /// doesn't answer, it is wedged, and a USB reset revives it.
    fn resolve(&self, device: &Device) -> Result<String> {
        let Some(profile) = device.profile else {
            return Ok(device.name.clone());
        };
        let names = sane::profile_names(profile, &|| self.is_cancelled())?;
        if self.is_cancelled() {
            bail!(Cancelled);
        }
        if names.contains(&device.name) {
            return Ok(device.name.clone()); // still there: keep the scanner the user chose
        }
        names
            .into_iter()
            .next()
            .context("the scanner is not responding. Switch it off and on again")
    }

    /// Wait for the running scanimage. It needs the lock, so it waits for a cancel in progress
    /// (including the USB reset) to finish.
    fn wait(&self) -> ExitStatus {
        loop {
            {
                let mut guard = self.child.lock().unwrap();
                match guard.as_mut().map(Child::try_wait) {
                    Some(Ok(Some(status))) => {
                        *guard = None;
                        return status;
                    }
                    Some(Err(_)) | None => {
                        *guard = None;
                        return ExitStatus::from_raw(1 << 8);
                    }
                    Some(Ok(None)) => {}
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Cancel the job. Only the first call does anything; it may take ~10 s.
    ///
    /// On the Epson 2480, SIGINT is only safe once image data flows (earlier it wedges the
    /// scanner), and even then scanimage's handler hangs in the backend about one time in three.
    /// So: SIGINT when safe, then kill if it doesn't exit within 10 s, and reset the scanner.
    pub fn cancel(&self) {
        let mut guard = self.child.lock().unwrap();
        if self.cancelled.swap(true, SeqCst) {
            return;
        }
        let Some(child) = guard.as_mut() else { return }; // run() now refuses to start
        let exited = |child: &mut Child| matches!(child.try_wait(), Ok(Some(_)));
        if exited(child) {
            return;
        }
        let wedges = self.profile.is_some_and(|p| p.early_cancel_wedges);
        if self.started.load(SeqCst) || !wedges {
            // SAFETY: the child is not reaped while we hold the lock, so the PID is still ours.
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
            for _ in 0..100 {
                if exited(child) {
                    return;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        if let Some(profile) = self.profile {
            let _ = profile::usb_reset(profile);
        }
    }
}

/// Split scanimage's stderr on '\n' and '\r' (progress lines end with '\r').
fn for_each_line(mut reader: impl Read, mut f: impl FnMut(&str)) {
    let (mut pending, mut buf) = (Vec::new(), [0u8; 4096]);
    while let Ok(n @ 1..) = reader.read(&mut buf) {
        for &byte in &buf[..n] {
            if byte == b'\n' || byte == b'\r' {
                f(&String::from_utf8_lossy(&pending));
                pending.clear();
            } else {
                pending.push(byte);
            }
        }
    }
    if !pending.is_empty() {
        f(&String::from_utf8_lossy(&pending));
    }
}

fn parse_progress(line: &str) -> Option<f64> {
    line.split_once("Progress: ")?
        .1
        .trim_end()
        .strip_suffix('%')?
        .parse()
        .ok()
}

fn is_open_io_error(errors: &str) -> bool {
    errors.contains("open of device") && errors.contains("I/O")
}

/// The scanner's options for the given source and mode (they decide which options are active).
pub fn options(job: &Job, device: &Device, settings: &Settings) -> Result<Vec<Opt>> {
    let query = |extra: Vec<String>| {
        let args = |name: &str| {
            Ok([
                vec!["-d".into(), name.into()],
                extra.clone(),
                vec!["-A".into()],
            ]
            .concat())
        };
        job.run(
            device,
            &|name| args(name).map(|a: Vec<String>| a.into_iter().map(OsString::from).collect()),
            &mut |_| {},
        )
    };
    let base = sane::parse_options(&query(vec![])?);
    let only = Settings {
        source: settings.source.clone(),
        mode: settings.mode.clone(),
        ..Default::default()
    };
    if only == Settings::default() {
        return Ok(base);
    }
    Ok(sane::parse_options(&query(sane::option_args(
        &base, &only,
    )?)?))
}

/// Scan to `output` (.png .jpg .tif .pnm or .pdf) and return the resolution used.
pub fn scan_to(
    job: &Job,
    device: &Device,
    opts: &[Opt],
    settings: &Settings,
    output: &Path,
    on_progress: &mut dyn FnMut(f64),
) -> Result<u32> {
    match sane::format_for(output)? {
        Format::Image(format) => {
            scan_image(job, device, opts, settings, output, format, on_progress)
        }
        Format::Pdf => {
            let tmp = TempDir::new()?;
            let page = tmp.path().join("page.pnm");
            let dpi = scan_pdf_page(job, device, opts, settings, &page, on_progress)?;
            pdf::write(&[(page, dpi)], output)?;
            Ok(dpi)
        }
    }
}

/// Scan one PDF page as PNM: always 8-bit, whatever the depth setting.
pub fn scan_pdf_page(
    job: &Job,
    device: &Device,
    opts: &[Opt],
    settings: &Settings,
    page: &Path,
    on_progress: &mut dyn FnMut(f64),
) -> Result<u32> {
    let settings = Settings {
        depth: sane::find(opts, "--depth").map(|_| 8),
        ..settings.clone()
    };
    scan_image(job, device, opts, &settings, page, "pnm", on_progress)
}

fn scan_image(
    job: &Job,
    device: &Device,
    opts: &[Opt],
    settings: &Settings,
    output: &Path,
    format: &str,
    on_progress: &mut dyn FnMut(f64),
) -> Result<u32> {
    sane::scan_args("", opts, settings, output, format)?; // validate before touching the scanner
    // scanimage writes straight to its output: a failed or cancelled scan must neither leave a
    // broken file nor destroy an existing one.
    let part = part_path(output);
    let result = job.run(
        device,
        &|name| sane::scan_args(name, opts, settings, &part, format),
        on_progress,
    );
    match result {
        Ok(_) => fs::rename(&part, output)
            .with_context(|| format!("can't write {}", output.display()))?,
        Err(e) => {
            let _ = fs::remove_file(&part);
            return Err(e);
        }
    }
    Ok(sane::effective_resolution(opts, settings))
}

/// `output` + ".part", without the lossy UTF-8 conversion of `Path::display`.
pub fn part_path(output: &Path) -> PathBuf {
    let mut part = output.as_os_str().to_owned();
    part.push(".part");
    PathBuf::from(part)
}

/// A tiny scan that switches the lamp on and waits until it is warm.
pub fn warmup(job: &Job, device: &Device, opts: &[Opt]) -> Result<()> {
    let tmp = TempDir::new()?;
    let settings = Settings {
        resolution: sane::low_resolution(opts, 50),
        area: Some([0.0, 0.0, 5.0, 5.0]),
        ..Default::default()
    };
    scan_image(
        job,
        device,
        opts,
        &settings,
        &tmp.path().join("warmup.pnm"),
        "pnm",
        &mut |_| {},
    )?;
    Ok(())
}

/// A private temporary directory, removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> Result<Self> {
        let base = std::env::temp_dir();
        for n in 0.. {
            let path = base.join(format!("appscan-{}-{n}", std::process::id()));
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e).context("can't create a temporary directory"),
            }
        }
        unreachable!()
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_progress_and_errors() {
        assert_eq!(parse_progress("Progress: 12.3%"), Some(12.3));
        assert_eq!(
            parse_progress("scanimage: sane_read: Operation was canceled"),
            None
        );
        let mut lines = vec![];
        for_each_line(&b"Progress: 1.0%\rProgress: 2.5%\rscanimage: open of device x failed: Error during device I/O\n"[..], |l| {
            lines.push(l.to_owned())
        });
        assert_eq!(lines.len(), 3);
        assert!(is_open_io_error(&lines[2]));
        assert!(!is_open_io_error(
            "scanimage: sane_start: Error during device I/O"
        ));
    }

    #[test]
    fn cancel_before_start_prevents_running() {
        let device = sane::named("test:0", String::new());
        let job = Job::new(device.profile);
        job.cancel();
        job.cancel(); // only the first call counts
        let error = job
            .run(&device, &|_| Ok(vec!["--version".into()]), &mut |_| {})
            .unwrap_err();
        assert!(error.is::<Cancelled>());
    }
}
