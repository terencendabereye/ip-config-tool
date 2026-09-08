#![allow(deprecated)] // nwg::Timer is deprecated in favour of AnimationTimer, but its
// OnTimerTick still fires on the UI thread, which is exactly what this app needs.

use crate::autodetect::{self, AutoDetectEvent};
use crate::netconfig;
use crate::netiface::{self, InterfaceConfig};
use crate::scanner::{self, ScanResult};
use crate::validate;

use native_windows_derive::NwgUi;
use native_windows_gui as nwg;
use nwg::NativeUi;

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const AUTO_REVERT_SECS: u64 = 180;
const TICK_MS: u32 = 200;

#[derive(Default, NwgUi)]
pub struct App {
    selected_iface: RefCell<Option<String>>,
    applied: Cell<bool>,
    revert_deadline: Cell<Option<Instant>>,

    scan_rx: RefCell<Option<Receiver<ScanResult>>>,
    scan_cancel: RefCell<Option<Arc<Mutex<bool>>>>,
    scan_running: Arc<AtomicBool>,
    scan_seen: RefCell<HashSet<String>>,

    autodetect_rx: RefCell<Option<Receiver<AutoDetectEvent>>>,
    autodetect_cancel: RefCell<Option<Arc<Mutex<bool>>>>,
    autodetect_running: Arc<AtomicBool>,

    #[nwg_control(size: (920, 660), position: (250, 150), title: "IP Config Tool", flags: "WINDOW|VISIBLE|RESIZABLE")]
    #[nwg_events( OnWindowClose: [App::on_close], OnInit: [App::on_init] )]
    window: nwg::Window,

    #[nwg_control(interval: TICK_MS)]
    #[nwg_events( OnTimerTick: [App::on_tick] )]
    timer: nwg::Timer,

    #[nwg_layout(parent: window, spacing: 8, margin: [10,10,10,10], min_size: [880, 600])]
    grid: nwg::GridLayout,

    #[nwg_control(text: "Adapter:")]
    #[nwg_layout_item(layout: grid, row: 0, col: 0)]
    iface_label: nwg::Label,

    #[nwg_control(collection: vec![])]
    #[nwg_layout_item(layout: grid, row: 0, col: 1, col_span: 3)]
    #[nwg_events( OnComboxBoxSelection: [App::on_iface_selected] )]
    iface_combo: nwg::ComboBox<String>,

    #[nwg_control(text: "Refresh")]
    #[nwg_layout_item(layout: grid, row: 0, col: 4)]
    #[nwg_events( OnButtonClick: [App::on_refresh] )]
    refresh_button: nwg::Button,

    #[nwg_control(text: "Select an adapter above.")]
    #[nwg_layout_item(layout: grid, row: 1, col: 0, col_span: 5)]
    current_cfg_label: nwg::Label,

    #[nwg_control(text: "IP address:")]
    #[nwg_layout_item(layout: grid, row: 2, col: 0)]
    ip_label: nwg::Label,

    #[nwg_control(text: "")]
    #[nwg_layout_item(layout: grid, row: 2, col: 1)]
    ip_input: nwg::TextInput,

    #[nwg_control(text: "Subnet mask:")]
    #[nwg_layout_item(layout: grid, row: 2, col: 2)]
    mask_label: nwg::Label,

    #[nwg_control(text: "255.255.255.0")]
    #[nwg_layout_item(layout: grid, row: 2, col: 3, col_span: 2)]
    mask_input: nwg::TextInput,

    #[nwg_control(text: "Gateway (optional):")]
    #[nwg_layout_item(layout: grid, row: 3, col: 0)]
    gateway_label: nwg::Label,

    #[nwg_control(text: "")]
    #[nwg_layout_item(layout: grid, row: 3, col: 1)]
    gateway_input: nwg::TextInput,

    #[nwg_control(text: "Auto-detect")]
    #[nwg_layout_item(layout: grid, row: 3, col: 2)]
    #[nwg_events( OnButtonClick: [App::on_autodetect] )]
    autodetect_button: nwg::Button,

    #[nwg_control(text: "Apply")]
    #[nwg_layout_item(layout: grid, row: 3, col: 3, col_span: 2)]
    #[nwg_events( OnButtonClick: [App::on_apply] )]
    apply_button: nwg::Button,

    #[nwg_control(text: "Scan for devices")]
    #[nwg_layout_item(layout: grid, row: 4, col: 0, col_span: 2)]
    #[nwg_events( OnButtonClick: [App::on_scan] )]
    scan_button: nwg::Button,

    #[nwg_control(text: "Cancel")]
    #[nwg_layout_item(layout: grid, row: 4, col: 2)]
    #[nwg_events( OnButtonClick: [App::on_cancel] )]
    cancel_button: nwg::Button,

    #[nwg_control(text: "Idle.")]
    #[nwg_layout_item(layout: grid, row: 4, col: 3, col_span: 2)]
    status_label: nwg::Label,

    #[nwg_control(list_style: nwg::ListViewStyle::Detailed, ex_flags: nwg::ListViewExFlags::GRID | nwg::ListViewExFlags::FULL_ROW_SELECT)]
    #[nwg_layout_item(layout: grid, row: 5, col: 0, col_span: 5, row_span: 6)]
    results_list: nwg::ListView,

    #[nwg_control(text: "")]
    #[nwg_layout_item(layout: grid, row: 11, col: 0, col_span: 5)]
    countdown_label: nwg::Label,

    #[nwg_control(text: "Keep these settings")]
    #[nwg_layout_item(layout: grid, row: 12, col: 0, col_span: 2)]
    #[nwg_events( OnButtonClick: [App::on_keep] )]
    keep_button: nwg::Button,

    #[nwg_control(text: "Revert to original settings")]
    #[nwg_layout_item(layout: grid, row: 12, col: 2, col_span: 3)]
    #[nwg_events( OnButtonClick: [App::on_revert_clicked] )]
    revert_button: nwg::Button,
}

impl App {
    fn on_init(&self) {
        self.results_list.insert_column("IP address");
        self.results_list.insert_column(nwg::InsertListViewColumn {
            index: Some(1),
            width: Some(180),
            text: Some("MAC address".into()),
            ..Default::default()
        });
        self.results_list.set_headers_enabled(true);

        self.revert_button.set_enabled(false);
        self.keep_button.set_enabled(false);

        self.refresh_interfaces();
        self.revert_all_backups_silently("Reverted leftover settings from a previous session");

        self.timer.start();
    }

    fn refresh_interfaces(&self) {
        match netiface::list_interfaces() {
            Ok(rows) => {
                let names: Vec<String> = rows.into_iter().map(|r| r.name).collect();
                self.iface_combo.set_collection(names);
                if self.iface_combo.selection().is_none() && self.iface_combo.len() > 0 {
                    self.iface_combo.set_selection(Some(0));
                    self.on_iface_selected();
                }
            }
            Err(e) => self.set_status(&format!("Failed to list adapters: {e}")),
        }
    }

    /// Reverts every interface that still has a saved backup (whether from
    /// this session or a previous one that didn't shut down cleanly), with
    /// no per-item prompts. Shows a single combined error dialog only if a
    /// revert genuinely fails — that's a real problem the user needs to
    /// know about, not routine confirmation noise.
    fn revert_all_backups_silently(&self, status_on_success: &str) {
        let orphans = netconfig::list_orphan_backups();
        if orphans.is_empty() {
            return;
        }

        let mut failures = Vec::new();
        for backup in &orphans {
            if let Err(e) = netconfig::revert(&backup.config.name) {
                failures.push(format!("{}: {e}", backup.config.name));
            }
        }

        if failures.is_empty() {
            self.set_status(status_on_success);
        } else {
            nwg::modal_error_message(
                &self.window,
                "Could not fully revert",
                &format!("Some adapters could not be reverted automatically:\n\n{}", failures.join("\n")),
            );
        }

        if let Some(iface) = self.current_iface() {
            self.applied.set(netconfig::has_backup(&iface));
            self.revert_button.set_enabled(self.applied.get());
            self.refresh_current_config_display(&iface);
        }
    }

    fn on_refresh(&self) {
        self.refresh_interfaces();
    }

    fn on_iface_selected(&self) {
        let Some(name) = self.iface_combo.selection_string() else { return };
        *self.selected_iface.borrow_mut() = Some(name.clone());
        self.refresh_current_config_display(&name);

        let has_backup = netconfig::has_backup(&name);
        self.applied.set(has_backup);
        self.revert_button.set_enabled(has_backup);
    }

    /// Always overwrites the IP/mask/gateway fields with the selected
    /// adapter's real current values. This is called on every interface
    /// selection change and after every Apply/Revert/auto-detect, so it must
    /// never leave a stale value from a previously-selected adapter sitting
    /// in a field (that previously caused the IP field to get stuck on the
    /// first-listed adapter's address after switching to a different one).
    fn refresh_current_config_display(&self, name: &str) {
        match netiface::get_interface_config(name) {
            Ok(cfg) => {
                self.current_cfg_label.set_text(&summarize(&cfg));
                self.ip_input.set_text(cfg.ip.as_deref().unwrap_or(""));
                self.mask_input.set_text(cfg.mask.as_deref().unwrap_or("255.255.255.0"));
                self.gateway_input.set_text(cfg.gateway.as_deref().unwrap_or(""));
            }
            Err(e) => self.current_cfg_label.set_text(&format!("Could not read config: {e}")),
        }
    }

    fn set_status(&self, text: &str) {
        self.status_label.set_text(text);
    }

    fn current_iface(&self) -> Option<String> {
        self.selected_iface.borrow().clone()
    }

    fn on_apply(&self) {
        let Some(iface) = self.current_iface() else {
            nwg::modal_error_message(&self.window, "No adapter selected", "Pick an adapter first.");
            return;
        };

        let ip = self.ip_input.text();
        let mask = self.mask_input.text();
        let gateway = self.gateway_input.text();

        if let Err(e) = validate::validate_static_config(&ip, &mask, &gateway) {
            nwg::modal_error_message(&self.window, "Invalid settings", &e);
            return;
        }

        if !netconfig::has_backup(&iface) {
            if let Err(e) = netconfig::backup_current_config(&iface) {
                nwg::modal_error_message(&self.window, "Could not save backup", &e);
                return;
            }
        }

        match netconfig::apply_static(&iface, &ip, &mask, &gateway) {
            Ok(()) => {
                self.applied.set(true);
                self.revert_button.set_enabled(true);
                self.keep_button.set_enabled(true);
                self.revert_deadline.set(Some(Instant::now() + Duration::from_secs(AUTO_REVERT_SECS)));
                self.set_status(&format!("Applied static config to '{iface}'."));
                self.refresh_current_config_display(&iface);
            }
            Err(e) => {
                nwg::modal_error_message(&self.window, "Apply failed", &e);
            }
        }
    }

    fn on_keep(&self) {
        self.revert_deadline.set(None);
        self.countdown_label.set_text("");
        self.set_status("Keeping current settings (auto-revert cancelled).");
    }

    fn on_revert_clicked(&self) {
        self.do_revert();
    }

    fn do_revert(&self) {
        let Some(iface) = self.current_iface() else { return };
        match netconfig::revert(&iface) {
            Ok(()) => {
                self.applied.set(false);
                self.revert_deadline.set(None);
                self.countdown_label.set_text("");
                self.revert_button.set_enabled(false);
                self.keep_button.set_enabled(false);
                self.set_status(&format!("Reverted '{iface}' to its original settings."));
                self.refresh_current_config_display(&iface);
            }
            Err(e) => {
                nwg::modal_error_message(&self.window, "Revert failed", &e);
            }
        }
    }

    fn on_autodetect(&self) {
        let Some(iface) = self.current_iface() else {
            nwg::modal_error_message(&self.window, "No adapter selected", "Pick an adapter first.");
            return;
        };
        if self.autodetect_running.load(Ordering::SeqCst) || self.scan_running.load(Ordering::SeqCst) {
            return;
        }

        if !netconfig::has_backup(&iface) {
            if let Err(e) = netconfig::backup_current_config(&iface) {
                nwg::modal_error_message(&self.window, "Could not save backup", &e);
                return;
            }
        }
        let Some(original) = netconfig::load_backup(&iface).map(|b| b.config) else {
            nwg::modal_error_message(&self.window, "Error", "Backup missing unexpectedly.");
            return;
        };

        let cancelled = Arc::new(Mutex::new(false));
        *self.autodetect_cancel.borrow_mut() = Some(Arc::clone(&cancelled));
        let (tx, rx) = std::sync::mpsc::channel();
        *self.autodetect_rx.borrow_mut() = Some(rx);
        self.autodetect_running.store(true, Ordering::SeqCst);

        let running = Arc::clone(&self.autodetect_running);
        let iface_owned = iface.clone();
        thread::spawn(move || {
            autodetect::run(&iface_owned, &original, tx, cancelled);
            running.store(false, Ordering::SeqCst);
        });

        self.autodetect_button.set_enabled(false);
        self.apply_button.set_enabled(false);
        self.set_status("Auto-detecting subnet...");
    }

    fn on_scan(&self) {
        if self.scan_running.load(Ordering::SeqCst) || self.autodetect_running.load(Ordering::SeqCst) {
            return;
        }
        let ip = self.ip_input.text();
        let base = base_of(&ip);
        let Some(base) = base else {
            nwg::modal_error_message(&self.window, "No base subnet", "Enter an IP address first (its first 3 octets are used as the /24 to scan).");
            return;
        };

        self.results_list.clear();
        self.scan_seen.borrow_mut().clear();

        let cancelled = Arc::new(Mutex::new(false));
        *self.scan_cancel.borrow_mut() = Some(Arc::clone(&cancelled));
        let (tx, rx) = std::sync::mpsc::channel();
        *self.scan_rx.borrow_mut() = Some(rx);
        self.scan_running.store(true, Ordering::SeqCst);

        let running = Arc::clone(&self.scan_running);
        thread::spawn(move || {
            scanner::sweep_subnet_v24(&base, tx, cancelled);
            running.store(false, Ordering::SeqCst);
        });

        self.scan_button.set_enabled(false);
        self.set_status("Scanning (ARP)...");
    }

    fn on_cancel(&self) {
        if let Some(flag) = self.scan_cancel.borrow().as_ref() {
            *flag.lock().unwrap() = true;
        }
        if let Some(flag) = self.autodetect_cancel.borrow().as_ref() {
            *flag.lock().unwrap() = true;
        }
    }

    fn on_tick(&self) {
        self.drain_autodetect_events();
        self.drain_scan_results();
        self.tick_revert_countdown();
    }

    fn drain_autodetect_events(&self) {
        let mut events = Vec::new();
        if let Some(rx) = self.autodetect_rx.borrow().as_ref() {
            events.extend(rx.try_iter());
        }
        for evt in events {
            match evt {
                AutoDetectEvent::TryingCandidate { base, .. } => {
                    self.set_status(&format!("Trying {base}.0/24..."));
                }
                AutoDetectEvent::CandidateResult { base, host_count } => {
                    self.set_status(&format!("{base}.0/24: {host_count} response(s)."));
                }
                AutoDetectEvent::Found { base, mask } => {
                    self.ip_input.set_text(&format!("{base}.{}", autodetect::PROBE_HOST_OCTET));
                    self.mask_input.set_text(&mask);
                    self.set_status(&format!("Found live device(s) on {base}.0/24 — review below, then Apply to keep it or Revert to undo."));
                    self.applied.set(true);
                    self.revert_button.set_enabled(true);
                    self.keep_button.set_enabled(true);
                    self.revert_deadline.set(Some(Instant::now() + Duration::from_secs(AUTO_REVERT_SECS)));
                    if let Some(iface) = self.current_iface() {
                        self.refresh_current_config_display(&iface);
                    }
                }
                AutoDetectEvent::NotFound => {
                    self.set_status("No devices found on common subnets. Enter values manually.");
                }
                AutoDetectEvent::Cancelled => {
                    self.set_status("Auto-detect cancelled; original settings restored.");
                }
                AutoDetectEvent::Error(e) => {
                    self.set_status(&format!("Auto-detect error: {e}"));
                }
            }
        }

        if !self.autodetect_running.load(Ordering::SeqCst) && self.autodetect_rx.borrow().is_some() {
            *self.autodetect_rx.borrow_mut() = None;
            self.autodetect_button.set_enabled(true);
            self.apply_button.set_enabled(true);
        }
    }

    fn drain_scan_results(&self) {
        let mut results = Vec::new();
        if let Some(rx) = self.scan_rx.borrow().as_ref() {
            results.extend(rx.try_iter());
        }
        for r in results {
            let mut seen = self.scan_seen.borrow_mut();
            if !seen.insert(r.ip.clone()) {
                continue;
            }
            drop(seen);
            self.results_list.insert_items_row(None, &[r.ip.as_str(), r.mac.as_str()]);
        }

        if !self.scan_running.load(Ordering::SeqCst) && self.scan_rx.borrow().is_some() {
            *self.scan_rx.borrow_mut() = None;
            self.scan_button.set_enabled(true);
            self.set_status(&format!("Scan complete: {} host(s) found.", self.results_list.len()));
        }
    }

    fn tick_revert_countdown(&self) {
        let Some(deadline) = self.revert_deadline.get() else { return };
        let now = Instant::now();
        if now >= deadline {
            self.revert_deadline.set(None);
            self.countdown_label.set_text("");
            self.do_revert();
            return;
        }
        let remaining = (deadline - now).as_secs() + 1;
        self.countdown_label
            .set_text(&format!("Reverting automatically in {remaining}s unless you click Keep."));
    }

    /// Reverts every applied adapter on the way out, with no confirmation
    /// prompt — the user should never have to remember to clean up network
    /// settings themselves. A dialog only appears if a revert genuinely fails.
    fn on_close(&self) {
        self.revert_all_backups_silently("");
        nwg::stop_thread_dispatch();
    }
}

fn summarize(cfg: &InterfaceConfig) -> String {
    if cfg.dhcp {
        match (&cfg.ip, &cfg.gateway) {
            (Some(ip), Some(gw)) => format!("DHCP: {ip} (gateway {gw})"),
            (Some(ip), None) => format!("DHCP: {ip}"),
            (None, _) => "DHCP: (no address / disconnected)".to_string(),
        }
    } else {
        match (&cfg.ip, &cfg.mask) {
            (Some(ip), Some(mask)) => {
                let gw = cfg.gateway.as_deref().unwrap_or("(none)");
                format!("Static: {ip} / {mask} (gateway {gw})")
            }
            _ => "Static: (unconfigured)".to_string(),
        }
    }
}

/// "192.168.1.42" -> Some("192.168.1")
fn base_of(ip: &str) -> Option<String> {
    let octets = validate::parse_ipv4(ip).ok()?;
    Some(format!("{}.{}.{}", octets[0], octets[1], octets[2]))
}

pub fn run() {
    nwg::init().expect("Failed to init Native Windows GUI");
    nwg::Font::set_global_family("Segoe UI").expect("Failed to set default font");
    let _app = App::build_ui(Default::default()).expect("Failed to build UI");
    nwg::dispatch_thread_events();
}
