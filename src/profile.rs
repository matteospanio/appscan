//! Quirk profiles: scanners that need more than generic SANE handling.
//!
//! Any SANE scanner works without a profile. A profile adds what a model needs on top:
//! firmware, a private backend configuration, lamp warm-up and safe cancelling.
//! Supporting such a model means adding an entry to [`PROFILES`].

use std::env;
use std::fs;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};

use crate::scan::TempDir;

#[derive(Debug)]
pub struct Profile {
    pub name: &'static str,
    /// USB vendor and product ID.
    pub usb: (u16, u16),
    /// SANE backend; the private dll.conf lists only this one, so listing is fast.
    pub backend: &'static str,
    /// Contents of `<backend>.conf`; `{firmware}` becomes the firmware path.
    pub conf: &'static str,
    pub firmware: Option<Firmware>,
    /// The lamp needs a long warm-up after power-on: the GUI warms it up at start.
    pub warmup: bool,
    /// Interrupting the scanner before image data flows leaves it wedged, so cancelling
    /// kills scanimage and resets the USB device instead.
    pub early_cancel_wedges: bool,
}

#[derive(Debug)]
pub struct Firmware {
    /// File name under the data directory.
    pub file: &'static str,
    pub sha256: &'static str,
    /// Downloads and extracts the firmware into the given directory; returns its path.
    pub fetch: fn(&Path) -> Result<PathBuf>,
}

pub static PROFILES: &[Profile] = &[Profile {
    name: "Epson Perfection 2480/2580 PHOTO",
    usb: (0x04b8, 0x0121),
    backend: "snapscan",
    conf: "firmware \"{firmware}\"\nusb 0x04b8 0x0121\n",
    firmware: Some(Firmware {
        file: "esfw41.bin", // the same file for the 2480 and the 2580
        sha256: "fe200d47179276e52e24a9bf826a567ea202ae800cd1dbcaaeba472e497ab5e9",
        fetch: epson_2480_firmware,
    }),
    // Verified on a 2480: after power-on or a USB reset the scanner answers "Not ready" for
    // ~32 s; SIGINT during firmware upload or warm-up, or a kill during warm-up, wedges it.
    warmup: true,
    early_cancel_wedges: true,
}];

const SYSFS_USB: &str = "/sys/bus/usb/devices";

/// `$XDG_DATA_HOME/appscan`; a relative XDG_DATA_HOME is ignored, as the XDG spec says.
pub fn data_dir() -> PathBuf {
    let base = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(".local/share"));
    base.join("appscan")
}

pub fn home() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn firmware_path(firmware: &Firmware) -> PathBuf {
    data_dir().join(firmware.file)
}

/// Profiles whose scanner is plugged in.
pub fn connected() -> Vec<&'static Profile> {
    let present = usb_devices(Path::new(SYSFS_USB));
    PROFILES
        .iter()
        .filter(|p| present.iter().any(|(id, _)| *id == p.usb))
        .collect()
}

/// The profile for a SANE device name like `snapscan:libusb:001:011`: the backend must match and
/// the USB device at that bus address must be the profile's model, so another scanner on the same
/// backend doesn't get this model's quirks.
pub fn for_device(name: &str) -> Option<&'static Profile> {
    let (_, address) = name.rsplit_once("libusb:")?;
    let node = PathBuf::from(format!("/dev/bus/usb/{}", address.replace(':', "/")));
    let (id, _) = usb_devices(Path::new(SYSFS_USB))
        .into_iter()
        .find(|(_, n)| *n == node)?;
    PROFILES
        .iter()
        .find(|p| p.usb == id && name.starts_with(&format!("{}:", p.backend)))
}

/// USB devices under a sysfs root, as ((vendor, product), device node).
fn usb_devices(sysfs: &Path) -> Vec<((u16, u16), PathBuf)> {
    let Ok(entries) = fs::read_dir(sysfs) else {
        return vec![];
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let dir = entry.path();
            let read = |file: &str| fs::read_to_string(dir.join(file)).ok();
            let hex = |file: &str| u16::from_str_radix(read(file)?.trim(), 16).ok();
            let num = |file: &str| read(file)?.trim().parse::<u32>().ok();
            let node = format!("/dev/bus/usb/{:03}/{:03}", num("busnum")?, num("devnum")?);
            Some(((hex("idVendor")?, hex("idProduct")?), PathBuf::from(node)))
        })
        .collect()
}

/// Reset the scanner's USB port, like replugging it. Revives a wedged scanner; the lamp then
/// has to warm up again. Needs no root: desktop sessions give the user access to the node.
pub fn usb_reset(profile: &Profile) -> Result<()> {
    const USBDEVFS_RESET: libc::c_ulong = 0x5514; // _IO('U', 20) from linux/usbdevice_fs.h
    let Some((_, node)) = usb_devices(Path::new(SYSFS_USB))
        .into_iter()
        .find(|(id, _)| *id == profile.usb)
    else {
        bail!(
            "{} not found. Is it plugged in and switched on?",
            profile.name
        )
    };
    let file = fs::OpenOptions::new()
        .write(true)
        .open(&node)
        .with_context(|| format!("can't open {}", node.display()))?;
    // SAFETY: plain ioctl without an argument on a descriptor we own.
    if unsafe { libc::ioctl(file.as_raw_fd(), USBDEVFS_RESET as _) } < 0 {
        let error = std::io::Error::last_os_error();
        // ENODEV: the device re-enumerated during the reset, i.e. the reset worked.
        if error.raw_os_error() != Some(libc::ENODEV) {
            return Err(error).context("USB reset failed");
        }
    }
    Ok(())
}

/// Download, verify and install the firmware of a profile; None if it needs none.
pub fn install_firmware(profile: &Profile) -> Result<Option<PathBuf>> {
    let Some(firmware) = &profile.firmware else {
        return Ok(None);
    };
    let tmp = TempDir::new()?;
    let file = (firmware.fetch)(tmp.path())?;
    // A wrong firmware file hangs the scanner until it is power-cycled.
    ensure!(
        sha256(&file)? == firmware.sha256,
        "the extracted firmware has an unexpected checksum; not installing it"
    );
    let dest = firmware_path(firmware);
    fs::create_dir_all(dest.parent().expect("data dir has a parent"))?;
    let part = dest.with_extension("part");
    fs::copy(&file, &part)?;
    fs::rename(&part, &dest)?;
    Ok(Some(dest))
}

fn sha256(path: &Path) -> Result<String> {
    let out = tool(Command::new("sha256sum").arg(path), false)?;
    Ok(out.split_whitespace().next().unwrap_or_default().to_owned())
}

/// Epson ships the 2480/2580 firmware only inside its Windows driver.
fn epson_2480_firmware(tmp: &Path) -> Result<PathBuf> {
    const URL: &str = "https://ftp.epson.com/drivers/epson12204.exe"; // Epson Scan 3.04A
    let exe = tmp.join("driver.exe");
    eprintln!("Downloading {URL} (~20 MB)");
    tool(
        Command::new("curl")
            .args(["-fL", "--progress-bar", "-o"])
            .arg(&exe)
            .arg(URL),
        true,
    )?;
    let seven_zip = if on_path("7z") { "7z" } else { "7zz" }; // newer distros ship 7-Zip as 7zz
    let out_dir = format!("-o{}", tmp.display());
    tool(
        Command::new(seven_zip)
            .args(["e", "-y", &out_dir])
            .arg(&exe)
            .arg("ModUsd.cab"),
        false,
    )?;
    let cab = tmp.join("ModUsd.cab");
    tool(
        Command::new("cabextract")
            .args(["-q", "-F", "Esfw41.bin", "-d"])
            .arg(tmp)
            .arg(cab),
        false,
    )?;
    Ok(tmp.join("Esfw41.bin"))
}

fn on_path(name: &str) -> bool {
    env::var_os("PATH")
        .is_some_and(|path| env::split_paths(&path).any(|dir| dir.join(name).is_file()))
}

/// Run a helper tool and return its stdout. `show` lets its stderr (e.g. a progress bar) through.
fn tool(cmd: &mut Command, show: bool) -> Result<String> {
    let name = cmd.get_program().to_string_lossy().into_owned();
    let out = cmd
        .stdin(Stdio::null())
        .stderr(if show {
            Stdio::inherit()
        } else {
            Stdio::piped()
        })
        .output()
        .map_err(|e| missing_tool(&name, e))?;
    ensure!(
        out.status.success(),
        "{name} failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn missing_tool(name: &str, error: std::io::Error) -> anyhow::Error {
    if error.kind() == std::io::ErrorKind::NotFound {
        let package = match name {
            "scanimage" => "sane-utils",
            "7z" | "7zz" => "p7zip-full",
            other => other,
        };
        anyhow::anyhow!("{name} is missing (apt install {package})")
    } else {
        anyhow::Error::new(error).context(format!("can't run {name}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_usb_devices_from_sysfs() {
        let root = TempDir::new().unwrap();
        let scanner = root.path().join("1-1");
        fs::create_dir(&scanner).unwrap();
        for (file, value) in [
            ("idVendor", "04b8\n"),
            ("idProduct", "0121\n"),
            ("busnum", "1\n"),
            ("devnum", "11\n"),
        ] {
            fs::write(scanner.join(file), value).unwrap();
        }
        fs::create_dir(root.path().join("1-1:1.0")).unwrap(); // interfaces have no IDs: skipped
        assert_eq!(
            usb_devices(root.path()),
            vec![((0x04b8, 0x0121), PathBuf::from("/dev/bus/usb/001/011"))]
        );
    }

    #[test]
    fn hashes_files() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("abc");
        fs::write(&file, "abc").unwrap();
        assert_eq!(
            sha256(&file).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
