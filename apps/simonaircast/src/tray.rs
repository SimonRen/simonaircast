//! The tray app: an icon + context menu driving the [`Engine`].
//!
//! Threads: the tray icon and menu live on the main thread, which runs a Win32
//! message loop. Engine state and discovery results arrive from tokio tasks via
//! a shared inbox plus a thread message (`WM_WAKE`) to wake the loop. A 1 s
//! timer re-checks the inbox, because thread messages posted while the context
//! menu is open are dropped by its modal loop.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::Notify;
use tray_icon::menu::{
    CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, PostThreadMessageW, SetTimer, TranslateMessage, WM_APP,
};

use crate::engine::{Command, Engine, State};
use crate::runtime::HighResTimer;
use crate::settings::AppSettings;
use crate::system::{self, SingleInstance};

const WM_WAKE: u32 = WM_APP + 1;
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(30);
/// (playout delay ms, label) for the Delay submenu.
const DELAYS: [(u64, &str); 3] = [(100, "Real-time"), (150, "Normal"), (250, "Buffered")];

#[derive(Debug, Clone, PartialEq)]
struct Speaker {
    name: String,
    model: String,
}

/// Updates handed from background tasks to the UI thread.
#[derive(Default)]
struct Inbox {
    state: Option<State>,
    speakers: Option<Vec<Speaker>>,
    firewall_ok: Option<bool>,
}

pub fn run() -> Result<()> {
    let log_path = crate::logging::init_file();
    install_panic_logging();
    let Some(_instance) = SingleInstance::acquire("simonaircast-tray") else {
        tracing::info!("another instance is already running");
        return Ok(());
    };
    tracing::info!(log = ?log_path, "simonaircast tray starting");

    let runtime = crate::runtime::build()?;
    let _timer = HighResTimer::enter();
    let mut settings = AppSettings::load();
    let engine = {
        let _guard = runtime.enter();
        Engine::spawn(settings.engine())
    };

    // SAFETY: plain query of the calling thread's id.
    let ui_thread = unsafe { GetCurrentThreadId() };
    let wake = move || {
        // SAFETY: posting a thread message has no memory-safety preconditions.
        unsafe { PostThreadMessageW(ui_thread, WM_WAKE, 0, 0) };
    };
    let inbox = Arc::new(Mutex::new(Inbox::default()));
    let refresh = Arc::new(Notify::new());
    spawn_state_forwarder(&runtime, &engine, inbox.clone(), wake);
    spawn_discovery(&runtime, inbox.clone(), refresh.clone(), wake);
    {
        // The installer creates the rule; at runtime we only look. A missing
        // rule shows "Repair firewall rule…" in the menu — never a surprise UAC.
        let inbox = inbox.clone();
        runtime.spawn_blocking(move || {
            let ok = system::firewall_rule_exists();
            if !ok {
                tracing::warn!("firewall rule missing; offering repair in the menu");
            }
            inbox.lock().map(|mut i| i.firewall_ok = Some(ok)).ok();
            wake();
        });
    }

    let mut ui = Ui::new(&settings).context("creating tray icon")?;
    if let Some(speaker) = settings.speaker.clone() {
        engine.send(Command::Start(speaker));
    }

    // SAFETY: thread timer (no window, no callback); delivers WM_TIMER to this loop.
    unsafe { SetTimer(std::ptr::null_mut(), 0, 1000, None) };
    // SAFETY: zeroed MSG is a valid out-parameter.
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    'pump: loop {
        // SAFETY: standard Win32 message loop on the thread that owns the tray icon.
        if unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } <= 0 {
            break;
        }
        // SAFETY: `msg` was filled by GetMessageW.
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            let action = ui.handle(&event.id, &mut settings);
            match action {
                Action::None => {}
                Action::Engine(cmd) => engine.send(cmd),
                Action::Refresh => refresh.notify_one(),
                Action::RepairFirewall => {
                    let inbox = inbox.clone();
                    runtime.spawn_blocking(move || {
                        let ok = system::repair_firewall_elevated().unwrap_or(false);
                        inbox.lock().map(|mut i| i.firewall_ok = Some(ok)).ok();
                        wake();
                    });
                }
                Action::Quit => break 'pump,
            }
        }
        let pending = inbox
            .lock()
            .map(|mut i| std::mem::take(&mut *i))
            .unwrap_or_default();
        ui.apply(pending);
    }

    tracing::info!("quitting");
    engine.send(Command::Stop);
    let mut state = engine.state.clone();
    runtime.block_on(async {
        let _ = tokio::time::timeout(
            Duration::from_secs(3),
            state.wait_for(|s| matches!(s, State::Idle { .. })),
        )
        .await;
    });
    Ok(())
}

fn save(settings: &AppSettings) {
    if let Err(e) = settings.save() {
        tracing::warn!(error = format!("{e:#}"), "saving settings failed");
    }
}

fn install_panic_logging() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("panic: {info}");
        default(info);
    }));
}

fn spawn_state_forwarder(
    runtime: &tokio::runtime::Runtime,
    engine: &Engine,
    inbox: Arc<Mutex<Inbox>>,
    wake: impl Fn() + Send + 'static,
) {
    let mut state = engine.state.clone();
    runtime.spawn(async move {
        loop {
            let current = state.borrow_and_update().clone();
            inbox.lock().map(|mut i| i.state = Some(current)).ok();
            wake();
            if state.changed().await.is_err() {
                return;
            }
        }
    });
}

fn spawn_discovery(
    runtime: &tokio::runtime::Runtime,
    inbox: Arc<Mutex<Inbox>>,
    refresh: Arc<Notify>,
    wake: impl Fn() + Send + 'static,
) {
    runtime.spawn(async move {
        loop {
            match crate::net::discover(Duration::from_secs(3)).await {
                Ok(devices) => {
                    let mut speakers: Vec<Speaker> = devices
                        .into_iter()
                        .map(|d| Speaker {
                            name: d.name,
                            model: d.model,
                        })
                        .collect();
                    speakers.sort_by(|a, b| a.name.cmp(&b.name));
                    speakers.dedup_by(|a, b| a.name == b.name);
                    tracing::debug!(?speakers, "discovered receivers");
                    inbox.lock().map(|mut i| i.speakers = Some(speakers)).ok();
                    wake();
                }
                Err(e) => tracing::warn!(error = format!("{e:#}"), "discovery failed"),
            }
            tokio::select! {
                _ = tokio::time::sleep(DISCOVERY_INTERVAL) => {}
                _ = refresh.notified() => {}
            }
        }
    });
}

enum Action {
    None,
    Engine(Command),
    Refresh,
    RepairFirewall,
    Quit,
}

struct Ui {
    tray: TrayIcon,
    status: MenuItem,
    stop: MenuItem,
    speakers_menu: Submenu,
    speaker_items: Vec<(String, CheckMenuItem)>,
    delay_items: Vec<(u64, CheckMenuItem)>,
    volume_sync: CheckMenuItem,
    autostart: CheckMenuItem,
    firewall: MenuItem,
    icons: Icons,
    state: State,
    speakers: Vec<Speaker>,
}

impl Ui {
    fn new(settings: &AppSettings) -> Result<Self> {
        let status = MenuItem::with_id("status", "Not streaming", false, None);
        let stop = MenuItem::with_id("stop", "Stop streaming", false, None);
        let speakers_menu = Submenu::new("Speakers", true);
        let delay_menu = Submenu::new("Delay", true);
        let delay_items: Vec<(u64, CheckMenuItem)> = DELAYS
            .iter()
            .map(|&(ms, label)| {
                let item = CheckMenuItem::with_id(
                    format!("delay:{ms}"),
                    format!("{label} ({ms} ms)"),
                    true,
                    settings.delay_ms == ms,
                    None,
                );
                (ms, item)
            })
            .collect();
        for (_, item) in &delay_items {
            delay_menu.append(item)?;
        }
        let volume_sync = CheckMenuItem::with_id(
            "volsync",
            "Use Windows volume",
            true,
            settings.volume_sync,
            None,
        );
        let autostart = CheckMenuItem::with_id(
            "autostart",
            "Start with Windows",
            true,
            system::autostart_enabled(),
            None,
        );
        let firewall = MenuItem::with_id("firewall", "Checking firewall…", false, None);

        let menu = Menu::new();
        menu.append_items(&[
            &status,
            &stop,
            &PredefinedMenuItem::separator(),
            &speakers_menu,
            &delay_menu,
            &volume_sync,
            &autostart,
            &firewall,
            &PredefinedMenuItem::separator(),
            &MenuItem::with_id("logs", "Open log folder", true, None),
            &MenuItem::with_id("quit", "Quit", true, None),
        ])?;

        let icons = Icons::new()?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("simonaircast")
            .with_icon(icons.idle.clone())
            .build()?;

        let mut ui = Self {
            tray,
            status,
            stop,
            speakers_menu,
            speaker_items: Vec::new(),
            delay_items,
            volume_sync,
            autostart,
            firewall,
            icons,
            state: State::Idle { note: None },
            speakers: Vec::new(),
        };
        ui.rebuild_speakers();
        Ok(ui)
    }

    /// The speaker we're streaming to or trying to.
    fn target(&self) -> Option<&str> {
        match &self.state {
            State::Idle { .. } => None,
            State::Connecting { speaker }
            | State::Streaming { speaker, .. }
            | State::Retrying { speaker, .. } => Some(speaker),
        }
    }

    fn handle(&mut self, id: &MenuId, settings: &mut AppSettings) -> Action {
        let id = id.as_ref();
        let action = if let Some(name) = id.strip_prefix("spk:") {
            if self.target() == Some(name) {
                settings.speaker = None;
                save(settings);
                Action::Engine(Command::Stop)
            } else {
                settings.speaker = Some(name.to_string());
                save(settings);
                Action::Engine(Command::Start(name.to_string()))
            }
        } else if let Some(ms) = id.strip_prefix("delay:").and_then(|ms| ms.parse().ok()) {
            settings.delay_ms = ms;
            save(settings);
            Action::Engine(Command::Update(settings.engine()))
        } else {
            match id {
                "stop" => {
                    settings.speaker = None;
                    save(settings);
                    Action::Engine(Command::Stop)
                }
                "volsync" => {
                    settings.volume_sync = !settings.volume_sync;
                    save(settings);
                    Action::Engine(Command::Update(settings.engine()))
                }
                "autostart" => {
                    let enable = !system::autostart_enabled();
                    let result = std::env::current_exe()
                        .context("locating simonaircast-tray.exe")
                        .and_then(|exe| system::set_autostart(enable, &exe));
                    if let Err(e) = result {
                        tracing::warn!(error = format!("{e:#}"), "changing autostart failed");
                    }
                    Action::None
                }
                "firewall" => Action::RepairFirewall,
                "refresh" => Action::Refresh,
                "logs" => {
                    system::open_folder(&crate::logging::log_dir());
                    Action::None
                }
                "quit" => Action::Quit,
                _ => Action::None,
            }
        };
        // Muda toggles check items itself on click; re-assert the real state.
        self.sync_checks(settings);
        action
    }

    fn sync_checks(&self, settings: &AppSettings) {
        for (ms, item) in &self.delay_items {
            item.set_checked(*ms == settings.delay_ms);
        }
        self.volume_sync.set_checked(settings.volume_sync);
        self.autostart.set_checked(system::autostart_enabled());
        let target = settings.speaker.as_deref().or(self.target());
        for (name, item) in &self.speaker_items {
            item.set_checked(Some(name.as_str()) == target);
        }
    }

    fn apply(&mut self, inbox: Inbox) {
        if let Some(speakers) = inbox.speakers
            && speakers != self.speakers
        {
            self.speakers = speakers;
            self.rebuild_speakers();
        }
        if let Some(ok) = inbox.firewall_ok {
            self.firewall.set_text(if ok {
                "Firewall: allowed"
            } else {
                "Repair firewall rule…"
            });
            self.firewall.set_enabled(!ok);
        }
        if let Some(state) = inbox.state
            && state != self.state
        {
            self.state = state;
            self.render_state();
        }
    }

    fn render_state(&self) {
        let (text, tooltip, icon) = match &self.state {
            State::Idle { note } => {
                let text = note.clone().unwrap_or_else(|| "Not streaming".into());
                (text.clone(), text, &self.icons.idle)
            }
            State::Connecting { speaker } => {
                let text = format!("Connecting to {speaker}…");
                (text.clone(), text, &self.icons.connecting)
            }
            State::Streaming { speaker, delay_ms } => (
                format!("● Streaming to {speaker} ({delay_ms} ms)"),
                format!("Streaming to {speaker}, {delay_ms} ms behind the PC"),
                &self.icons.streaming,
            ),
            State::Retrying {
                speaker,
                reason,
                retry_in,
            } => (
                format!("{speaker}: reconnecting in {} s", retry_in.as_secs()),
                format!("{speaker}: {reason}"),
                &self.icons.retrying,
            ),
        };
        self.status.set_text(text);
        self.stop.set_enabled(self.target().is_some());
        // Windows truncates tray tooltips at 127 characters.
        let tooltip: String = format!("simonaircast\n{tooltip}")
            .chars()
            .take(127)
            .collect();
        let _ = self.tray.set_tooltip(Some(tooltip));
        let _ = self.tray.set_icon(Some(icon.clone()));
        let target = self.target();
        for (name, item) in &self.speaker_items {
            item.set_checked(Some(name.as_str()) == target);
        }
    }

    fn rebuild_speakers(&mut self) {
        while self.speakers_menu.remove_at(0).is_some() {}
        self.speaker_items.clear();

        let mut names: Vec<(String, String)> = self
            .speakers
            .iter()
            .map(|s| (s.name.clone(), s.model.clone()))
            .collect();
        // Keep the current target listed even if discovery missed it this round.
        if let Some(target) = self.target()
            && !names.iter().any(|(n, _)| n == target)
        {
            names.push((target.to_string(), String::new()));
        }

        if names.is_empty() {
            let _ = self
                .speakers_menu
                .append(&MenuItem::new("Searching…", false, None));
        }
        let target = self.target().map(str::to_string);
        for (name, model) in names {
            let label = match friendly_model(&model) {
                Some(kind) => format!("{name}  ({kind})"),
                None => name.clone(),
            };
            let item = CheckMenuItem::with_id(
                format!("spk:{name}"),
                label,
                true,
                target.as_deref() == Some(name.as_str()),
                None,
            );
            let _ = self.speakers_menu.append(&item);
            self.speaker_items.push((name, item));
        }
        let _ = self.speakers_menu.append(&PredefinedMenuItem::separator());
        let _ =
            self.speakers_menu
                .append(&MenuItem::with_id("refresh", "Refresh list", true, None));
    }
}

/// Human name for common Apple model identifiers.
fn friendly_model(model: &str) -> Option<&'static str> {
    match model {
        m if m.starts_with("AudioAccessory5") => Some("HomePod mini"),
        m if m.starts_with("AudioAccessory") => Some("HomePod"),
        m if m.starts_with("AppleTV") => Some("Apple TV"),
        m if m.starts_with("Mac") => Some("Mac"),
        "" => None,
        _ => Some("AirPlay"),
    }
}

struct Icons {
    idle: Icon,
    connecting: Icon,
    streaming: Icon,
    retrying: Icon,
}

impl Icons {
    fn new() -> Result<Self> {
        use crate::icon::{CONNECTING, IDLE, RETRYING, STREAMING};
        Ok(Self {
            idle: draw_icon(IDLE)?,
            connecting: draw_icon(CONNECTING)?,
            streaming: draw_icon(STREAMING)?,
            retrying: draw_icon(RETRYING)?,
        })
    }
}

/// Tray icons are 16 logical px; 32 px stays sharp up to 200 % scaling.
const TRAY_ICON_PX: usize = 32;

fn draw_icon(color: [u8; 3]) -> Result<Icon> {
    let size = TRAY_ICON_PX as u32;
    Ok(Icon::from_rgba(
        crate::icon::rgba(color, TRAY_ICON_PX),
        size,
        size,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_render() {
        assert!(Icons::new().is_ok());
    }

    #[test]
    fn names_common_models() {
        assert_eq!(friendly_model("AudioAccessory1,1"), Some("HomePod"));
        assert_eq!(friendly_model("AudioAccessory5,1"), Some("HomePod mini"));
        assert_eq!(friendly_model("AppleTV14,1"), Some("Apple TV"));
        assert_eq!(friendly_model(""), None);
    }
}
