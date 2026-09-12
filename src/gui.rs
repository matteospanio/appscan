//! The GTK4 + libadwaita interface. Scanner work runs on a thread, one task at a time (a second
//! scanimage would collide on the USB bus); results come back over a channel polled by a timer.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use adw::prelude::*;
use anyhow::Result;
use gtk::{gdk, glib};

use crate::sane::{self, Device, Kind, Opt, Settings};
use crate::scan::{self, Cancelled, Job, TempDir, WARMUP_HINT};
use crate::{APP_ID, pdf};

/// Offered when a scanner's resolution is a range rather than a list.
const COMMON_DPI: [u32; 10] = [50, 75, 100, 150, 200, 300, 400, 600, 1200, 2400];

pub fn run(device: Option<String>) -> ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(move |app| {
        // A second launch just shows the window (the application id makes it single-instance).
        if let Some(window) = app.active_window() {
            window.present();
            return;
        }
        match Gui::new(app, device.clone()) {
            // The handler keeps the Gui (and its temporary directory) alive until the app ends.
            Ok(gui) => drop(app.connect_shutdown(move |_| gui.state.borrow_mut().pages.clear())),
            Err(e) => eprintln!("error: {e:#}"),
        }
    });
    ExitCode::from(app.run_with_args::<&str>(&[]))
}

#[derive(Clone, Copy)]
enum Output {
    Preview,
    File,
    Page,
}

enum Task {
    Discover(bool),
    Options(Device, Settings),
    Warmup(Device, Vec<Opt>),
    Scan {
        device: Device,
        opts: Vec<Opt>,
        settings: Settings,
        output: PathBuf,
        kind: Output,
    },
    SavePdf(Vec<(PathBuf, u32)>, PathBuf),
}

enum Reply {
    Devices(Vec<Device>),
    Options(Device, Vec<Opt>),
    Warm,
    Scanned(Output, PathBuf, u32),
    Saved(PathBuf),
}

enum Event {
    Progress(f64),
    Done(Result<Reply>),
}

/// Runs on the worker thread.
fn work(task: Task, job: &Job, tx: &mpsc::Sender<Event>) -> Result<Reply> {
    let mut progress = |percent| {
        let _ = tx.send(Event::Progress(percent));
    };
    Ok(match task {
        Task::Discover(all) => Reply::Devices(sane::discover(all)?),
        Task::Options(device, settings) => {
            let opts = scan::options(job, &device, &settings)?;
            Reply::Options(device, opts)
        }
        Task::Warmup(device, opts) => {
            scan::warmup(job, &device, &opts)?;
            Reply::Warm
        }
        Task::Scan {
            device,
            opts,
            settings,
            output,
            kind,
        } => {
            let dpi = match kind {
                Output::Page => {
                    scan::scan_pdf_page(job, &device, &opts, &settings, &output, &mut progress)?
                }
                Output::Preview | Output::File => {
                    scan::scan_to(job, &device, &opts, &settings, &output, &mut progress)?
                }
            };
            Reply::Scanned(kind, output, dpi)
        }
        Task::SavePdf(pages, output) => {
            pdf::write(&pages, &output)?;
            Reply::Saved(output)
        }
    })
}

struct Ui {
    window: adw::ApplicationWindow,
    title: adw::WindowTitle,
    toasts: adw::ToastOverlay,
    device: adw::ComboRow,
    refresh: gtk::Button,
    source: adw::ComboRow,
    mode: adw::ComboRow,
    resolution: adw::ComboRow,
    depth: adw::ComboRow,
    brightness: (adw::ActionRow, gtk::Scale),
    contrast: (adw::ActionRow, gtk::Scale),
    pages: adw::PreferencesGroup,
    preview: gtk::Button,
    scan: gtk::Button,
    cancel: gtk::Button,
    add_page: gtk::Button,
    save_pdf: gtk::Button,
    discard: gtk::Button,
    progress: gtk::ProgressBar,
    status: gtk::Label,
    frame: gtk::AspectFrame,
    picture: gtk::Picture,
    area: gtk::DrawingArea,
}

#[derive(Default)]
struct State {
    devices: Vec<Device>,
    /// Options of the selected device for the current source and mode.
    opts: Vec<Opt>,
    cache: HashMap<(String, Option<String>, Option<String>), Vec<Opt>>,
    busy: bool,
    scanning: bool,
    cancelling: bool,
    closing: bool,
    discard_confirmed: bool,
    warmed_up: bool,
    job: Option<Arc<Job>>,
    pages: Vec<(PathBuf, u32)>,
    page_files: u32,
    /// Area to scan in mm (left, top, width, height), and the rectangle being dragged in pixels.
    selection: Option<[f64; 4]>,
    drag: Option<[f64; 4]>,
    chooser: Option<gtk::FileChooserNative>,
}

struct Gui {
    ui: Ui,
    state: RefCell<State>,
    /// Set while widgets are filled in code, so their change handlers stay quiet.
    updating: Cell<bool>,
    tmp: TempDir,
    tx: mpsc::Sender<Event>,
    rx: mpsc::Receiver<Event>,
}

impl Gui {
    fn new(app: &adw::Application, device: Option<String>) -> Result<Rc<Self>> {
        let (tx, rx) = mpsc::channel();
        let gui = Rc::new(Gui {
            ui: build(app),
            state: RefCell::default(),
            updating: Cell::new(false),
            tmp: TempDir::new()?,
            tx,
            rx,
        });
        gui.connect();
        gui.refresh();
        gui.ui.window.present();
        match device {
            Some(name) => gui.show_devices(vec![sane::named(&name, name.clone())]),
            None => gui.start(Task::Discover(false), "Looking for scanners…"),
        }
        Ok(gui)
    }

    fn connect(self: &Rc<Self>) {
        let ui = &self.ui;
        let weak = Rc::downgrade(self);
        let on = move |f: fn(&Rc<Gui>)| {
            let weak = weak.clone();
            move || {
                if let Some(gui) = weak.upgrade() {
                    f(&gui)
                }
            }
        };
        let click = |button: &gtk::Button, f: fn(&Rc<Gui>)| {
            let f = on(f);
            button.connect_clicked(move |_| f());
        };
        click(&ui.refresh, |gui| {
            gui.start(Task::Discover(true), "Looking for scanners…")
        });
        click(&ui.preview, Gui::preview);
        click(&ui.scan, Gui::scan_to_file);
        click(&ui.cancel, Gui::cancel);
        click(&ui.add_page, Gui::add_page);
        click(&ui.save_pdf, Gui::save_pdf);
        click(&ui.discard, Gui::discard_pages);

        let changed = |row: &adw::ComboRow, f: fn(&Rc<Gui>)| {
            let f = on(f);
            let updating = Rc::downgrade(self);
            row.connect_selected_notify(move |_| {
                if updating.upgrade().is_some_and(|gui| !gui.updating.get()) {
                    // Later, not inside the drop-down's own selection handling: refilling the
                    // row that is emitting (cached options apply at once) hangs GTK 4.6.
                    glib::idle_add_local_once(f.clone());
                }
            });
        };
        changed(&ui.device, Gui::device_changed);
        changed(&ui.source, |gui| {
            gui.clear_preview(); // another source has another scan area
            gui.options_changed();
        });
        changed(&ui.mode, Gui::options_changed);

        for (_, scale) in [&ui.brightness, &ui.contrast] {
            let reset = gtk::GestureClick::new();
            let scale_weak = scale.downgrade();
            reset.connect_pressed(move |_, presses, _, _| {
                if let (2, Some(scale)) = (presses, scale_weak.upgrade()) {
                    scale.set_value(0.0);
                }
            });
            scale.add_controller(reset);
        }

        let drag = gtk::GestureDrag::new();
        let begin = Rc::downgrade(self);
        drag.connect_drag_begin(move |_, x, y| {
            if let Some(gui) = begin.upgrade() {
                gui.state.borrow_mut().drag = Some([x, y, x, y]);
                gui.ui.area.queue_draw();
            }
        });
        let update = Rc::downgrade(self);
        drag.connect_drag_update(move |_, dx, dy| {
            if let Some(gui) = update.upgrade() {
                if let Some(rect) = gui.state.borrow_mut().drag.as_mut() {
                    rect[2] = rect[0] + dx;
                    rect[3] = rect[1] + dy;
                }
                gui.ui.area.queue_draw();
            }
        });
        let end = Rc::downgrade(self);
        drag.connect_drag_end(move |_, _, _| {
            if let Some(gui) = end.upgrade() {
                let size = (gui.ui.area.width() as f64, gui.ui.area.height() as f64);
                let mut state = gui.state.borrow_mut();
                let bed = sane::bed_size(&state.opts);
                state.selection = state
                    .drag
                    .take()
                    .and_then(|rect| selection_mm(rect, size, bed));
                drop(state);
                gui.ui.area.queue_draw();
            }
        });
        ui.area.add_controller(drag);

        let draw = Rc::downgrade(self);
        ui.area.set_draw_func(move |_, cr, width, height| {
            let Some(gui) = draw.upgrade() else { return };
            let state = gui.state.borrow();
            let size = (width as f64, height as f64);
            let rect = match (state.drag, state.selection) {
                (Some([x0, y0, x1, y1]), _) => {
                    [x0.min(x1), y0.min(y1), (x1 - x0).abs(), (y1 - y0).abs()]
                }
                (None, Some(mm)) => mm_to_px(mm, size, sane::bed_size(&state.opts)),
                (None, None) => return,
            };
            cr.rectangle(rect[0], rect[1], rect[2], rect[3]);
            cr.set_source_rgba(0.21, 0.52, 0.89, 0.2); // GNOME blue
            let _ = cr.fill_preserve();
            cr.set_source_rgb(0.21, 0.52, 0.89);
            cr.set_line_width(2.0);
            cr.set_dash(&[6.0, 4.0], 0.0);
            let _ = cr.stroke();
        });

        let close = Rc::downgrade(self);
        ui.window
            .connect_close_request(move |_| match close.upgrade() {
                Some(gui) => gui.close_request(),
                None => glib::Propagation::Proceed,
            });
        // Ctrl-C, termination or a closed terminal go through the same path as closing the window.
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            let window = ui.window.downgrade();
            glib::unix_signal_add_local(signal, move || {
                if let Some(window) = window.upgrade() {
                    window.close();
                }
                glib::ControlFlow::Continue
            });
        }

        let poll = Rc::downgrade(self);
        glib::timeout_add_local(Duration::from_millis(100), move || match poll.upgrade() {
            Some(gui) => {
                gui.poll();
                glib::ControlFlow::Continue
            }
            None => glib::ControlFlow::Break,
        });
    }

    fn start(self: &Rc<Self>, task: Task, status: &str) {
        let profile = match &task {
            Task::Options(device, _) | Task::Warmup(device, _) | Task::Scan { device, .. } => {
                device.profile
            }
            Task::Discover(_) | Task::SavePdf(..) => None,
        };
        let job = Arc::new(Job::new(profile));
        {
            let mut state = self.state.borrow_mut();
            if state.busy {
                return; // one task at a time
            }
            (state.busy, state.scanning, state.cancelling, state.job) =
                (true, false, false, Some(job.clone()));
        }
        self.ui.status.set_text(status);
        self.ui.progress.set_fraction(0.0);
        self.refresh();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = work(task, &job, &tx);
            let _ = tx.send(Event::Done(result));
        });
    }

    fn poll(self: &Rc<Self>) {
        while let Ok(event) = self.rx.try_recv() {
            match event {
                Event::Progress(percent) => {
                    self.ui.progress.set_fraction(percent / 100.0);
                    if !std::mem::replace(&mut self.state.borrow_mut().scanning, true) {
                        self.ui.status.set_text("Scanning…");
                    }
                }
                Event::Done(result) => self.finish(result),
            }
        }
        let state = self.state.borrow();
        if state.busy && !state.scanning {
            self.ui.progress.pulse(); // waiting for the scanner (warm-up, calibration)
        }
    }

    fn finish(self: &Rc<Self>, result: Result<Reply>) {
        let closing = {
            let mut state = self.state.borrow_mut();
            (state.busy, state.job) = (false, None);
            state.closing
        };
        if closing {
            self.ui.window.destroy();
            return;
        }
        self.ui.progress.set_fraction(0.0);
        self.ui.status.set_text("");
        match result {
            Ok(reply) => self.handle(reply),
            Err(e) if e.is::<Cancelled>() => self.toast("Cancelled"),
            Err(e) => self.error("Something went wrong", &format!("{e:#}")),
        }
        self.refresh();
    }

    fn handle(self: &Rc<Self>, reply: Reply) {
        match reply {
            Reply::Devices(devices) => self.show_devices(devices),
            Reply::Options(device, opts) => {
                let warm_up = {
                    let mut state = self.state.borrow_mut();
                    let key = |source, mode| (device.name.clone(), source, mode);
                    let current = |name| sane::find(&opts, name).and_then(|o| o.default.clone());
                    state
                        .cache
                        .insert(key(current("--source"), current("--mode")), opts.clone());
                    let warm_up = !state.warmed_up && device.profile.is_some_and(|p| p.warmup);
                    state.warmed_up |= warm_up;
                    warm_up
                };
                self.apply_options(opts.clone());
                if warm_up {
                    self.start(Task::Warmup(device, opts), "Warming up the lamp…");
                }
            }
            Reply::Warm => self.toast("Scanner ready"),
            Reply::Scanned(Output::Preview, path, _) => match gdk::Texture::from_filename(&path) {
                Ok(texture) => {
                    self.ui.picture.set_paintable(Some(&texture));
                    self.state.borrow_mut().selection = None;
                    self.ui.area.queue_draw();
                }
                Err(e) => self.error("Can't show the preview", &e.to_string()),
            },
            Reply::Scanned(Output::File, path, _) => {
                self.toast(&format!("Saved {}", file_name(&path)))
            }
            Reply::Scanned(Output::Page, path, dpi) => {
                let count = {
                    let mut state = self.state.borrow_mut();
                    state.pages.push((path, dpi));
                    state.pages.len()
                };
                self.toast(&format!("Page {count} added"));
            }
            Reply::Saved(path) => {
                let count = self.clear_pages();
                self.toast(&format!("Saved {count} pages to {}", file_name(&path)));
            }
        }
    }

    fn show_devices(self: &Rc<Self>, devices: Vec<Device>) {
        let labels: Vec<String> = devices
            .iter()
            .map(|d| format!("{} ({})", d.label, d.name))
            .collect();
        let empty = devices.is_empty();
        self.state.borrow_mut().devices = devices;
        self.updating.set(true);
        let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
        self.ui
            .device
            .set_model(Some(&gtk::StringList::new(&labels)));
        self.ui.device.set_selected(0);
        self.updating.set(false);
        if empty {
            self.ui.title.set_subtitle("No scanner found");
            self.ui
                .status
                .set_text("No scanner found. Connect one and press the refresh button.");
        } else {
            self.device_changed();
        }
    }

    fn current_device(&self) -> Option<Device> {
        self.state
            .borrow()
            .devices
            .get(self.ui.device.selected() as usize)
            .cloned()
    }

    fn device_changed(self: &Rc<Self>) {
        let Some(device) = self.current_device() else {
            return;
        };
        self.ui.title.set_subtitle(&device.label);
        self.clear_preview();
        self.load_options(device, Settings::default());
    }

    /// Source and mode decide which options exist and are active: re-read them.
    fn options_changed(self: &Rc<Self>) {
        let Some(device) = self.current_device() else {
            return;
        };
        let current = self.settings();
        self.load_options(
            device,
            Settings {
                source: current.source,
                mode: current.mode,
                ..Default::default()
            },
        );
    }

    fn load_options(self: &Rc<Self>, device: Device, settings: Settings) {
        let key = (
            device.name.clone(),
            settings.source.clone(),
            settings.mode.clone(),
        );
        let cached = self.state.borrow().cache.get(&key).cloned();
        match cached {
            Some(opts) => self.apply_options(opts),
            None => self.start(
                Task::Options(device, settings),
                "Reading the scanner's options…",
            ),
        }
    }

    fn apply_options(self: &Rc<Self>, opts: Vec<Opt>) {
        self.updating.set(true);
        fill(&self.ui.source, &opts, "--source", None);
        fill(&self.ui.mode, &opts, "--mode", None);
        fill(
            &self.ui.resolution,
            &opts,
            "--resolution",
            Some(&COMMON_DPI),
        );
        fill(&self.ui.depth, &opts, "--depth", None);
        for (row, name) in [
            (&self.ui.brightness, "--brightness"),
            (&self.ui.contrast, "--contrast"),
        ] {
            slider(row, &opts, name);
        }
        self.updating.set(false);
        let (width, height) = sane::bed_size(&opts);
        self.ui.frame.set_ratio((width / height) as f32);
        self.state.borrow_mut().opts = opts;
        self.ui.area.queue_draw();
        self.refresh();
    }

    fn settings(&self) -> Settings {
        let slider = |(row, scale): &(adw::ActionRow, gtk::Scale)| {
            row.is_visible().then(|| scale.value().round() as i32)
        };
        Settings {
            source: combo_value(&self.ui.source),
            mode: combo_value(&self.ui.mode),
            resolution: combo_value(&self.ui.resolution).and_then(|v| v.parse().ok()),
            depth: combo_value(&self.ui.depth).and_then(|v| v.parse().ok()),
            brightness: slider(&self.ui.brightness),
            contrast: slider(&self.ui.contrast),
            area: self.state.borrow().selection,
        }
    }

    /// The device and its options, if a scan can start.
    fn ready(&self) -> Option<(Device, Vec<Opt>)> {
        let opts = self.state.borrow().opts.clone();
        Some((self.current_device()?, opts)).filter(|(_, opts)| !opts.is_empty())
    }

    fn scan_status(device: &Device) -> &'static str {
        if device.profile.is_some_and(|p| p.warmup) {
            WARMUP_HINT
        } else {
            "Starting the scanner…"
        }
    }

    fn preview(self: &Rc<Self>) {
        let Some((device, opts)) = self.ready() else {
            return;
        };
        let settings = Settings {
            resolution: sane::low_resolution(&opts, 50),
            depth: sane::find(&opts, "--depth").map(|_| 8),
            area: None,
            ..self.settings()
        };
        let output = self.tmp.path().join("preview.png");
        let status = Self::scan_status(&device);
        self.start(
            Task::Scan {
                device,
                opts,
                settings,
                output,
                kind: Output::Preview,
            },
            status,
        );
    }

    fn scan_to_file(self: &Rc<Self>) {
        let filters = [
            ("PNG", "*.png"),
            ("JPEG", "*.jpg"),
            ("TIFF", "*.tif"),
            ("PDF", "*.pdf"),
        ];
        self.choose_file("Save the scan", "scan.png", &filters, |gui, path| {
            let Some((device, opts)) = gui.ready() else {
                return;
            };
            let output = with_extension(path, "png", false);
            let status = Self::scan_status(&device);
            gui.start(
                Task::Scan {
                    device,
                    opts,
                    settings: gui.settings(),
                    output,
                    kind: Output::File,
                },
                status,
            );
        });
    }

    fn add_page(self: &Rc<Self>) {
        let Some((device, opts)) = self.ready() else {
            return;
        };
        let output = {
            let mut state = self.state.borrow_mut();
            state.page_files += 1;
            self.tmp
                .path()
                .join(format!("page{:03}.pnm", state.page_files))
        };
        let status = Self::scan_status(&device);
        self.start(
            Task::Scan {
                device,
                opts,
                settings: self.settings(),
                output,
                kind: Output::Page,
            },
            status,
        );
    }

    fn save_pdf(self: &Rc<Self>) {
        self.choose_file(
            "Save the PDF",
            "document.pdf",
            &[("PDF", "*.pdf")],
            |gui, path| {
                let pages = gui.state.borrow().pages.clone();
                gui.start(
                    Task::SavePdf(pages, with_extension(path, "pdf", true)),
                    "Writing the PDF…",
                );
            },
        );
    }

    fn discard_pages(self: &Rc<Self>) {
        self.clear_pages();
        self.toast("Pages discarded");
        self.refresh();
    }

    fn clear_pages(&self) -> usize {
        let pages = std::mem::take(&mut self.state.borrow_mut().pages);
        for (path, _) in &pages {
            let _ = std::fs::remove_file(path);
        }
        pages.len()
    }

    fn clear_preview(&self) {
        self.ui.picture.set_paintable(None::<&gdk::Paintable>);
        self.state.borrow_mut().selection = None;
        self.ui.area.queue_draw();
    }

    fn cancel(self: &Rc<Self>) {
        let job = {
            let mut state = self.state.borrow_mut();
            if !state.busy || state.cancelling {
                return;
            }
            state.cancelling = true;
            state.job.clone()
        };
        self.ui.status.set_text("Cancelling…");
        self.refresh();
        if let Some(job) = job {
            thread::spawn(move || job.cancel()); // may take ~10 s and reset the scanner
        }
    }

    fn close_request(self: &Rc<Self>) -> glib::Propagation {
        let (busy, closing, unsaved) = {
            let state = self.state.borrow();
            (
                state.busy,
                state.closing,
                !state.pages.is_empty() && !state.discard_confirmed,
            )
        };
        if closing {
            return glib::Propagation::Stop; // already waiting for the task to stop
        }
        if unsaved {
            self.confirm_discard();
            return glib::Propagation::Stop;
        }
        if busy {
            // Don't leave scanimage running without us: hide, cancel, and close when it's done.
            self.state.borrow_mut().closing = true;
            self.ui.window.set_visible(false);
            self.cancel();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    }

    fn confirm_discard(self: &Rc<Self>) {
        let dialog = gtk::MessageDialog::builder()
            .transient_for(&self.ui.window)
            .modal(true)
            .message_type(gtk::MessageType::Question)
            .text("Discard the unsaved PDF pages?")
            .secondary_text("The pages you added haven't been saved to a PDF yet.")
            .build();
        dialog.add_button("_Cancel", gtk::ResponseType::Cancel);
        dialog.add_button("_Discard", gtk::ResponseType::Accept);
        if let Some(button) = dialog.widget_for_response(gtk::ResponseType::Accept) {
            button.add_css_class("destructive-action");
        }
        let weak = Rc::downgrade(self);
        dialog.connect_response(move |dialog, response| {
            dialog.destroy();
            if let (gtk::ResponseType::Accept, Some(gui)) = (response, weak.upgrade()) {
                gui.state.borrow_mut().discard_confirmed = true;
                gui.ui.window.close();
            }
        });
        dialog.show();
    }

    fn choose_file(
        self: &Rc<Self>,
        title: &str,
        name: &str,
        filters: &[(&str, &str)],
        then: fn(&Rc<Gui>, PathBuf),
    ) {
        let chooser = gtk::FileChooserNative::new(
            Some(title),
            Some(&self.ui.window),
            gtk::FileChooserAction::Save,
            Some("_Save"),
            Some("_Cancel"),
        );
        chooser.set_current_name(name);
        for (label, pattern) in filters {
            let filter = gtk::FileFilter::new();
            filter.set_name(Some(label));
            filter.add_pattern(pattern);
            chooser.add_filter(&filter);
        }
        let weak = Rc::downgrade(self);
        chooser.connect_response(move |chooser, response| {
            let Some(gui) = weak.upgrade() else { return };
            let path = chooser.file().and_then(|file| file.path());
            gui.state.borrow_mut().chooser = None;
            if let (gtk::ResponseType::Accept, Some(path)) = (response, path) {
                then(&gui, path);
            }
        });
        chooser.show();
        self.state.borrow_mut().chooser = Some(chooser); // a native dialog must be kept alive
    }

    fn toast(&self, text: &str) {
        self.ui.toasts.add_toast(adw::Toast::new(text));
    }

    fn error(&self, title: &str, detail: &str) {
        let dialog = gtk::MessageDialog::builder()
            .transient_for(&self.ui.window)
            .modal(true)
            .message_type(gtk::MessageType::Error)
            .buttons(gtk::ButtonsType::Close)
            .text(title)
            .secondary_text(detail)
            .build();
        dialog.connect_response(|dialog, _| dialog.destroy());
        dialog.show();
    }

    /// The only place that decides what can be used right now.
    fn refresh(&self) {
        let state = self.state.borrow();
        let idle = !state.busy;
        let ready = idle && !state.opts.is_empty();
        let active = |name| sane::find(&state.opts, name).is_some_and(|o| o.active);
        let ui = &self.ui;
        ui.device.set_sensitive(idle && !state.devices.is_empty());
        ui.refresh.set_sensitive(idle);
        for (row, name) in [
            (&ui.source, "--source"),
            (&ui.mode, "--mode"),
            (&ui.resolution, "--resolution"),
            (&ui.depth, "--depth"),
        ] {
            row.set_sensitive(ready && active(name));
        }
        ui.brightness
            .0
            .set_sensitive(ready && active("--brightness"));
        ui.contrast.0.set_sensitive(ready && active("--contrast"));
        for button in [&ui.preview, &ui.scan, &ui.add_page] {
            button.set_sensitive(ready);
        }
        for button in [&ui.save_pdf, &ui.discard] {
            button.set_sensitive(idle && !state.pages.is_empty());
        }
        ui.cancel.set_visible(state.busy);
        ui.cancel.set_sensitive(!state.cancelling);
        let pages = state.pages.len();
        ui.pages.set_description(Some(&match pages {
            0 => "Scan pages one by one, then save them as one PDF".to_owned(),
            1 => "1 page".to_owned(),
            n => format!("{n} pages"),
        }));
    }
}

fn build(app: &adw::Application) -> Ui {
    let title = adw::WindowTitle::new("appscan", "");
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&title));
    let preview = gtk::Button::with_label("Preview");
    let scan = gtk::Button::with_label("Scan…");
    scan.add_css_class("suggested-action");
    let cancel = gtk::Button::with_label("Cancel");
    cancel.add_css_class("destructive-action");
    header.pack_start(&preview);
    header.pack_end(&scan);
    header.pack_end(&cancel);

    let combo = |title: &str| adw::ComboRow::builder().title(title).build();
    let slider = |title: &str| {
        let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, -100.0, 100.0, 5.0);
        scale.set_value(0.0);
        scale.set_digits(0);
        scale.set_draw_value(true);
        scale.set_width_request(200); // fixed, so both sliders line up whatever their label
        let row = adw::ActionRow::builder().title(title).build();
        row.add_suffix(&scale);
        (row, scale)
    };

    let device = combo("Device");
    let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh.set_tooltip_text(Some("Look for scanners again"));
    refresh.add_css_class("flat");
    let scanner = adw::PreferencesGroup::builder().title("Scanner").build();
    scanner.set_header_suffix(Some(&refresh));
    scanner.add(&device);

    let (source, mode, resolution, depth) = (
        combo("Source"),
        combo("Mode"),
        combo("Resolution (dpi)"),
        combo("Depth (bits)"),
    );
    let (brightness, contrast) = (slider("Brightness"), slider("Contrast"));
    let settings = adw::PreferencesGroup::builder().title("Settings").build();
    for row in [&source, &mode, &resolution, &depth] {
        settings.add(row);
    }
    settings.add(&brightness.0);
    settings.add(&contrast.0);

    let add_page = gtk::Button::with_label("Add page");
    let save_pdf = gtk::Button::with_label("Save PDF…");
    let discard = gtk::Button::with_label("Discard");
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    buttons.set_homogeneous(true);
    for button in [&add_page, &save_pdf, &discard] {
        buttons.append(button);
    }
    let pages = adw::PreferencesGroup::builder()
        .title("Multi-page PDF")
        .build();
    pages.add(&buttons);

    let progress = gtk::ProgressBar::new();
    let status = gtk::Label::new(None);
    status.set_wrap(true);
    status.set_xalign(0.0);
    let hint = gtk::Label::new(Some(
        "Drag on the preview to scan only part of it. Double-click a slider to reset it.",
    ));
    hint.set_wrap(true);
    hint.set_xalign(0.0);
    hint.add_css_class("dim-label");

    let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 18);
    margins(&sidebar, 12);
    for widget in [
        scanner.upcast_ref::<gtk::Widget>(),
        settings.upcast_ref(),
        pages.upcast_ref(),
        progress.upcast_ref(),
        status.upcast_ref(),
        hint.upcast_ref(),
    ] {
        sidebar.append(widget);
    }
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .width_request(380)
        .child(&sidebar)
        .hexpand(false) // the sliders expand; the preview should get the extra width
        .build();

    let picture = gtk::Picture::new();
    picture.set_keep_aspect_ratio(false); // the frame already has the scan area's proportions
    let area = gtk::DrawingArea::new();
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&picture));
    overlay.add_overlay(&area);
    overlay.add_css_class("card");
    overlay.set_overflow(gtk::Overflow::Hidden);
    let frame = gtk::AspectFrame::new(0.5, 0.5, 216.0 / 297.0, false);
    frame.set_child(Some(&overlay));
    frame.set_hexpand(true);
    frame.set_vexpand(true);
    margins(&frame, 12);

    let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    body.append(&scroller);
    body.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    body.append(&frame);
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&body));
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&header);
    content.append(&toasts);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("appscan")
        .icon_name("appscan")
        .default_width(1100)
        .default_height(760)
        .content(&content)
        .build();

    Ui {
        window,
        title,
        toasts,
        device,
        refresh,
        source,
        mode,
        resolution,
        depth,
        brightness,
        contrast,
        pages,
        preview,
        scan,
        cancel,
        add_page,
        save_pdf,
        discard,
        progress,
        status,
        frame,
        picture,
        area,
    }
}

fn margins(widget: &impl IsA<gtk::Widget>, px: i32) {
    widget.set_margin_top(px);
    widget.set_margin_bottom(px);
    widget.set_margin_start(px);
    widget.set_margin_end(px);
}

/// Fill a drop-down from a scanner option, keeping the user's choice when it is still offered.
fn fill(row: &adw::ComboRow, opts: &[Opt], name: &str, range_choices: Option<&[u32]>) {
    let values: Vec<String> = match (sane::find(opts, name).map(|o| &o.kind), range_choices) {
        (Some(Kind::List(values)), _) => values.clone(),
        (Some(Kind::Range { min, max }), Some(choices)) => choices
            .iter()
            .filter(|v| (*min..=*max).contains(&(**v as f64)))
            .map(u32::to_string)
            .collect(),
        _ => vec![],
    };
    row.set_visible(!values.is_empty());
    if values.is_empty() {
        return;
    }
    let keep = combo_value(row).filter(|current| values.contains(current));
    let pick = keep.or_else(|| sane::find(opts, name)?.default.clone());
    let current: Option<Vec<String>> = row.model().and_downcast::<gtk::StringList>().map(|list| {
        (0..list.n_items())
            .filter_map(|i| list.string(i))
            .map(|s| s.to_string())
            .collect()
    });
    if current.as_ref() != Some(&values) {
        let labels: Vec<&str> = values.iter().map(String::as_str).collect();
        row.set_model(Some(&gtk::StringList::new(&labels)));
    }
    if let Some(index) = pick.and_then(|p| values.iter().position(|v| *v == p))
        && row.selected() != index as u32
    {
        row.set_selected(index as u32);
    }
}

/// Set a slider's range from a scanner option, limited to ±100 so the GUI stays usable.
fn slider((row, scale): &(adw::ActionRow, gtk::Scale), opts: &[Opt], name: &str) {
    let Some(Kind::Range { min, max }) = sane::find(opts, name).map(|o| &o.kind) else {
        row.set_visible(false);
        return;
    };
    let (low, high) = (min.max(-100.0), max.min(100.0));
    scale.set_range(low, high);
    scale.set_value(scale.value().clamp(low, high));
    row.set_visible(true);
}

fn combo_value(row: &adw::ComboRow) -> Option<String> {
    if !row.is_visible() {
        return None;
    }
    row.selected_item()
        .and_downcast::<gtk::StringObject>()
        .map(|item| item.string().to_string())
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// Dragged rectangle (x0, y0, x1, y1) in widget pixels -> scan area in mm (left, top, width,
/// height), clamped to the widget; None for a click or a tiny drag.
fn selection_mm(rect: [f64; 4], size: (f64, f64), bed: (f64, f64)) -> Option<[f64; 4]> {
    let span = |a: f64, b: f64, max: f64| (a.min(b).clamp(0.0, max), a.max(b).clamp(0.0, max));
    let ((x0, x1), (y0, y1)) = (
        span(rect[0], rect[2], size.0),
        span(rect[1], rect[3], size.1),
    );
    if x1 - x0 < 5.0 || y1 - y0 < 5.0 {
        return None;
    }
    let (sx, sy) = (bed.0 / size.0, bed.1 / size.1);
    // Round down to 0.1 mm so left + width never exceeds the scanner's range.
    let mm = |v: f64| (v * 10.0).floor() / 10.0;
    Some([
        mm(x0 * sx),
        mm(y0 * sy),
        mm((x1 - x0) * sx),
        mm((y1 - y0) * sy),
    ])
}

fn mm_to_px(mm: [f64; 4], size: (f64, f64), bed: (f64, f64)) -> [f64; 4] {
    let (sx, sy) = (size.0 / bed.0, size.1 / bed.1);
    [mm[0] * sx, mm[1] * sy, mm[2] * sx, mm[3] * sy]
}

/// Add `.ext` when the name has no extension (GTK's save dialog doesn't), or always unless it
/// already is `.ext` when `force` ("Report v1.2" -> "Report v1.2.pdf").
fn with_extension(path: PathBuf, ext: &str, force: bool) -> PathBuf {
    let current = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase);
    match current {
        Some(current) if current == ext || !force => path,
        _ => PathBuf::from(format!("{}.{ext}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_selection_to_mm() {
        // 432x594 px shows the 216x297 mm flatbed: 2 px per mm; drag direction doesn't matter
        assert_eq!(
            selection_mm([200.0, 100.0, 20.0, 40.0], (432.0, 594.0), (216.0, 297.0)),
            Some([10.0, 20.0, 90.0, 30.0])
        );
        assert_eq!(
            selection_mm([5.0, 5.0, 7.0, 90.0], (432.0, 594.0), (216.0, 297.0)),
            None
        );
        let clamped =
            selection_mm([-50.0, -50.0, 900.0, 900.0], (432.0, 594.0), (216.0, 297.0)).unwrap();
        assert_eq!(clamped, [0.0, 0.0, 216.0, 297.0]);
        assert_eq!(
            mm_to_px([10.0, 20.0, 90.0, 30.0], (432.0, 594.0), (216.0, 297.0)),
            [20.0, 40.0, 180.0, 60.0]
        );
    }

    #[test]
    fn adds_extensions() {
        let ext = |p: &str, e, force| with_extension(PathBuf::from(p), e, force);
        assert_eq!(ext("scan", "png", false), PathBuf::from("scan.png"));
        assert_eq!(ext("scan.tif", "png", false), PathBuf::from("scan.tif"));
        assert_eq!(
            ext("Report v1.2", "pdf", true),
            PathBuf::from("Report v1.2.pdf")
        );
        assert_eq!(ext("doc.PDF", "pdf", true), PathBuf::from("doc.PDF"));
    }
}
