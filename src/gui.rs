#![allow(deprecated)] // nwg::Timer is deprecated in favour of AnimationTimer, but its
// OnTimerTick still fires on the UI thread, which is exactly what this app needs.

use crate::ansi::{Color, ScreenBuffer};
use crate::autodetect::{self, AutoDetectEvent};
use crate::identify::{self, IdentifyResult};
use crate::ipv6::{self, Ipv6Neighbor};
use crate::netconfig;
use crate::netiface::{self, InterfaceConfig};
use crate::pty::PtySession;
use crate::scanner::{self, ScanResult};
use crate::subnets::{self, CustomSubnet};
use crate::tools;
use crate::validate;

use native_windows_derive::NwgUi;
use native_windows_gui as nwg;
use nwg::NativeUi;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const AUTO_REVERT_SECS: u64 = 180;
const TICK_MS: u32 = 200;
// Fixed pty grid size. A real terminal would resize this to match the pane's
// actual pixel size as the window is dragged; nwg's TabsContainer only
// propagates resize to its direct Tab children (not further into a Tab's own
// content, see `TabsContainer::hook_tabs`), so wiring that up would need
// extra per-tab resize plumbing. 80x24 matches the size most device CLIs
// (and `ssh`/`ping`/`tracert` themselves) already assume, which is "enough
// fidelity for guiding someone through a device's CLI over the phone" per
// the issue's stated scope — not attempting full dynamic resize.
const TERMINAL_COLS: u16 = 80;
const TERMINAL_ROWS: u16 = 24;

#[derive(Default, NwgUi)]
pub struct App {
    selected_iface: RefCell<Option<String>>,
    applied: Cell<bool>,
    revert_deadline: Cell<Option<Instant>>,

    scan_rx: RefCell<Option<Receiver<ScanResult>>>,
    scan_cancel: RefCell<Option<Arc<Mutex<bool>>>>,
    scan_running: Arc<AtomicBool>,
    scan_seen: RefCell<HashSet<String>>,

    // IPv6 link-local neighbor discovery runs alongside the ARP sweep on
    // every "Scan for devices" click (folded in rather than a separate
    // button, so you don't have to remember to try it when IPv4 is the
    // thing that's broken). Its own thread/channel/running-flag, same
    // established pattern as the ARP scan above.
    ipv6_rx: RefCell<Option<Receiver<Ipv6Neighbor>>>,
    ipv6_running: Arc<AtomicBool>,
    // True while a scan is in flight and its "N host(s) found" completion
    // message hasn't been reported yet — both the ARP and IPv6 sides clear
    // their own rx independently, but the combined completion status (and
    // the direct-connection hint) should only fire once, after both finish.
    scan_completion_pending: Cell<bool>,

    autodetect_rx: RefCell<Option<Receiver<AutoDetectEvent>>>,
    autodetect_cancel: RefCell<Option<Arc<Mutex<bool>>>>,
    autodetect_running: Arc<AtomicBool>,

    // User-saved subnets (e.g. read off a device's own HMI), tried first
    // during auto-detect, ahead of `autodetect::BUILTIN_CANDIDATES`.
    // Persisted via `subnets::save` — loaded once at startup.
    custom_subnets: RefCell<Vec<CustomSubnet>>,

    // A single persistent channel shared by every identify request, rather
    // than one created per click: selecting a device while an earlier one is
    // still resolving (e.g. clicking a device's IPv4 row, then its IPv6 row
    // before the first finishes) used to replace this channel outright,
    // silently dropping the in-flight request's result — its row would then
    // show "Resolving…" forever. `identify_inflight` is a set (not a single
    // slot) for the same reason: more than one IP can legitimately be
    // resolving at once.
    identify_tx: RefCell<Option<std::sync::mpsc::Sender<(String, IdentifyResult)>>>,
    identify_rx: RefCell<Option<Receiver<(String, IdentifyResult)>>>,
    identify_inflight: RefCell<HashSet<String>>,
    identify_cache: RefCell<HashMap<String, IdentifyResult>>,

    // Live Ping/Traceroute/SSH sessions, one per launched tab. Tabs and
    // their RichTextBoxes are created dynamically (not derive-macro fields)
    // since there's one per tool launch rather than a fixed set — see
    // `spawn_terminal_tab`.
    pty_sessions: RefCell<Vec<PtyTabSession>>,

    // Reads the icon back out of the exe's own compiled-in resources (put
    // there by `winres` in build.rs, at id "1") rather than decoding raw
    // bytes at runtime — the latter (`Icon::from_bin`) requires nwg's
    // "image-decoder" feature and panics without it.
    #[nwg_resource]
    embedded_resources: nwg::EmbedResource,

    #[nwg_resource(source_embed: Some(&data.embedded_resources), source_embed_id: 1)]
    app_icon: nwg::Icon,

    // The window's global font (set via `Font::set_global_family` in `run`)
    // is what every other control picks up automatically; RichTextBox does
    // not, so pty tabs need an explicit font or they render at a tiny
    // default size — see `spawn_terminal_tab`.
    #[nwg_resource(family: "Consolas", size: 18)]
    terminal_font: nwg::Font,

    #[nwg_control(size: (1000, 1180), position: (200, 100), title: "IP Config Tool", icon: Some(&data.app_icon), flags: "WINDOW|VISIBLE|RESIZABLE")]
    #[nwg_events( OnWindowClose: [App::on_close], OnInit: [App::on_init] )]
    window: nwg::Window,

    #[nwg_control(interval: TICK_MS)]
    #[nwg_events( OnTimerTick: [App::on_tick] )]
    timer: nwg::Timer,

    #[nwg_layout(parent: window, spacing: 8, margin: [10,10,10,10], min_size: [950, 1120])]
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
    #[nwg_layout_item(layout: grid, row: 3, col: 3)]
    #[nwg_events( OnButtonClick: [App::on_apply] )]
    apply_button: nwg::Button,

    #[nwg_control(text: "Save subnet")]
    #[nwg_layout_item(layout: grid, row: 3, col: 4)]
    #[nwg_events( OnButtonClick: [App::on_save_subnet] )]
    save_subnet_button: nwg::Button,

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
    #[nwg_layout_item(layout: grid, row: 5, col: 0, col_span: 5, row_span: 5)]
    #[nwg_events( OnListViewClick: [App::on_list_select(SELF, EVT_DATA)] )]
    results_list: nwg::ListView,

    #[nwg_control(text: "Ping")]
    #[nwg_layout_item(layout: grid, row: 10, col: 0)]
    #[nwg_events( OnButtonClick: [App::on_ping] )]
    ping_button: nwg::Button,

    #[nwg_control(text: "Traceroute")]
    #[nwg_layout_item(layout: grid, row: 10, col: 1)]
    #[nwg_events( OnButtonClick: [App::on_traceroute] )]
    traceroute_button: nwg::Button,

    #[nwg_control(text: "SSH")]
    #[nwg_layout_item(layout: grid, row: 10, col: 2)]
    #[nwg_events( OnButtonClick: [App::on_ssh] )]
    ssh_button: nwg::Button,

    #[nwg_control(text: "Open web UI")]
    #[nwg_layout_item(layout: grid, row: 10, col: 3)]
    #[nwg_events( OnButtonClick: [App::on_open_browser] )]
    open_browser_button: nwg::Button,

    #[nwg_control(text: "Wake-on-LAN")]
    #[nwg_layout_item(layout: grid, row: 10, col: 4)]
    #[nwg_events( OnButtonClick: [App::on_wake_on_lan] )]
    wol_button: nwg::Button,

    #[nwg_control(text: "Copy IP")]
    #[nwg_layout_item(layout: grid, row: 11, col: 0)]
    #[nwg_events( OnButtonClick: [App::on_copy_ip] )]
    copy_ip_button: nwg::Button,

    #[nwg_control(text: "Copy MAC")]
    #[nwg_layout_item(layout: grid, row: 11, col: 1)]
    #[nwg_events( OnButtonClick: [App::on_copy_mac] )]
    copy_mac_button: nwg::Button,

    #[nwg_control(text: "Select a device above to enable these actions.")]
    #[nwg_layout_item(layout: grid, row: 11, col: 2, col_span: 3)]
    actions_hint_label: nwg::Label,

    #[nwg_control(text: "")]
    #[nwg_layout_item(layout: grid, row: 12, col: 0, col_span: 5)]
    countdown_label: nwg::Label,

    #[nwg_control(text: "Keep these settings")]
    #[nwg_layout_item(layout: grid, row: 13, col: 0, col_span: 2)]
    #[nwg_events( OnButtonClick: [App::on_keep] )]
    keep_button: nwg::Button,

    #[nwg_control(text: "Revert to original settings")]
    #[nwg_layout_item(layout: grid, row: 13, col: 2, col_span: 3)]
    #[nwg_events( OnButtonClick: [App::on_revert_clicked] )]
    revert_button: nwg::Button,

    #[nwg_control(text: "Ping / Traceroute / SSH sessions appear here as tabs once launched.")]
    #[nwg_layout_item(layout: grid, row: 14, col: 0, col_span: 5)]
    terminal_hint_label: nwg::Label,

    #[nwg_control]
    #[nwg_layout_item(layout: grid, row: 15, col: 0, col_span: 5, row_span: 8)]
    terminal_tabs: nwg::TabsContainer,
}

/// One launched Ping/Traceroute/SSH tab: the tab/output controls, the live
/// ConPTY session, and the virtual screen buffer being rendered into the
/// output box. `session` is `Rc`-shared with the key-forwarding closure
/// bound in `spawn_terminal_tab` (see its comment for why).
struct PtyTabSession {
    title: String,
    tab: nwg::Tab,
    output: nwg::RichTextBox,
    _layout: nwg::GridLayout,
    session: Rc<PtySession>,
    screen: ScreenBuffer,
    ended: Cell<bool>,
    _key_handler: nwg::EventHandler,
}

impl App {
    fn on_init(&self) {
        self.results_list.insert_column("IP address");
        self.results_list.insert_column(nwg::InsertListViewColumn {
            index: Some(1),
            width: Some(150),
            text: Some("MAC address".into()),
            ..Default::default()
        });
        self.results_list.insert_column(nwg::InsertListViewColumn {
            index: Some(2),
            width: Some(160),
            text: Some("Vendor".into()),
            ..Default::default()
        });
        self.results_list.insert_column(nwg::InsertListViewColumn {
            index: Some(3),
            width: Some(160),
            text: Some("Hostname".into()),
            ..Default::default()
        });
        self.results_list.insert_column(nwg::InsertListViewColumn {
            index: Some(4),
            width: Some(220),
            text: Some("Open ports (select a row to resolve)".into()),
            ..Default::default()
        });
        self.results_list.set_headers_enabled(true);

        self.revert_button.set_enabled(false);
        self.keep_button.set_enabled(false);
        self.set_device_actions_enabled(false);

        *self.custom_subnets.borrow_mut() = subnets::load();

        self.refresh_interfaces();
        self.revert_all_backups_silently("Reverted leftover settings from a previous session");

        self.timer.start();
    }

    fn set_device_actions_enabled(&self, enabled: bool) {
        self.ping_button.set_enabled(enabled);
        self.traceroute_button.set_enabled(enabled);
        self.ssh_button.set_enabled(enabled);
        self.open_browser_button.set_enabled(enabled);
        self.wol_button.set_enabled(enabled);
        self.copy_ip_button.set_enabled(enabled);
        self.copy_mac_button.set_enabled(enabled);
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
    ///
    /// Retries briefly if the adapter reports no IP at all: right after
    /// `netsh interface ip set address` returns, Windows can take a moment
    /// to actually settle the new address (the adapter can briefly reset/
    /// renegotiate) before `netsh interface ip show config` reflects it —
    /// without this, an immediate re-read right after a successful Apply
    /// could read that transient gap and wrongly appear to "clear" the IP
    /// box, even though the address was actually applied correctly.
    ///
    /// Retries purely on `cfg.ip.is_none()` — NOT also gated on the `dhcp`
    /// flag having already flipped, since that flag can be just as stale as
    /// the IP itself in the same window (a first attempt that still reports
    /// the old `dhcp: true` would otherwise skip retrying altogether and
    /// clear the box on that one stale read — this was the bug in the first
    /// version of this fix).
    fn refresh_current_config_display(&self, name: &str) {
        const MAX_ATTEMPTS: u32 = 6;
        const RETRY_DELAY: Duration = Duration::from_millis(400);

        let mut last = netiface::get_interface_config(name);
        for _ in 1..MAX_ATTEMPTS {
            match &last {
                Ok(cfg) if cfg.ip.is_none() => {
                    thread::sleep(RETRY_DELAY);
                    last = netiface::get_interface_config(name);
                }
                _ => break,
            }
        }

        match last {
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

    /// Saves (or, if it's already saved, offers to remove) the subnet
    /// currently in the IP/mask fields — e.g. one read straight off a
    /// device's own HMI/settings screen — so future auto-detects try it
    /// first, ahead of the built-in common-range list. Persisted to disk,
    /// so it's remembered on the next visit to the same site, not just this
    /// session.
    fn on_save_subnet(&self) {
        let ip = self.ip_input.text();
        let mask = self.mask_input.text();

        let Some(base) = base_of(&ip) else {
            nwg::modal_error_message(&self.window, "No subnet to save", "Enter a valid IP address first.");
            return;
        };
        if let Err(e) = validate::parse_mask(&mask) {
            nwg::modal_error_message(&self.window, "Invalid subnet mask", &e);
            return;
        }

        let mut list = self.custom_subnets.borrow_mut();

        if subnets::remove(&mut list, &base, &mask) {
            if let Err(e) = subnets::save(&list) {
                nwg::modal_error_message(&self.window, "Could not update saved subnets", &e);
                return;
            }
            self.set_status(&format!("Removed {base}.0/24 ({mask}) from saved subnets ({} remaining).", list.len()));
            return;
        }

        list.push(CustomSubnet { base: base.clone(), mask: mask.clone(), label: None });
        if let Err(e) = subnets::save(&list) {
            list.pop();
            nwg::modal_error_message(&self.window, "Could not save subnet", &e);
            return;
        }
        self.set_status(&format!(
            "Saved {base}.0/24 ({mask}) — tried first on every auto-detect from now on. \
             Click Save subnet again with the same values to remove it. ({} saved total)",
            list.len()
        ));
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

        // Saved subnets first (most likely relevant — the user chose to
        // remember them), then the built-in common-range list.
        let candidates: Vec<(String, String)> = self
            .custom_subnets
            .borrow()
            .iter()
            .map(|c| (c.base.clone(), c.mask.clone()))
            .chain(autodetect::BUILTIN_CANDIDATES.iter().map(|(b, m)| (b.to_string(), m.to_string())))
            .collect();

        let cancelled = Arc::new(Mutex::new(false));
        *self.autodetect_cancel.borrow_mut() = Some(Arc::clone(&cancelled));
        let (tx, rx) = std::sync::mpsc::channel();
        *self.autodetect_rx.borrow_mut() = Some(rx);
        self.autodetect_running.store(true, Ordering::SeqCst);

        let running = Arc::clone(&self.autodetect_running);
        let iface_owned = iface.clone();
        thread::spawn(move || {
            autodetect::run(&iface_owned, &original, &candidates, tx, cancelled);
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
        self.scan_completion_pending.set(true);

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

        self.start_ipv6_discovery();

        self.scan_button.set_enabled(false);
        self.set_status("Scanning (ARP + IPv6 neighbors)...");
    }

    /// Looks up the selected interface's numeric index (needed to scope
    /// link-local pings/sockets) and spawns the IPv6 neighbor-table read on
    /// its own background thread. Best-effort: if the interface can't be
    /// found for some reason, the ARP scan still proceeds without it.
    fn start_ipv6_discovery(&self) {
        let Some(iface_name) = self.current_iface() else { return };
        let Ok(interfaces) = netiface::list_interfaces() else { return };
        let Some(iface_idx) = interfaces.iter().find(|i| i.name == iface_name).map(|i| i.idx) else { return };

        let (tx, rx) = std::sync::mpsc::channel();
        *self.ipv6_rx.borrow_mut() = Some(rx);
        self.ipv6_running.store(true, Ordering::SeqCst);

        let running = Arc::clone(&self.ipv6_running);
        thread::spawn(move || {
            for neighbor in ipv6::discover_neighbors(iface_idx, &iface_name) {
                let _ = tx.send(neighbor);
            }
            running.store(false, Ordering::SeqCst);
        });
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
        self.drain_ipv6_results();
        self.drain_identify_results();
        self.drain_pty_sessions();
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
            self.results_list.insert_items_row(None, &[r.ip.as_str(), r.mac.as_str(), "", "", ""]);
        }

        if !self.scan_running.load(Ordering::SeqCst) && self.scan_rx.borrow().is_some() {
            *self.scan_rx.borrow_mut() = None;
            self.try_report_scan_complete();
        }
    }

    fn drain_ipv6_results(&self) {
        let mut neighbors = Vec::new();
        if let Some(rx) = self.ipv6_rx.borrow().as_ref() {
            neighbors.extend(rx.try_iter());
        }
        for n in neighbors {
            let mut seen = self.scan_seen.borrow_mut();
            if !seen.insert(n.address_with_zone.clone()) {
                continue;
            }
            drop(seen);
            // `netsh interface ipv6 show neighbors` reports MACs lowercase;
            // the ARP-based IPv4 scan (`scanner::format_mac`) uppercases
            // them. Normalize to uppercase here so the same physical NIC
            // shows an identical MAC whether it was found via IPv4 or IPv6 —
            // otherwise the same device's two rows look like they disagree.
            let mac = n.mac.as_deref().unwrap_or("").to_uppercase();
            self.results_list.insert_items_row(None, &[n.address_with_zone.as_str(), mac.as_str(), "", "", ""]);
        }

        if !self.ipv6_running.load(Ordering::SeqCst) && self.ipv6_rx.borrow().is_some() {
            *self.ipv6_rx.borrow_mut() = None;
            self.try_report_scan_complete();
        }
    }

    /// Reports the combined "N host(s) found" status (and the direct-cable
    /// hint) exactly once, after both the ARP sweep and the IPv6 neighbor
    /// read have finished — whichever of the two finishes second is the one
    /// that ends up calling this with everything actually settled.
    fn try_report_scan_complete(&self) {
        if !self.scan_completion_pending.get() {
            return;
        }
        if self.scan_running.load(Ordering::SeqCst) || self.ipv6_running.load(Ordering::SeqCst) {
            return;
        }
        if self.scan_rx.borrow().is_some() || self.ipv6_rx.borrow().is_some() {
            return;
        }

        self.scan_completion_pending.set(false);
        self.scan_button.set_enabled(true);

        let mut message = format!("Scan complete: {} host(s) found.", self.results_list.len());
        if let Some(iface) = self.current_iface() {
            if let Ok(cfg) = netiface::get_interface_config(&iface) {
                if cfg.gateway.is_none() {
                    message.push_str(" No gateway detected — this may be a direct cable connection.");
                }
            }
        }
        self.set_status(&message);
    }

    /// Reads the IP/MAC out of the currently selected results row, if any.
    fn selected_row_ip_mac(&self) -> Option<(String, String)> {
        let row = self.results_list.selected_item()?;
        let ip = self.results_list.item(row, 0, 64)?.text;
        let mac = self.results_list.item(row, 1, 64)?.text;
        Some((ip, mac))
    }

    /// Selecting a row identifies that one device on a background thread
    /// (vendor via the offline OUI table, hostname via reverse DNS/NetBIOS,
    /// a quick port scan) — never automatically for the whole scan, which
    /// would slow the bulk sweep down for no benefit. Cached per-IP so
    /// re-selecting an already-identified row doesn't redo the work.
    fn on_list_select(&self, data: &nwg::EventData) {
        let nwg::EventData::OnListViewItemIndex { row_index, .. } = data else { return };
        if !self.results_list.selected_items().contains(row_index) {
            return; // click landed on a row that isn't actually selected (e.g. deselect)
        }

        let row = *row_index;
        let Some(item) = self.results_list.item(row, 0, 64) else { return };
        let ip = item.text;
        self.set_device_actions_enabled(true);

        if let Some(cached) = self.identify_cache.borrow().get(&ip).cloned() {
            self.write_identify_result(row, &cached);
            return;
        }

        if self.identify_inflight.borrow().contains(&ip) {
            return; // already resolving this one
        }

        let Some(mac_text) = self.results_list.item(row, 1, 64).map(|i| i.text) else { return };
        // A blank/unresolved MAC (e.g. an IPv6 neighbor whose link layer
        // address never resolved) still gets identified — vendor lookup is
        // just skipped for it, not the whole thing.
        let mac = tools::parse_mac(&mac_text);

        self.results_list.update_item(row, nwg::InsertListViewItem {
            column_index: 4,
            text: Some("Resolving…".to_string()),
            ..Default::default()
        });

        self.identify_inflight.borrow_mut().insert(ip.clone());

        // Create the shared channel on the first identify ever, then reuse
        // it (cloning the sender) for every later request — see the
        // `identify_tx`/`identify_rx` field comment for why this can't be
        // recreated per click the way `scan_rx`/`autodetect_rx` are.
        let tx = {
            let mut tx_slot = self.identify_tx.borrow_mut();
            if tx_slot.is_none() {
                let (tx, rx) = std::sync::mpsc::channel();
                *self.identify_rx.borrow_mut() = Some(rx);
                *tx_slot = Some(tx);
            }
            tx_slot.as_ref().unwrap().clone()
        };

        let ip_owned = ip.clone();
        thread::spawn(move || {
            let result = identify::identify(&ip_owned, mac);
            let _ = tx.send((ip_owned, result));
        });
    }

    fn drain_identify_results(&self) {
        let mut results = Vec::new();
        if let Some(rx) = self.identify_rx.borrow().as_ref() {
            results.extend(rx.try_iter());
        }
        for (ip, result) in results {
            self.identify_cache.borrow_mut().insert(ip.clone(), result.clone());
            self.identify_inflight.borrow_mut().remove(&ip);
            if let Some(row) = self.find_row_by_ip(&ip) {
                self.write_identify_result(row, &result);
            }
        }
    }

    fn find_row_by_ip(&self, ip: &str) -> Option<usize> {
        (0..self.results_list.len()).find(|&row| self.results_list.item(row, 0, 64).map(|i| i.text.as_str() == ip).unwrap_or(false))
    }

    fn write_identify_result(&self, row: usize, result: &IdentifyResult) {
        let vendor = result.vendor.as_deref().unwrap_or("(unknown)");
        let hostname = result.hostname.as_deref().unwrap_or("(none)");
        let ports = if result.open_ports.is_empty() { "(none open)".to_string() } else { result.open_ports.join(", ") };

        self.results_list.update_item(row, nwg::InsertListViewItem {
            column_index: 2,
            text: Some(vendor.to_string()),
            ..Default::default()
        });
        self.results_list.update_item(row, nwg::InsertListViewItem {
            column_index: 3,
            text: Some(hostname.to_string()),
            ..Default::default()
        });
        self.results_list.update_item(row, nwg::InsertListViewItem {
            column_index: 4,
            text: Some(ports),
            ..Default::default()
        });
    }

    fn on_ping(&self) {
        let Some((ip, _)) = self.selected_row_ip_mac() else { return };
        self.launch_terminal_tool(&format!("Ping {ip}"), "ping", &["-t", &ip], "Could not start ping");
    }

    fn on_traceroute(&self) {
        let Some((ip, _)) = self.selected_row_ip_mac() else { return };
        self.launch_terminal_tool(&format!("Traceroute {ip}"), "tracert", &[&ip], "Could not start traceroute");
    }

    fn on_ssh(&self) {
        let Some((ip, _)) = self.selected_row_ip_mac() else { return };
        self.launch_terminal_tool(&format!("SSH {ip}"), "ssh", &[&ip], "Could not start SSH");
    }

    /// Opens `program args...` in a new in-window ConPTY tab. Falls back to
    /// the old external-console `tools::launch_visible` (with a status
    /// message explaining why) if ConPTY itself is unavailable — e.g.
    /// Windows older than the 1809 update — rather than hard-failing.
    fn launch_terminal_tool(&self, title: &str, program: &str, args: &[&str], fail_title: &str) {
        match self.spawn_terminal_tab(title, program, args) {
            Ok(()) => {
                self.set_status(&format!("{title}: session started."));
            }
            Err(e) => {
                self.set_status(&format!("In-window terminal unavailable ({e}); opening external console instead."));
                if let Err(e) = tools::launch_visible(program, args) {
                    let hint = if program == "ssh" {
                        "\n\nWindows' OpenSSH client (ssh.exe) may not be installed — it's an optional Windows feature."
                    } else {
                        ""
                    };
                    nwg::modal_error_message(&self.window, fail_title, &format!("{e}{hint}"));
                }
            }
        }
    }

    /// Spawns `program args...` under a real ConPTY, in a new tab of
    /// `terminal_tabs`. Keystrokes typed into the tab's (read-only) output
    /// box are forwarded raw to the pty; the pty's own output is decoded
    /// from the ANSI byte stream and rendered back on the timer tick (see
    /// `drain_pty_sessions`) rather than synchronously here, matching the
    /// scan/autodetect/identify background-thread pattern already used
    /// elsewhere in this file.
    fn spawn_terminal_tab(&self, title: &str, program: &str, args: &[&str]) -> Result<(), String> {
        let session = PtySession::spawn(program, args, TERMINAL_COLS, TERMINAL_ROWS)?;
        let session = Rc::new(session);

        let mut tab = nwg::Tab::default();
        nwg::Tab::builder().parent(&self.terminal_tabs).text(title).build(&mut tab).map_err(|e| e.to_string())?;

        let mut output = nwg::RichTextBox::default();
        nwg::RichTextBox::builder()
            .parent(&tab)
            .readonly(true)
            .font(Some(&self.terminal_font))
            .flags(nwg::RichTextBoxFlags::VISIBLE | nwg::RichTextBoxFlags::VSCROLL | nwg::RichTextBoxFlags::AUTOVSCROLL)
            .build(&mut output)
            .map_err(|e| e.to_string())?;

        let layout = nwg::GridLayout::default();
        nwg::GridLayout::builder().parent(&tab).spacing(0).child(0, 0, &output).build(&layout).map_err(|e| e.to_string())?;

        // Read-only prevents the control's default WM_CHAR handling from
        // locally inserting typed characters (which would double up with
        // the remote echo); we still receive OnChar/OnKeyPress on it and
        // forward those bytes to the pty ourselves — see `key_event_bytes`.
        let forward_session = Rc::clone(&session);
        let key_handler = nwg::bind_event_handler(&output.handle, &tab.handle, move |evt, evt_data, _handle| {
            let bytes = match evt {
                nwg::Event::OnChar => key_event_bytes_from_char(evt_data.on_char()),
                nwg::Event::OnKeyPress => key_event_bytes_from_vk(evt_data.on_key()),
                _ => None,
            };
            if let Some(bytes) = bytes {
                forward_session.write_input(&bytes);
            }
        });

        let index = self.terminal_tabs.tab_count().saturating_sub(1);
        self.terminal_tabs.set_selected_tab(index);
        output.set_focus();

        self.pty_sessions.borrow_mut().push(PtyTabSession {
            title: title.to_string(),
            tab,
            output,
            _layout: layout,
            session,
            screen: ScreenBuffer::new(TERMINAL_COLS, TERMINAL_ROWS),
            ended: Cell::new(false),
            _key_handler: key_handler,
        });

        Ok(())
    }

    fn drain_pty_sessions(&self) {
        let mut sessions = self.pty_sessions.borrow_mut();
        for s in sessions.iter_mut() {
            if s.ended.get() {
                continue;
            }

            let chunks: Vec<Vec<u8>> = s.session.output_rx.try_iter().collect();
            for chunk in chunks {
                s.screen.feed(&chunk);
            }
            if s.screen.take_dirty() {
                redraw_pty_output(&s.output, &s.screen);
            }

            if s.session.has_exited() {
                s.ended.set(true);
                s.tab.set_text(&format!("{} (ended)", s.title));
            }
        }
    }

    /// Kills every still-running Ping/Traceroute/SSH session. Called on
    /// window close so no `ping`/`tracert`/`ssh` child process is left
    /// running after the app exits — same unconditional-cleanup philosophy
    /// as `revert_all_backups_silently`. Individual tabs can't be removed
    /// from `terminal_tabs` at runtime (native-windows-gui 1.0.13's
    /// `TabsContainer`/`Tab` has no item-removal API — only `Drop`, which
    /// destroys the tab's window but not its entry in the tab strip), so a
    /// session instead just marks itself "(ended)" when its process exits;
    /// this only forcibly stops sessions still running.
    fn kill_all_pty_sessions(&self) {
        for s in self.pty_sessions.borrow().iter() {
            if !s.ended.get() {
                s.session.kill();
            }
        }
    }

    fn on_open_browser(&self) {
        let Some((ip, _)) = self.selected_row_ip_mac() else { return };
        let scheme = self
            .identify_cache
            .borrow()
            .get(&ip)
            .map(|r| if r.open_ports.contains(&"HTTPS") && !r.open_ports.contains(&"HTTP") { "https" } else { "http" })
            .unwrap_or("http");
        let host = if ip.contains(':') { ipv6::ipv6_url_host(&ip) } else { ip };
        if let Err(e) = tools::open_browser(&format!("{scheme}://{host}")) {
            nwg::modal_error_message(&self.window, "Could not open browser", &e);
        }
    }

    fn on_wake_on_lan(&self) {
        let Some((_, mac_text)) = self.selected_row_ip_mac() else { return };
        let Some(mac) = tools::parse_mac(&mac_text) else {
            nwg::modal_error_message(&self.window, "Invalid MAC address", &mac_text);
            return;
        };
        match tools::wake_on_lan(mac) {
            Ok(()) => self.set_status(&format!("Sent Wake-on-LAN packet to {mac_text}.")),
            Err(e) => {
                nwg::modal_error_message(&self.window, "Wake-on-LAN failed", &e);
            }
        }
    }

    fn on_copy_ip(&self) {
        if let Some((ip, _)) = self.selected_row_ip_mac() {
            nwg::Clipboard::set_data_text(&self.window, &ip);
            self.set_status(&format!("Copied {ip} to clipboard."));
        }
    }

    fn on_copy_mac(&self) {
        if let Some((_, mac)) = self.selected_row_ip_mac() {
            nwg::Clipboard::set_data_text(&self.window, &mac);
            self.set_status(&format!("Copied {mac} to clipboard."));
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
        self.kill_all_pty_sessions();
        self.revert_all_backups_silently("");
        nwg::stop_thread_dispatch();
    }
}

/// Translates an `OnChar` event into the raw byte(s) to send to a pty.
/// `nwg` already gives us the platform's translated character (including
/// Ctrl-combinations, which Windows reports as the corresponding C0 control
/// character, e.g. Ctrl+C -> 0x03) — no separate modifier-state check needed
/// for those.
fn key_event_bytes_from_char(c: char) -> Option<Vec<u8>> {
    // Enter arrives here as '\r'; pty programs (and remote shells) expect
    // that as-is, so pass it straight through rather than translating.
    let mut buf = [0u8; 4];
    Some(c.encode_utf8(&mut buf).as_bytes().to_vec())
}

/// Translates a non-printable `OnKeyPress` virtual-key code (arrows, Home,
/// End, ...) into the ANSI escape sequence a terminal program expects.
/// Printable keys and Enter/Backspace/Tab are handled via `OnChar` instead
/// (this event fires for both, so only the non-printable ones are mapped
/// here to avoid double-sending).
fn key_event_bytes_from_vk(vk: u32) -> Option<Vec<u8>> {
    use nwg::keys;
    let seq: &[u8] = match vk {
        keys::UP => b"\x1b[A",
        keys::DOWN => b"\x1b[B",
        keys::RIGHT => b"\x1b[C",
        keys::LEFT => b"\x1b[D",
        keys::HOME => b"\x1b[H",
        keys::END => b"\x1b[F",
        keys::DELETE => b"\x1b[3~",
        keys::PRIOR => b"\x1b[5~", // Page Up
        keys::NEXT => b"\x1b[6~",  // Page Down
        _ => return None,
    };
    Some(seq.to_vec())
}

/// Redraws a pty tab's whole screen buffer into its RichTextBox: clears the
/// control, writes the plain text back, then applies `CharFormat` color runs
/// on top (the RichEdit "select range, then format the selection" pattern
/// used by nwg's rich-text example). Full-buffer redraw on every dirty tick
/// is simple and fast enough at 80x24; only runs when `take_dirty()` is
/// true, so an idle session costs nothing between ticks.
///
/// `ScreenBuffer` is a fixed 80x24 grid, so `rows_as_runs()` always returns
/// 24 rows — most of them blank padding until the session has produced that
/// much output. Rendering all 24 (and then `scroll_lastline()`-ing to the
/// very end) made the view jump past the real last line into that blank
/// space below it. Trimming trailing blank rows/whitespace before rendering
/// keeps the last *actual* line of output at the bottom, and means there's
/// nothing to scroll to until the content genuinely overflows the visible
/// area — ordinary terminal behavior.
fn redraw_pty_output(output: &nwg::RichTextBox, screen: &ScreenBuffer) {
    let mut rows = screen.rows_as_runs();
    for row in rows.iter_mut() {
        trim_trailing_whitespace(row);
    }
    let last_content_row = rows.iter().rposition(|runs| !runs.is_empty()).unwrap_or(0);
    let rows = &rows[..=last_content_row];

    let text = rows.iter().map(|runs| runs.iter().map(|(s, _, _)| s.as_str()).collect::<String>()).collect::<Vec<_>>().join("\r\n");
    output.set_text(&text);

    let mut offset: u32 = 0;
    for (row_idx, runs) in rows.iter().enumerate() {
        for (run_text, color, bold) in runs {
            let len = run_text.chars().count() as u32;
            if (*color != Color::Default || *bold) && len > 0 {
                output.set_selection(offset..offset + len);
                output.set_char_format(&nwg::CharFormat {
                    text_color: color.rgb().map(|(r, g, b)| [r, g, b]),
                    effects: if *bold { Some(nwg::CharEffects::BOLD) } else { None },
                    ..Default::default()
                });
            }
            offset += len;
        }
        if row_idx + 1 < rows.len() {
            offset += 2; // "\r\n" separator
        }
    }
    output.set_selection(offset..offset);
    output.scroll_lastline();
}

/// Strips trailing space padding from a row's styled runs (dropping runs
/// that become empty), so a row shorter than the buffer's full column width
/// doesn't render as text padded out with invisible trailing spaces.
fn trim_trailing_whitespace(runs: &mut Vec<(String, Color, bool)>) {
    while let Some(last) = runs.last_mut() {
        let trimmed_len = last.0.trim_end().len();
        if trimmed_len == last.0.len() {
            break;
        }
        last.0.truncate(trimmed_len);
        if last.0.is_empty() {
            runs.pop();
        } else {
            break;
        }
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
