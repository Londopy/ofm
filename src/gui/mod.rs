// the ofm window: the modem's state at a glance, its log, the calibration tone, and the
// settings, around the same station the command line runs

mod look;
mod modem;
mod settings;
mod status;

use crate::audio::{devices, Devices};
use crate::calibrate::{dbfs, step_gain, Calibrator, MAX_TX};
use crate::config::{self, Config, ProfileName, PttMethod};
use crate::launch::{open_ptt, profile};
use anyhow::Result;
use eframe::egui::{
    self, Color32, ComboBox, Frame, Grid, Margin, ProgressBar, RichText, ScrollArea, Slider,
    TextEdit, Ui,
};
use look::Palette;
use modem::{Modem, Started};
use settings::{Field, Form};
use status::{loudness, Link, Loudness, Status, HOT_ABOVE_DBFS, QUIET_BELOW_DBFS};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub fn run() -> Result<()> {
    #[cfg(windows)]
    release_console();
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("ofm")
        .with_inner_size([860.0, 640.0])
        .with_min_inner_size([560.0, 440.0]);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../../assets/ofm.png")) {
        viewport = viewport.with_icon(Arc::new(icon));
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "ofm",
        options,
        Box::new(|cc| Ok(Box::new(App::new(&cc.egui_ctx)))),
    )
    .map_err(|e| {
        let message = format!(
            "ofm could not open its window: {e}\n\n`ofm run` in a terminal starts the modem \
             without a window."
        );
        #[cfg(windows)]
        show_error(&message);
        anyhow::anyhow!(message)
    })
}

// started from a shortcut there is no terminal left to print to
#[cfg(windows)]
fn show_error(message: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let (text, title) = (wide(message), wide("ofm"));
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        )
    };
}

/// True when ofm was started on its own, from Explorer or a shortcut, rather than from a
/// terminal: the console it got is then its alone.
#[cfg(windows)]
pub fn launched_alone() -> bool {
    use windows_sys::Win32::System::Console::GetConsoleProcessList;
    let mut ids = [0u32; 2];
    unsafe { GetConsoleProcessList(ids.as_mut_ptr(), ids.len() as u32) == 1 }
}

// a console window opened just for us only gets in the way of the real one
#[cfg(windows)]
fn release_console() {
    use windows_sys::Win32::System::Console::{
        FreeConsole, SetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    if launched_alone() {
        unsafe {
            FreeConsole();
            // the std handle values outlive the console and are handed out again to the next
            // socket or file, which eprintln! would then write into
            for h in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
                SetStdHandle(h, std::ptr::null_mut());
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Station,
    Calibrate,
    Settings,
}

enum Phase {
    Starting,
    /// Carries the sound card problem when the modem runs without one.
    Running(Option<String>),
    Failed(String),
    /// Stopped while calibrating, which needs the sound card and PTT.
    Paused,
}

struct Calibrating {
    cal: Result<Calibrator, String>,
    note: Option<String>,
}

struct App {
    cfg: Config,
    tab: Tab,
    modem: Option<Modem>,
    phase: Phase,
    status: Status,
    since: Instant,
    calibrating: Option<Calibrating>,
    form: Form,
    problems: Vec<(Field, String)>,
    save_error: Option<String>,
    saved: Option<String>,
    // the transmit gain in the config file; Calibrate can run with another until it is saved
    saved_gain: f32,
    // the startup check for a newer release, then what it found
    update_check: Option<Receiver<String>>,
    update: Option<String>,
    devices: Option<Result<Devices, String>>,
    serial_ports: Vec<String>,
}

impl App {
    fn new(ctx: &egui::Context) -> App {
        look::apply(ctx);
        let (cfg, problem) = match Config::load() {
            Ok(cfg) => (cfg, None),
            Err(e) => (Config::default(), Some(format!("{e:#}"))),
        };
        let mut app = App {
            form: Form::from_config(&cfg),
            saved_gain: cfg.audio.tx_gain,
            update_check: None,
            update: None,
            cfg,
            tab: Tab::Station,
            modem: None,
            phase: Phase::Starting,
            status: Status::default(),
            since: Instant::now(),
            calibrating: None,
            problems: Vec::new(),
            save_error: None,
            saved: None,
            devices: None,
            serial_ports: Vec::new(),
        };
        if app.cfg.updater.check_on_start {
            let (tx, rx) = channel();
            std::thread::spawn(move || {
                if let Ok(Some(u)) = crate::updater::check() {
                    let _ = tx.send(format!(
                        "ofm {} is out: run `ofm update` in a terminal, or get it from {}",
                        u.version, u.page
                    ));
                }
            });
            app.update_check = Some(rx);
        }
        match problem {
            // don't run on defaults the operator never chose; let them fix the file or save
            Some(p) => {
                app.phase = Phase::Failed(format!(
                    "The settings file could not be read, so the modem was not started: {p}. \
                     Fix the file, or choose settings here and save them over it."
                ));
                app.tab = Tab::Settings;
            }
            None => app.start_modem(),
        }
        app
    }

    fn start_modem(&mut self) {
        // the old modem has to let go of the sound card and ports first
        self.modem = None;
        self.status = Status::default();
        self.since = Instant::now();
        self.phase = Phase::Starting;
        self.modem = Some(Modem::start(self.cfg.clone()));
    }

    fn poll(&mut self) {
        if let Some(m) = &self.modem {
            loop {
                match m.started.try_recv() {
                    Ok(Started::Running(problem)) => self.phase = Phase::Running(problem),
                    Ok(Started::Failed(e)) => self.phase = Phase::Failed(e),
                    Err(TryRecvError::Empty) => break,
                    // the thread ended before saying how starting went
                    Err(TryRecvError::Disconnected) => {
                        if matches!(self.phase, Phase::Starting) {
                            self.phase =
                                Phase::Failed("The modem stopped while it was starting.".into());
                        }
                        break;
                    }
                }
            }
            let secs = self.since.elapsed().as_secs();
            loop {
                match m.events.try_recv() {
                    Ok(ev) => self.status.apply(secs, ev),
                    Err(TryRecvError::Empty) => break,
                    // only the station holds the sender, so the modem is gone
                    Err(TryRecvError::Disconnected) => {
                        if matches!(self.phase, Phase::Running(_)) {
                            self.phase = Phase::Failed("The modem stopped unexpectedly.".into());
                        }
                        break;
                    }
                }
            }
        }
        if let Some(rx) = &self.update_check {
            if let Ok(text) = rx.try_recv() {
                self.update = Some(text);
                self.update_check = None;
            }
        }
        if let Some(c) = &mut self.calibrating {
            if let Ok(cal) = &mut c.cal {
                match cal.poll(self.cfg.audio.tx_gain) {
                    Ok(true) => {
                        c.note = Some(format!(
                            "The tone stopped after {} s to spare the transmitter.",
                            MAX_TX.as_secs()
                        ))
                    }
                    Ok(false) => {}
                    Err(e) => c.note = Some(format!("{e:#}")),
                }
            }
        }
    }

    fn start_calibrating(&mut self) {
        self.modem = None;
        self.phase = Phase::Paused;
        let cal = open_ptt(&self.cfg)
            .and_then(|ptt| Calibrator::open(&self.cfg, profile(&self.cfg), ptt))
            .map_err(|e| format!("{e:#}"));
        self.calibrating = Some(Calibrating { cal, note: None });
    }

    fn stop_calibrating(&mut self) {
        self.calibrating = None;
        self.start_modem();
    }

    fn set_tab(&mut self, tab: Tab) {
        if tab != Tab::Calibrate && self.calibrating.is_some() {
            self.stop_calibrating();
        }
        if tab == Tab::Settings && self.devices.is_none() {
            self.refresh_devices();
        }
        self.tab = tab;
    }

    fn refresh_devices(&mut self) {
        self.devices = Some(devices().map_err(|e| format!("{e:#}")));
        self.serial_ports = serialport::available_ports()
            .map(|ports| ports.into_iter().map(|p| p.port_name).collect())
            .unwrap_or_default();
    }

    fn save_settings(&mut self) {
        self.saved = None;
        self.save_error = None;
        // a gain tried in Calibrate but not saved there stays out of the file
        let mut base = self.cfg.clone();
        base.audio.tx_gain = self.saved_gain;
        match self.form.to_config(&base) {
            Err(problems) => self.problems = problems,
            Ok(mut cfg) => {
                self.problems.clear();
                if let Err(e) = cfg.save() {
                    self.save_error = Some(format!("{e:#}"));
                    return;
                }
                cfg.audio.tx_gain = self.cfg.audio.tx_gain;
                self.cfg = cfg;
                self.form = Form::from_config(&self.cfg);
                self.calibrating = None;
                self.start_modem();
                self.saved = Some("Saved. The modem restarted with these settings.".into());
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        // ctrl-c or closing the terminal: close the window the normal way so the modem and
        // the calibration tone are dropped and PTT is released
        if crate::stop::requested() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
        self.poll();
        ui.ctx().request_repaint_after(Duration::from_millis(100));
        let pal = look::palette(ui.visuals().dark_mode);
        egui::Panel::top("header").show(ui, |ui| self.header(ui, pal));
        egui::Panel::bottom("footer").show(ui, |ui| self.footer(ui, pal));
        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::Station => self.station(ui, pal),
            Tab::Calibrate => {
                ScrollArea::vertical().show(ui, |ui| self.calibrate(ui, pal));
            }
            Tab::Settings => {
                ScrollArea::vertical().show(ui, |ui| self.settings(ui, pal));
            }
        });
    }
}

fn tile(ui: &mut Ui, pal: Palette, label: &str, value: RichText) {
    Frame::new()
        .fill(pal.tile)
        .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
        .corner_radius(3)
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_min_width(130.0);
            ui.vertical(|ui| {
                ui.label(RichText::new(label).small().color(pal.muted));
                ui.label(value.monospace().size(18.0));
            });
        });
}

fn banner(ui: &mut Ui, color: Color32, add: impl FnOnce(&mut Ui)) {
    Frame::new()
        .stroke(egui::Stroke::new(1.5, color))
        .corner_radius(3)
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        });
}

fn level_meter(ui: &mut Ui, pal: Palette, db: f32) {
    let (word, color) = match loudness(db) {
        Loudness::Quiet => ("too quiet, or nothing on the channel", pal.muted),
        Loudness::Good => ("good", pal.good),
        Loudness::Hot => ("too hot, turn the radio's audio down", pal.bad),
    };
    let fraction = |db: f32| ((db + 60.0) / 60.0).clamp(0.0, 1.0);
    let label = ui.label(RichText::new(format!("Receive level {db:.0} dBFS, {word}")).color(color));
    let bar = ui
        .add(
            ProgressBar::new(fraction(db))
                .fill(color)
                .desired_width(ui.available_width().min(520.0))
                .desired_height(12.0)
                .corner_radius(3),
        )
        .labelled_by(label.id);
    // the range to aim for, so the level can be set without reading numbers
    let r = bar.rect;
    let x = |db: f32| r.left() + r.width() * fraction(db);
    let band = egui::Rect::from_x_y_ranges(x(QUIET_BELOW_DBFS)..=x(HOT_ABOVE_DBFS), r.y_range());
    let painter = ui.painter();
    painter.rect_stroke(
        band,
        2,
        egui::Stroke::new(1.5, pal.good),
        egui::StrokeKind::Outside,
    );
    let font = egui::FontId::proportional(11.0);
    for (db, text) in [
        (QUIET_BELOW_DBFS, format!("{QUIET_BELOW_DBFS:.0}")),
        (HOT_ABOVE_DBFS, format!("{HOT_ABOVE_DBFS:.0} dBFS")),
    ] {
        painter.text(
            egui::pos2(x(db), r.bottom() + 3.0),
            egui::Align2::CENTER_TOP,
            text,
            font.clone(),
            pal.muted,
        );
    }
    ui.add_space(16.0);
}

fn section(ui: &mut Ui, title: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(title).size(16.5).strong());
    ui.add_space(2.0);
}

// a field's problems as an extra grid row under it, where the eye already is
fn problem_rows(ui: &mut Ui, pal: Palette, problems: &[(Field, String)], field: Field) {
    for (_, text) in problems.iter().filter(|(f, _)| *f == field) {
        ui.label("");
        ui.label(RichText::new(text).color(pal.bad));
        ui.end_row();
    }
}

fn ptt_name(m: PttMethod) -> &'static str {
    match m {
        PttMethod::Client => "the client (Pat with rigctld)",
        PttMethod::Vox => "the radio's VOX",
        PttMethod::Rts => "serial RTS",
        PttMethod::Dtr => "serial DTR",
        PttMethod::Cm108 => "CM108 GPIO",
        PttMethod::Rigctld => "rigctld",
    }
}

impl App {
    fn header(&mut self, ui: &mut Ui, pal: Palette) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("ofm").heading().strong().color(pal.accent));
            ui.add_space(12.0);
            for (tab, name) in [
                (Tab::Station, "Station"),
                (Tab::Calibrate, "Calibrate"),
                (Tab::Settings, "Settings"),
            ] {
                if ui.selectable_label(self.tab == tab, name).clicked() {
                    self.set_tab(tab);
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let keyed = self.status.ptt
                    || self
                        .calibrating
                        .as_ref()
                        .is_some_and(|c| c.cal.as_ref().is_ok_and(|c| c.keyed()));
                if keyed {
                    ui.label(
                        RichText::new(" TX ")
                            .strong()
                            .color(pal.tile)
                            .background_color(pal.accent),
                    );
                }
                let (text, color) = match (&self.phase, &self.status.link) {
                    (Phase::Starting, _) => ("Starting".to_string(), pal.muted),
                    (Phase::Failed(_), _) => ("Not running".to_string(), pal.bad),
                    (Phase::Paused, _) => ("Paused for calibration".to_string(), pal.warn),
                    (_, Link::Connected { .. }) => (
                        format!("Connected to {}", self.status.peer().unwrap_or("?")),
                        pal.good,
                    ),
                    (_, Link::Pending) => ("Connecting".to_string(), pal.warn),
                    (_, Link::Idle) => ("Idle".to_string(), pal.muted),
                };
                ui.label(RichText::new(text).strong().color(color));
            });
        });
        ui.add_space(4.0);
    }

    fn footer(&self, ui: &mut Ui, pal: Palette) {
        ui.add_space(3.0);
        ui.horizontal_wrapped(|ui| {
            let c = &self.cfg;
            let text = format!(
                "Pat: 127.0.0.1:{} (data {})   KISS: {}   {}   PTT: {}   ofm {}",
                c.host.port,
                c.host.port.saturating_add(1),
                if c.kiss.enabled {
                    c.kiss.port.to_string()
                } else {
                    "off".into()
                },
                profile(c).name(),
                ptt_name(c.ptt.method),
                env!("CARGO_PKG_VERSION"),
            );
            ui.label(RichText::new(text).small().color(pal.muted));
            if self.cfg.audio.tx_gain != self.saved_gain {
                ui.label(
                    RichText::new(format!(
                        "TX gain {:+.1} dB, not saved",
                        dbfs(self.cfg.audio.tx_gain)
                    ))
                    .small()
                    .color(pal.warn),
                );
            }
            if let Some(u) = &self.update {
                ui.label(RichText::new(u).small().color(pal.accent));
            }
        });
        ui.add_space(2.0);
    }

    fn station(&mut self, ui: &mut Ui, pal: Palette) {
        ui.add_space(8.0);
        match &self.phase {
            Phase::Starting => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Starting the modem: opening the sound card, PTT and ports...");
                });
                return;
            }
            Phase::Failed(e) => {
                let e = e.clone();
                banner(ui, pal.bad, |ui| {
                    ui.label(
                        RichText::new("The modem is not running")
                            .strong()
                            .color(pal.bad),
                    );
                    ui.label(e);
                    ui.horizontal(|ui| {
                        if ui.button("Try again").clicked() {
                            self.start_modem();
                        }
                        if ui.button("Open settings").clicked() {
                            self.set_tab(Tab::Settings);
                        }
                    });
                });
                return;
            }
            Phase::Paused => {
                banner(ui, pal.warn, |ui| {
                    ui.label("The modem is paused while the Calibrate tab has the sound card.");
                    if ui.button("Resume the modem").clicked() {
                        self.stop_calibrating();
                    }
                });
                return;
            }
            Phase::Running(Some(problem)) => {
                let problem = problem.clone();
                banner(ui, pal.warn, |ui| {
                    ui.label(RichText::new("No sound card").strong().color(pal.warn));
                    ui.label(format!(
                        "{problem}. Pat can still connect, but nothing goes on the air."
                    ));
                    ui.horizontal(|ui| {
                        if ui.button("Try again").clicked() {
                            self.start_modem();
                        }
                        if ui.button("Choose devices").clicked() {
                            self.set_tab(Tab::Settings);
                        }
                    });
                });
                ui.add_space(8.0);
            }
            Phase::Running(None) => {}
        }

        let s = &self.status;
        let (title, detail) = match &s.link {
            Link::Idle => (
                "Waiting for a connection".to_string(),
                match &s.mycall {
                    Some(call) => format!(
                        "Pat is attached as {call}. Connect from Pat, or wait for a station to call."
                    ),
                    None => format!(
                        "No client yet. Set Pat's varafm address to 127.0.0.1:{}.",
                        self.cfg.host.port
                    ),
                },
            ),
            Link::Pending => (
                "Connecting".to_string(),
                "Setting up a link; this takes a few seconds.".to_string(),
            ),
            Link::Connected { src, profile, .. } => {
                let we_called = s.mycall.as_deref().is_some_and(|m| m.eq_ignore_ascii_case(src));
                (
                    format!("Connected to {}", s.peer().unwrap_or("?")),
                    format!(
                        "{} on the {} profile.",
                        if we_called { "We called" } else { "They called" },
                        profile.to_lowercase()
                    ),
                )
            }
        };
        ui.label(RichText::new(title).heading());
        ui.label(RichText::new(detail).color(pal.muted));
        ui.add_space(10.0);

        let dash = || RichText::new("-");
        ui.horizontal_wrapped(|ui| {
            tile(
                ui,
                pal,
                "Speed",
                s.level
                    .map_or_else(dash, |(l, bps)| RichText::new(format!("L{l} {bps} bps"))),
            );
            tile(
                ui,
                pal,
                "SNR",
                s.snr
                    .map_or_else(dash, |db| RichText::new(format!("{db:.1} dB"))),
            );
            tile(
                ui,
                pal,
                "Waiting for ack",
                RichText::new(format!("{} bytes", s.buffer)),
            );
            tile(
                ui,
                pal,
                "Channel",
                if s.busy {
                    RichText::new("Busy").color(pal.warn)
                } else {
                    RichText::new("Clear").color(pal.good)
                },
            );
            tile(
                ui,
                pal,
                "Radio",
                if s.ptt {
                    RichText::new("Transmit").color(pal.accent)
                } else {
                    RichText::new("Receive")
                },
            );
        });
        ui.add_space(10.0);
        level_meter(ui, pal, s.rx_dbfs());
        ui.add_space(12.0);

        ui.horizontal(|ui| {
            ui.label(RichText::new("Log").strong());
            if !s.log.is_empty() && ui.button("Copy").clicked() {
                let text: String = s
                    .log
                    .iter()
                    .map(|(t, l)| format!("{:02}:{:02} {l}\n", t / 60, t % 60))
                    .collect();
                ui.ctx().copy_text(text);
            }
        });
        Frame::new()
            .fill(ui.visuals().extreme_bg_color)
            .corner_radius(3)
            .inner_margin(Margin::same(8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.set_min_height((ui.available_height() - 8.0).max(60.0));
                if s.log.is_empty() {
                    ui.label(
                        RichText::new(
                            "Nothing yet. Connects, disconnects and speed changes show up here.",
                        )
                        .color(pal.muted),
                    );
                    return;
                }
                ScrollArea::vertical()
                    .id_salt("log")
                    .max_height(ui.available_height().max(120.0))
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for (t, line) in &s.log {
                            ui.label(
                                RichText::new(format!("{:02}:{:02}  {line}", t / 60, t % 60))
                                    .monospace(),
                            );
                        }
                    });
            });
    }

    fn calibrate(&mut self, ui: &mut Ui, pal: Palette) {
        ui.add_space(8.0);
        ui.label(RichText::new("Calibrate").heading());
        ui.label(
            "Key a steady tone at the level ofm's loudest frames reach, and set the radio's \
             deviation with it: about 3 kHz on Narrow, 3.5 kHz on Wide. Over-deviation is the \
             most common reason an FM data link fails. The meter shows what the radio hears, \
             for setting the receive level with the other station transmitting.",
        );
        ui.add_space(8.0);
        let Some(c) = &mut self.calibrating else {
            if let Some(peer) = self.status.peer() {
                ui.label(
                    RichText::new(format!(
                        "Calibrating stops the modem and drops the link with {peer}."
                    ))
                    .color(pal.warn),
                );
            }
            if ui.button("Pause the modem and calibrate").clicked() {
                self.start_calibrating();
            }
            return;
        };
        let mut resume = false;
        match &mut c.cal {
            Err(e) => {
                let e = e.clone();
                banner(ui, pal.bad, |ui| {
                    ui.label(
                        RichText::new("Could not open the sound card or PTT")
                            .strong()
                            .color(pal.bad),
                    );
                    ui.label(e);
                    ui.horizontal(|ui| {
                        if ui.button("Try again").clicked() {
                            self.calibrating = None;
                            self.start_calibrating();
                        }
                        if ui.button("Resume the modem").clicked() {
                            resume = true;
                        }
                    });
                });
            }
            Ok(cal) => {
                let keyed = cal.keyed();
                ui.horizontal(|ui| {
                    let label = if keyed {
                        "Stop the tone"
                    } else {
                        "Transmit the tone"
                    };
                    if ui.button(label).clicked() {
                        if let Err(e) = cal.set_tone(!keyed) {
                            c.note = Some(format!("{e:#}"));
                        }
                    }
                    if keyed {
                        ui.label(
                            RichText::new(format!(
                                "Transmitting. The tone stops by itself after {} s.",
                                MAX_TX.as_secs()
                            ))
                            .color(pal.accent),
                        );
                    }
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("Transmit gain");
                    if ui.button("-1 dB").clicked() {
                        self.cfg.audio.tx_gain = step_gain(self.cfg.audio.tx_gain, -1);
                    }
                    ui.label(
                        RichText::new(format!("{:+.1} dB", dbfs(self.cfg.audio.tx_gain)))
                            .monospace(),
                    );
                    if ui.button("+1 dB").clicked() {
                        self.cfg.audio.tx_gain = step_gain(self.cfg.audio.tx_gain, 1);
                    }
                    let unsaved = self.cfg.audio.tx_gain != self.saved_gain;
                    if ui
                        .add_enabled(unsaved, egui::Button::new("Save gain"))
                        .clicked()
                    {
                        let gain = self.cfg.audio.tx_gain;
                        c.note = Some(
                            match Config::load().and_then(|mut saved| {
                                saved.audio.tx_gain = gain;
                                saved.save()
                            }) {
                                Ok(()) => {
                                    self.saved_gain = gain;
                                    "Saved. ofm starts with this gain from now on.".into()
                                }
                                Err(e) => format!("{e:#}"),
                            },
                        );
                    }
                });
                if self.cfg.audio.tx_gain != self.saved_gain {
                    ui.label(
                        RichText::new(
                            "Not saved: the modem uses this gain until ofm closes, then goes back \
                             to the saved one.",
                        )
                        .color(pal.warn),
                    );
                }
                ui.add_space(6.0);
                level_meter(ui, pal, cal.level_db());
                ui.add_space(8.0);
                if ui.button("Done, resume the modem").clicked() {
                    resume = true;
                }
            }
        }
        if let Some(c) = &self.calibrating {
            if let Some(note) = &c.note {
                ui.label(note);
            }
        }
        if resume {
            self.stop_calibrating();
        }
    }

    fn settings(&mut self, ui: &mut Ui, pal: Palette) {
        ui.add_space(8.0);
        let f = &mut self.form;
        let field_width = 260.0;

        section(ui, "Sound card");
        ui.horizontal(|ui| match &self.devices {
            None => {
                ui.spinner();
            }
            Some(Err(e)) => {
                ui.label(RichText::new(format!("Could not list sound cards: {e}")).color(pal.bad));
            }
            Some(Ok(d)) => {
                Grid::new("devices")
                    .num_columns(2)
                    .spacing([12.0, 6.0])
                    .show(ui, |ui| {
                        for (label, chosen, names, default) in [
                            (
                                "Input (from the radio)",
                                &mut f.input,
                                &d.inputs,
                                &d.default_input,
                            ),
                            (
                                "Output (to the radio)",
                                &mut f.output,
                                &d.outputs,
                                &d.default_output,
                            ),
                        ] {
                            let label_id = ui.label(label).id;
                            let shown = chosen.clone().unwrap_or_else(|| {
                                format!("System default ({})", default.as_deref().unwrap_or("none"))
                            });
                            ComboBox::from_id_salt(label)
                                .selected_text(shown)
                                .width(field_width + 80.0)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(chosen, None, "System default");
                                    for n in names {
                                        ui.selectable_value(chosen, Some(n.clone()), n);
                                    }
                                })
                                .response
                                .labelled_by(label_id);
                            ui.end_row();
                        }
                    });
            }
        });
        if ui.button("Look for devices again").clicked() {
            self.refresh_devices();
        }
        let f = &mut self.form;
        ui.add_space(10.0);

        section(ui, "Radio");
        Grid::new("radio")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.label("Profile");
                ui.vertical(|ui| {
                    ui.radio_value(
                        &mut f.profile,
                        ProfileName::Narrow,
                        "Narrow: mic and speaker, or the 1200-baud data jack",
                    );
                    ui.radio_value(
                        &mut f.profile,
                        ProfileName::Wide,
                        "Wide: the flat 9600-baud discriminator port",
                    );
                });
                ui.end_row();
                let label = ui.label("Keyup delay (TXDELAY)");
                ui.add(Slider::new(&mut f.txdelay_ms, 50..=500).suffix(" ms"))
                    .labelled_by(label.id);
                ui.end_row();
                ui.label("CW ID");
                ui.checkbox(&mut f.cwid, "Send my call in CW at the end of a link");
                ui.end_row();
            });
        ui.add_space(10.0);

        section(ui, "Keying the radio (PTT)");
        Grid::new("ptt")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                let label = ui.label("Keyed by");
                ComboBox::from_id_salt("ptt-method")
                    .selected_text(ptt_name(f.ptt))
                    .width(field_width)
                    .show_ui(ui, |ui| {
                        for m in [
                            PttMethod::Client,
                            PttMethod::Vox,
                            PttMethod::Rts,
                            PttMethod::Dtr,
                            PttMethod::Cm108,
                            PttMethod::Rigctld,
                        ] {
                            ui.selectable_value(&mut f.ptt, m, ptt_name(m));
                        }
                    })
                    .response
                    .labelled_by(label.id);
                ui.end_row();
                match f.ptt {
                    PttMethod::Rts | PttMethod::Dtr => {
                        let label = ui.label("Serial port");
                        ui.horizontal(|ui| {
                            ui.add(TextEdit::singleline(&mut f.serial_port).desired_width(120.0))
                                .labelled_by(label.id);
                            for p in &self.serial_ports {
                                if ui.small_button(p).clicked() {
                                    f.serial_port.clone_from(p);
                                }
                            }
                        });
                        ui.end_row();
                        problem_rows(ui, pal, &self.problems, Field::SerialPort);
                    }
                    PttMethod::Cm108 => {
                        let label = ui.label("GPIO pin");
                        ui.add(Slider::new(&mut f.cm108_gpio, 1..=8))
                            .labelled_by(label.id);
                        ui.end_row();
                        let label = ui.label("HID path");
                        ui.add(
                            TextEdit::singleline(&mut f.cm108_path)
                                .hint_text("empty: the first CM108 or AIOC found")
                                .desired_width(field_width),
                        )
                        .labelled_by(label.id);
                        ui.end_row();
                    }
                    PttMethod::Rigctld => {
                        let label = ui.label("rigctld address");
                        ui.add(TextEdit::singleline(&mut f.rigctld).desired_width(field_width))
                            .labelled_by(label.id);
                        ui.end_row();
                        problem_rows(ui, pal, &self.problems, Field::Rigctld);
                    }
                    PttMethod::Client | PttMethod::Vox => {}
                }
            });
        ui.add_space(10.0);

        section(ui, "Ports");
        Grid::new("ports")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                let label = ui.label("Command port for Pat");
                ui.horizontal(|ui| {
                    ui.add(TextEdit::singleline(&mut f.host_port).desired_width(80.0))
                        .labelled_by(label.id);
                    ui.label(RichText::new("the data port is one up").color(pal.muted));
                });
                ui.end_row();
                problem_rows(ui, pal, &self.problems, Field::HostPort);
                let label = ui.label("KISS port");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut f.kiss_enabled, "On, at port");
                    ui.add_enabled(
                        f.kiss_enabled,
                        TextEdit::singleline(&mut f.kiss_port).desired_width(80.0),
                    )
                    .labelled_by(label.id);
                });
                ui.end_row();
                problem_rows(ui, pal, &self.problems, Field::KissPort);
                let label = ui.label("KISS speed level");
                ui.add_enabled(f.kiss_enabled, Slider::new(&mut f.kiss_level, 0..=9))
                    .labelled_by(label.id);
                ui.end_row();
            });
        ui.add_space(10.0);

        section(ui, "Updates");
        ui.checkbox(
            &mut f.check_updates,
            "Check GitHub for a newer ofm when the modem starts",
        );
        ui.add_space(14.0);

        let changed = self.form != Form::from_config(&self.cfg);
        if changed && self.status.peer().is_some() {
            ui.label(
                RichText::new("Saving restarts the modem and drops the current link.")
                    .color(pal.warn),
            );
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(changed, egui::Button::new("Save and restart the modem"))
                .clicked()
            {
                self.save_settings();
            }
            if ui
                .add_enabled(changed, egui::Button::new("Undo changes"))
                .clicked()
            {
                self.form = Form::from_config(&self.cfg);
                self.problems.clear();
                self.save_error = None;
            }
        });
        if !self.problems.is_empty() {
            ui.label(
                RichText::new("Nothing was saved. Fix the settings marked in red above.")
                    .color(pal.bad),
            );
        }
        if let Some(e) = &self.save_error {
            ui.label(RichText::new(format!("Could not save: {e}")).color(pal.bad));
        }
        if let Some(s) = &self.saved {
            ui.label(RichText::new(s).color(pal.good));
        }
        if let Ok(path) = config::path() {
            ui.add_space(6.0);
            ui.label(
                RichText::new(format!("Settings live in {}", path.display()))
                    .small()
                    .color(pal.muted),
            );
        }
    }
}
