#!/usr/bin/env python3
"""appscan: CLI and GUI for the Epson Perfection 2480/2580 PHOTO on Linux.

A thin layer over SANE's `snapscan` backend (scanimage). The scanner needs Epson's
firmware uploaded at every power-on; the firmware and a private SANE config live in
$XDG_DATA_HOME/appscan, so nothing in /etc has to change.
"""
import argparse
import errno
import fcntl
import functools
import hashlib
import io
import os
import queue
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

from PIL import Image

__version__ = "0.1.0"
Image.MAX_IMAGE_PIXELS = None  # our own scans: a 2400 dpi page is far past Pillow's decompression-bomb limit

_xdg = os.environ.get("XDG_DATA_HOME", "")
DATA_DIR = (Path(_xdg) if os.path.isabs(_xdg) else Path.home() / ".local" / "share") / "appscan"
FIRMWARE = DATA_DIR / "esfw41.bin"  # same file for 2480 and 2580
CONF_DIR = DATA_DIR / "sane.d"
DRIVER_URL = "https://ftp.epson.com/drivers/epson12204.exe"  # Epson Scan 3.04A for Windows
FIRMWARE_SHA256 = "fe200d47179276e52e24a9bf826a567ea202ae800cd1dbcaaeba472e497ab5e9"

MODES = ["Color", "Gray", "Lineart"]
RESOLUTIONS = [50, 75, 100, 150, 200, 300, 400, 600, 1200, 2400]
SOURCES = {"Flatbed": (216, 297), "Transparency Adapter": (55, 125)}  # scan area in mm
FORMATS = {".png": "png", ".jpg": "jpeg", ".jpeg": "jpeg", ".tif": "tiff", ".tiff": "tiff",
           ".pnm": "pnm"}  # written by scanimage; .pdf is assembled by save_pdf()
BRIGHTNESS = range(-400, 401)  # percent, the backend's limits
CONTRAST = range(-100, 401)
WARMUP = dict(resolution=50, area=(0, 0, 5, 5))  # a tiny scan that just gets the lamp warm
WARMUP_HINT = "Starting the scanner (a cold lamp needs up to 40 s to warm up)…"
PROGRESS = re.compile(r"Progress: ([\d.]+)%")
USBDEVFS_RESET = 0x5514  # _IO('U', 20) from linux/usbdevice_fs.h


class ScanError(Exception):
    pass


def require(*tools):
    missing = [t for t in tools if not shutil.which(t)]
    if missing:
        raise ScanError(f"Missing tools: {' '.join(missing)} (apt install sane-utils p7zip-full cabextract)")


@functools.cache
def sane_env():
    """Environment pointing SANE at our snapscan.conf; the trailing ':' keeps /etc/sane.d as fallback."""
    require("scanimage")
    if ":" in str(DATA_DIR) or '"' in str(DATA_DIR):  # ':' separates SANE_CONFIG_DIR entries, '"' ends the conf string
        raise ScanError(f"appscan can't keep its data in a path containing ':' or '\"': {DATA_DIR}")
    if not FIRMWARE.exists():
        raise ScanError("Firmware missing: run 'appscan firmware' first")
    (CONF_DIR / "dll.d").mkdir(parents=True, exist_ok=True)  # empty, so /etc/sane.d/dll.d is skipped
    (CONF_DIR / "dll.conf").write_text("snapscan\n")  # only snapscan: no slow network-scanner probing
    (CONF_DIR / "snapscan.conf").write_text(f'firmware "{FIRMWARE}"\nusb 0x04b8 0x0121\n')
    return {**os.environ, "SANE_CONFIG_DIR": f"{CONF_DIR}:"}


def devices():
    # Look the device up every time: its libusb:BUS:DEV name changes on replug.
    out = subprocess.run(["scanimage", "-f", "%d%n"], env=sane_env(), capture_output=True,
                         text=True, timeout=60).stdout
    return [d for d in out.split() if d.startswith("snapscan:")]


def fetch_firmware():
    """Download Epson's Windows driver, extract esfw41.bin and verify it.

    A wrong firmware file hangs the scanner until power-cycled, hence the checksum.
    """
    require("curl", "7z", "cabextract")
    with tempfile.TemporaryDirectory() as tmp:
        exe = f"{tmp}/driver.exe"
        print(f"Downloading {DRIVER_URL} (~20 MB)")
        subprocess.run(["curl", "-fL", "--progress-bar", "-o", exe, DRIVER_URL], check=True)
        subprocess.run(["7z", "e", "-y", f"-o{tmp}", exe, "ModUsd.cab"], check=True, stdout=subprocess.DEVNULL)
        subprocess.run(["cabextract", "-q", "-F", "Esfw41.bin", "-d", tmp, f"{tmp}/ModUsd.cab"], check=True)
        data = Path(tmp, "Esfw41.bin").read_bytes()
    if hashlib.sha256(data).hexdigest() != FIRMWARE_SHA256:
        raise ScanError("Extracted firmware has an unexpected checksum; not installing it")
    FIRMWARE.parent.mkdir(parents=True, exist_ok=True)
    FIRMWARE.write_bytes(data)
    print(f"Firmware installed: {FIRMWARE}")


def scanimage_args(device, output, mode="Color", resolution=300, depth=8, source="Flatbed", area=None,
                   brightness=0, contrast=0):
    fmt = FORMATS.get(Path(output).suffix.lower())
    if not fmt:
        raise ScanError(f"Unsupported file type {Path(output).suffix!r}; use one of {' '.join(FORMATS)} .pdf")
    if depth == 16 and fmt not in ("png", "tiff", "pnm"):
        raise ScanError("16-bit scans need a .png, .tif or .pnm file")
    args = ["scanimage", "-d", device, "--mode", mode, "--resolution", str(resolution),
            "--format", fmt, "-o", str(output), "-p"]
    if source != "Flatbed":  # the option is inactive (and rejected) when no transparency unit is attached
        args += ["--source", source]
    if mode != "Lineart":  # inactive (and rejected) in Lineart, which thresholds instead
        args += ["--depth", str(depth)]
        if brightness:
            args += ["--brightness", str(brightness)]
        if contrast:
            args += ["--contrast", str(contrast)]
    if area:
        for flag, value in zip("ltxy", area):  # left, top, width, height in mm
            args += [f"-{flag}", str(value)]
    return args


def save_pdf(pages, output, resolution):
    """Combine scanned page images (same resolution) into one PDF sized to the scanned area."""
    images = [Image.open(p) for p in pages]  # ponytail: all pages in RAM (~1.7 GB for a 2400 dpi A4 page)
    # Lineart-only PDFs stay lossless 1-bit. Otherwise every page is a JPEG: Pillow uses one encoder
    # setting for all pages, and the 1-bit codec rejects `quality`.
    options = {}
    if any(im.mode != "1" for im in images):
        images = [im.convert("L") if im.mode == "1" else im for im in images]
        options["quality"] = 90
    part = f"{output}.part"  # an interrupted save must not destroy an existing file
    images[0].save(part, "PDF", save_all=True, append_images=images[1:], resolution=resolution, **options)
    os.replace(part, output)


def usb_reset():
    """Reset the scanner's USB port, like replugging it. Recovers a wedged scanner."""
    for product in Path("/sys/bus/usb/devices").glob("*/idProduct"):
        d = product.parent
        if (d / "idVendor").read_text().strip() == "04b8" and product.read_text().strip() == "0121":
            node = f"/dev/bus/usb/{int((d / 'busnum').read_text()):03}/{int((d / 'devnum').read_text()):03}"
            break
    else:
        raise ScanError("Scanner not found. Is it plugged in and switched on?")
    fd = os.open(node, os.O_WRONLY)
    try:
        fcntl.ioctl(fd, USBDEVFS_RESET)
    except OSError as e:
        if e.errno != errno.ENODEV:  # ENODEV: the scanner re-enumerated, i.e. the reset worked
            raise
    finally:
        os.close(fd)


def stop(proc):
    """Cancel a scan started by scan(). Call it once; it may block for ~10 s.

    A graceful cancel (SIGINT) only works once image data is flowing: earlier, during the
    firmware upload or warm-up, it leaves the scanner wedged. Even then scanimage's handler
    sometimes hangs inside the backend. In both cases: kill it and reset the scanner.
    """
    if proc.poll() is not None:
        return
    if proc.started:
        proc.send_signal(signal.SIGINT)
        try:
            proc.wait(timeout=10)
            return
        except subprocess.TimeoutExpired:
            pass
    proc.kill()
    proc.wait()
    usb_reset()


def scan(output, on_progress=None, on_start=None, **opts):
    """Scan to `output`, an image or a one-page PDF. on_progress(percent) and on_start(Popen)
    are called from this thread; pass the Popen to stop() to cancel."""
    if Path(output).suffix.lower() == ".pdf":
        with tempfile.TemporaryDirectory(prefix="appscan-") as tmp:
            scan(f"{tmp}/page.png", on_progress, on_start, **{**opts, "depth": 8})
            save_pdf([f"{tmp}/page.png"], output, opts.get("resolution", 300))
        return
    env = sane_env()
    # Two attempts: right after the power-on firmware upload SANE 1.1.1 often fails to
    # open the scanner with an I/O error although it is ready; the second open works.
    for _ in range(2):
        found = devices()
        if not found:  # plugged in (else usb_reset raises) but not answering: wedged
            usb_reset()
            time.sleep(3)  # ponytail: fixed wait for USB re-enumeration, poll if it proves flaky
            found = devices()
            if not found:
                raise ScanError("Scanner not responding. Switch it off and on again.")
        # Own session: a terminal Ctrl-C must reach only us, since a second signal makes
        # scanimage abort mid-scan, which wedges the scanner.
        proc = subprocess.Popen(scanimage_args(found[0], output, **opts), env=env, start_new_session=True,
                                stderr=subprocess.PIPE, text=True)  # text mode turns scanimage's \r into lines
        proc.started = False  # set once image data flows; stop() depends on it
        if on_start:
            on_start(proc)
        errors = []
        try:
            for line in proc.stderr:
                if m := PROGRESS.search(line):
                    proc.started = True
                    if on_progress:
                        on_progress(float(m.group(1)))
                elif line.strip():
                    errors.append(line.strip())
        except KeyboardInterrupt:  # only ever raised in the main thread, so signal.signal is allowed
            previous = signal.signal(signal.SIGINT, signal.SIG_IGN)  # a second Ctrl-C mustn't cut stop() short
            try:
                stop(proc)
            finally:
                signal.signal(signal.SIGINT, previous)
            raise
        if proc.wait() == 0:
            return
        message = "\n".join(errors)
        if not ("open of device" in message and "I/O" in message):
            break
    raise ScanError(message or f"scanimage exited with code {proc.returncode}")


def warmup():
    """Switch the lamp on and wait until it is warm, so the next scan starts right away."""
    with tempfile.TemporaryDirectory(prefix="appscan-") as tmp:
        scan(f"{tmp}/warmup.png", **WARMUP)


def canvas_to_mm(rect, canvas_size, bed_mm):
    """Selection (x0, y0, x1, y1) in canvas pixels -> (left, top, width, height) in mm; None if tiny."""
    x0, x1 = sorted(rect[0::2])
    y0, y1 = sorted(rect[1::2])
    if x1 - x0 < 5 or y1 - y0 < 5:
        return None
    sx, sy = bed_mm[0] / canvas_size[0], bed_mm[1] / canvas_size[1]
    return round(x0 * sx, 1), round(y0 * sy, 1), round((x1 - x0) * sx, 1), round((y1 - y0) * sy, 1)


def gui():
    try:
        import tkinter as tk
        from tkinter import filedialog, messagebox, ttk
    except ImportError as e:
        raise ScanError(f"the GUI needs Tk ({e}): apt install python3-tk, or use 'appscan scan'") from None

    class App:
        PREVIEW_DPI = 50
        CANVAS_H = 600

        def __init__(self, root):
            self.root, self.proc, self.cancelled, self.stopper = root, None, False, None
            self.events, self.sel, self.photo, self.scanning, self.busy = queue.Queue(), None, None, False, False
            self.lock, self.worker = threading.Lock(), None
            self.tmp = tempfile.TemporaryDirectory(prefix="appscan-")
            self.pages = []  # scanned PDF pages waiting to be saved
            self.vars = {"source": tk.StringVar(value="Flatbed"), "mode": tk.StringVar(value="Color"),
                         "resolution": tk.StringVar(value="300"), "depth": tk.StringVar(value="8")}
            self.brightness, self.contrast = tk.IntVar(value=0), tk.IntVar(value=0)
            self.status = tk.StringVar(value="Ready")
            self.pages_text = tk.StringVar()

            panel = ttk.Frame(root, padding=10)
            panel.pack(side="left", fill="y")
            choices = {"source": list(SOURCES), "mode": MODES, "resolution": RESOLUTIONS, "depth": [8, 16]}
            self.combos = {}
            for row, (name, values) in enumerate(choices.items()):
                ttk.Label(panel, text=name.capitalize()).grid(row=row, column=0, sticky="w", pady=2, padx=(0, 8))
                combo = ttk.Combobox(panel, textvariable=self.vars[name], values=values, state="readonly", width=20)
                combo.grid(row=row, column=1, pady=2)
                self.combos[name] = combo
            self.vars["source"].trace_add("write", lambda *_: self.reset_canvas())
            self.vars["mode"].trace_add("write", lambda *_: self.refresh())
            self.sliders = []
            for row, (name, var) in enumerate([("Brightness", self.brightness), ("Contrast", self.contrast)], start=4):
                ttk.Label(panel, text=name).grid(row=row, column=0, sticky="sw", pady=2)
                slider = tk.Scale(panel, variable=var, from_=-100, to=100, resolution=5, orient="horizontal",
                                  length=170, highlightthickness=0)
                slider.grid(row=row, column=1, sticky="ew")
                slider.bind("<Double-Button-1>", lambda _e, v=var: v.set(0))  # double-click resets
                self.sliders.append(slider)

            def button(text, command, row, pady=2):
                widget = ttk.Button(panel, text=text, command=command)
                widget.grid(row=row, column=0, columnspan=2, sticky="ew", pady=pady)
                return widget

            self.preview_button = button("Preview", self.preview, 6, pady=(10, 2))
            self.scan_button = button("Scan…", self.scan, 7)
            ttk.Label(panel, textvariable=self.pages_text).grid(row=8, column=0, columnspan=2, sticky="w",
                                                                pady=(12, 2))
            pdf = ttk.Frame(panel)
            pdf.grid(row=9, column=0, columnspan=2, sticky="ew")
            self.add_button = ttk.Button(pdf, text="Add page", command=self.add_page)
            self.save_button = ttk.Button(pdf, text="Save PDF…", command=self.save_pages)
            self.discard_button = ttk.Button(pdf, text="Discard", command=self.discard_pages)
            for column, widget in enumerate([self.add_button, self.save_button, self.discard_button]):
                widget.grid(row=0, column=column, sticky="ew", padx=(0 if column == 0 else 2, 0))
                pdf.columnconfigure(column, weight=1)
            self.cancel_button = button("Cancel", self.cancel, 10, pady=(12, 2))
            self.bar = ttk.Progressbar(panel, maximum=100)
            self.bar.grid(row=11, column=0, columnspan=2, sticky="ew", pady=(10, 2))
            ttk.Label(panel, textvariable=self.status, wraplength=260).grid(row=12, column=0, columnspan=2,
                                                                            sticky="w")
            ttk.Label(panel, text="Drag on the preview to scan only part of the bed. "
                                  "Double-click a slider to reset it.",
                      wraplength=260, foreground="gray").grid(row=13, column=0, columnspan=2, sticky="w", pady=(10, 0))

            self.canvas = tk.Canvas(root, background="#ccc", highlightthickness=0)
            self.canvas.pack(side="left", padx=(0, 10), pady=10)
            self.canvas.bind("<ButtonPress-1>", self.select_start)
            self.canvas.bind("<B1-Motion>", self.select_drag)
            self.reset_canvas()
            self.refresh()
            root.protocol("WM_DELETE_WINDOW", self.close)
            root.after(100, self.poll)
            root.after(200, self.warmup)

        def canvas_size(self):
            return int(self.canvas["width"]), int(self.canvas["height"])

        def reset_canvas(self):
            w_mm, h_mm = SOURCES[self.vars["source"].get()]
            self.canvas.configure(width=round(self.CANVAS_H * w_mm / h_mm), height=self.CANVAS_H)
            self.canvas.delete("all")
            self.sel = self.photo = None

        def select_start(self, e):
            self.sel = [e.x, e.y, e.x, e.y]
            self.canvas.delete("sel")
            self.canvas.create_rectangle(*self.sel, outline="red", width=2, dash=(4, 2), tags="sel")

        def select_drag(self, e):
            w, h = self.canvas_size()
            self.sel[2:] = min(max(e.x, 0), w), min(max(e.y, 0), h)
            self.canvas.coords("sel", *self.sel)

        def options(self, **overrides):
            source = self.vars["source"].get()
            area = self.sel and canvas_to_mm(self.sel, self.canvas_size(), SOURCES[source])
            opts = dict(source=source, mode=self.vars["mode"].get(), resolution=int(self.vars["resolution"].get()),
                        depth=int(self.vars["depth"].get()), area=area,
                        brightness=self.brightness.get(), contrast=self.contrast.get())
            return {**opts, **overrides}

        def warmup(self):
            self.start(f"{self.tmp.name}/warmup.png", lambda: self.status.set("Ready"), **WARMUP)
            self.status.set("Warming up the lamp…")

        def preview(self):
            path = f"{self.tmp.name}/preview.png"

            def show():
                w, h = self.canvas_size()
                with Image.open(path) as im:
                    png = io.BytesIO()
                    im.convert("RGB").resize((w, h)).save(png, "PNG")
                # Tk 8.6 reads PNG itself; PIL.ImageTk breaks on uv's standalone Python builds.
                self.photo = tk.PhotoImage(data=png.getvalue())
                self.canvas.delete("all")
                self.canvas.create_image(0, 0, anchor="nw", image=self.photo)
                self.sel = None
                self.status.set("Preview ready")

            self.start(path, show, **self.options(mode="Color", resolution=self.PREVIEW_DPI, depth=8, area=None))

        def scan(self):
            types = [("PNG", "*.png"), ("JPEG", "*.jpg"), ("TIFF", "*.tif"), ("PDF", "*.pdf")]
            path = filedialog.asksaveasfilename(defaultextension=".png", filetypes=types)
            if path:
                self.start(path, lambda: self.status.set(f"Saved {path}"), **self.options())

        def add_page(self):
            path = f"{self.tmp.name}/page{len(self.pages) + 1:03}.png"

            def added():
                self.pages.append(path)
                self.status.set(f"Page {len(self.pages)} added. Place the next sheet, or save the PDF.")

            self.start(path, added, **self.options(depth=8))

        def save_pages(self):
            path = filedialog.asksaveasfilename(defaultextension=".pdf", filetypes=[("PDF", "*.pdf")])
            if not path:
                return
            if Path(path).suffix.lower() != ".pdf":  # Tk only adds the extension when there is none ("v1.2")
                path += ".pdf"
            try:
                save_pdf(self.pages, path, int(self.vars["resolution"].get()))  # locked while pages exist
            except Exception as e:  # keep the pages whatever went wrong, so the user can retry
                messagebox.showerror("Saving the PDF failed", str(e))
                return
            self.status.set(f"Saved {len(self.pages)} pages to {path}")
            self.pages.clear()
            self.refresh()

        def discard_pages(self):
            self.pages.clear()
            self.status.set("Pages discarded")
            self.refresh()

        def start(self, output, on_done, **opts):
            if self.busy:  # one scan at a time: a second scanimage would collide on the USB bus
                return
            self.busy, self.cancelled, self.proc, self.stopper, self.scanning = True, False, None, None, False
            self.bar["value"] = 0
            self.status.set(WARMUP_HINT)
            self.refresh()

            def started(proc):
                with self.lock:  # cancel() takes the same lock, so exactly one of us calls stop()
                    self.proc, cancelled = proc, self.cancelled
                if cancelled:  # Cancel was pressed before scanimage was launched
                    stop(proc)

            def work():
                try:
                    scan(output, on_progress=lambda p: self.events.put(("progress", p)), on_start=started, **opts)
                    event = ("done", on_done)
                except Exception as e:  # anything: the GUI must never stay stuck as busy
                    event = ("error", str(e))
                if self.stopper:  # a cancel may still be resetting the scanner: stay busy until it's done
                    self.stopper.join()
                self.events.put(event)

            self.worker = threading.Thread(target=work, daemon=True)
            self.worker.start()

        def poll(self):  # Tk isn't thread-safe: the worker only talks to us through the queue
            self.root.after(100, self.poll)  # first, so an exception below can't stop the loop
            while not self.events.empty():
                kind, arg = self.events.get()
                if kind == "progress":
                    self.bar["value"] = arg
                    if not self.scanning:
                        self.scanning = True
                        self.status.set("Scanning…")
                    continue
                self.busy = False
                self.bar["value"] = 0
                try:
                    if self.cancelled:
                        self.status.set("Cancelled")
                    elif kind == "done":
                        arg()
                    else:
                        self.status.set("Scan failed")
                        messagebox.showerror("Scan failed", arg)
                finally:
                    self.refresh()

        def refresh(self):
            """Enable the widgets that make sense now; safe to call at any time."""
            busy = self.busy
            for widget in (self.preview_button, self.scan_button, self.add_button):
                widget["state"] = "disabled" if busy else "normal"
            for widget in self.sliders:  # Lineart ignores brightness and contrast
                widget["state"] = "disabled" if busy or self.vars["mode"].get() == "Lineart" else "normal"
            for widget in (self.save_button, self.discard_button):
                widget["state"] = "normal" if self.pages and not busy else "disabled"
            for name, widget in self.combos.items():
                # changing the source mid-scan would reset the canvas; PDF pages must share one resolution
                locked = busy or (name == "resolution" and self.pages)
                widget["state"] = "disabled" if locked else "readonly"
            self.cancel_button["state"] = "normal" if busy and not self.cancelled else "disabled"
            self.pages_text.set(f"Multi-page PDF: {len(self.pages)} page{'s' * (len(self.pages) != 1)}")

        def cancel(self):
            if not self.busy or self.cancelled or (self.proc and self.proc.poll() is not None):
                return  # idle, already cancelling, or scanning is over and the file is being written
            with self.lock:
                self.cancelled, proc = True, self.proc
            self.cancel_button["state"] = "disabled"
            self.status.set("Cancelling…")
            if proc:  # otherwise started() stops scanimage as soon as it is launched
                self.stopper = threading.Thread(target=stop, args=(proc,))  # can take ~10 s
                self.stopper.start()

        def close(self):
            if self.pages and not messagebox.askokcancel("Quit", "Discard the unsaved PDF pages?"):
                return
            signal.signal(signal.SIGINT, signal.SIG_IGN)  # we're closing: another Ctrl-C mustn't re-enter
            self.root.withdraw()
            self.cancel()  # don't leave scanimage running without us
            if self.busy:
                self.worker.join()  # waits for the cancel (and any reset), or for the file being written
            self.root.destroy()

    try:
        root = tk.Tk(className="appscan")  # the class matches StartupWMClass in appscan.desktop
    except tk.TclError as e:
        raise ScanError(f"can't open the GUI ({e}); use 'appscan scan' instead") from None
    root.title("appscan: Epson Perfection 2480/2580")
    app = App(root)
    signal.signal(signal.SIGINT, lambda *_: app.close())  # Ctrl-C in the terminal: same path as closing
    root.mainloop()


def percent(limits):
    def parse(text):
        value = int(text)
        if value not in limits:
            raise argparse.ArgumentTypeError(f"must be between {limits.start} and {limits.stop - 1}")
        return value
    return parse


def scan_pages(output, count, **opts):
    """CLI multi-page PDF: scan `count` pages, waiting for Enter between them.
    Stopping early (Ctrl-C or end of input) still saves the pages scanned so far, and so does
    a failing page (the error is raised afterwards)."""
    progress = lambda p: print(f"\rScanning {p:5.1f}%", end="", file=sys.stderr, flush=True)  # noqa: E731
    with tempfile.TemporaryDirectory(prefix="appscan-") as tmp:
        pages = []
        try:
            for n in range(1, count + 1):
                if n > 1:
                    print(f"\nPlace page {n} of {count} on the glass and press Enter… ", end="", file=sys.stderr,
                          flush=True)
                    if not sys.stdin.readline():
                        raise EOFError
                scan(f"{tmp}/page{n}.png", on_progress=progress, **{**opts, "depth": 8})
                pages.append(f"{tmp}/page{n}.png")
        except (KeyboardInterrupt, EOFError):
            if not pages:
                raise KeyboardInterrupt from None
            print(f"\nStopped early after {len(pages)} page(s)", file=sys.stderr)
        except Exception:
            if pages:
                save_pdf(pages, output, opts["resolution"])
                print(f"\nSaved the {len(pages)} page(s) scanned before the error to {output}", file=sys.stderr)
            raise
        save_pdf(pages, output, opts["resolution"])
    return len(pages)


def main(argv=None):
    parser = argparse.ArgumentParser(prog="appscan", description=__doc__.splitlines()[0])
    parser.add_argument("--version", action="version", version=f"%(prog)s {__version__}")
    sub = parser.add_subparsers(dest="cmd")
    sub.add_parser("gui", help="open the graphical interface (default)")
    s = sub.add_parser("scan", help="scan to a file (.png .jpg .tif .pnm .pdf)")
    s.add_argument("output")
    s.add_argument("--mode", choices=MODES, default="Color")
    s.add_argument("-r", "--resolution", type=int, choices=RESOLUTIONS, default=300, metavar="DPI",
                   help=f"one of {' '.join(map(str, RESOLUTIONS))} (default: 300)")
    s.add_argument("--depth", type=int, choices=[8, 16], default=8)
    s.add_argument("--source", choices=list(SOURCES), default="Flatbed")
    s.add_argument("--area", nargs=4, type=float, metavar=("LEFT", "TOP", "WIDTH", "HEIGHT"),
                   help="scan area in mm (default: the whole bed)")
    s.add_argument("--brightness", type=percent(BRIGHTNESS), default=0, metavar="PERCENT", help="-400..400")
    s.add_argument("--contrast", type=percent(CONTRAST), default=0, metavar="PERCENT", help="-100..400")
    s.add_argument("--pages", type=int, default=1, metavar="N",
                   help="scan N pages into one PDF, pressing Enter between pages")
    sub.add_parser("list", help="list detected scanners")
    sub.add_parser("warmup", help="warm up the lamp so the next scan starts right away")
    sub.add_parser("reset", help="reset the scanner's USB connection if it stops responding")
    sub.add_parser("firmware", help="download Epson's driver and extract the scanner firmware")
    a = parser.parse_args(argv)
    if a.cmd == "scan":
        if a.pages != 1 and (a.pages < 1 or Path(a.output).suffix.lower() != ".pdf"):
            parser.error("--pages needs a positive number and a .pdf output")
        folder = Path(a.output).absolute().parent  # check now, not after minutes of scanning
        if not os.access(folder, os.W_OK):
            parser.error(f"can't write to {folder}")
    try:
        if a.cmd == "scan":
            opts = dict(mode=a.mode, resolution=a.resolution, depth=a.depth, source=a.source, area=a.area,
                        brightness=a.brightness, contrast=a.contrast)
            print(WARMUP_HINT, file=sys.stderr)
            if a.pages > 1:
                saved = scan_pages(a.output, a.pages, **opts)
                print(f"\nSaved {saved} page(s) to {a.output}", file=sys.stderr)
            else:
                scan(a.output, on_progress=lambda p: print(f"\rScanning {p:5.1f}%", end="", file=sys.stderr,
                                                           flush=True), **opts)
                print(f"\nSaved {a.output}", file=sys.stderr)
        elif a.cmd == "list":
            print("\n".join(devices()) or "No scanner found")
        elif a.cmd == "warmup":
            print(WARMUP_HINT, file=sys.stderr)
            warmup()
            print("Scanner ready", file=sys.stderr)
        elif a.cmd == "reset":
            usb_reset()
            print("Scanner reset; its lamp needs to warm up again before the next scan")
        elif a.cmd == "firmware":
            fetch_firmware()
        else:
            gui()
    except (ScanError, subprocess.SubprocessError, OSError) as e:
        sys.exit(f"error: {e}")
    except KeyboardInterrupt:
        sys.exit("\nCancelled")


if __name__ == "__main__":
    main()
