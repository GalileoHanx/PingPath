//! PingPath's window (docs/decisions/007-client-architecture.md). It runs
//! as the signed-in user and never elevates: everything that needs rights
//! (the adapter, routes, the tunnel) happens in the PingPath service, which
//! this app asks over the named pipe (`service`, `transport`).
//!
//! The page (`frontend/`) calls the commands below and listens to three
//! events: `connection` (whether the service can be reached), `status` and
//! `sample` (forwarded from the service). The Rust side also keeps an eye
//! on the status itself, for the tray icon, for hiding the window instead
//! of closing it while Optimize is on, and for Windows notifications when
//! a session falls back while the window is hidden.

// No console window behind the app in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod prefs;
mod report;
mod service;
mod strings;
mod transport;
mod tray;

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use pingpath_client_core::ipc::{ErrorBody, Request, SettingsChange};
use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager, RunEvent, State, WindowEvent};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_notification::NotificationExt;

use prefs::{Language, LanguagePreference, Prefs};
use service::{Client, Connection, Event};
use strings::Strings;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What the commands and event handlers share.
struct Ui {
    client: Client,
    prefs: Mutex<Prefs>,
    /// `None` if the system has no app data folder; preferences then last
    /// until the window closes.
    prefs_path: Option<PathBuf>,
    system_locale: Option<String>,
    session: Mutex<Session>,
    /// The window is hidden in the tray.
    hidden: AtomicBool,
    /// `None` if the tray icon couldn't be created; the window then never
    /// hides, since nothing could bring it back.
    tray: Option<tray::Tray>,
}

/// What the Rust side follows of the service's status.
#[derive(Default)]
struct Session {
    /// A session is on (connecting, optimized or failing over).
    active: bool,
    /// ... and it isn't another user's.
    ours: bool,
    /// Notices already seen, so each raises a notification once.
    seen_notices: HashSet<u64>,
    /// The player has been told once that the window went to the tray.
    told_about_tray: bool,
    /// What the tray icon shows now, so a status that changes nothing for
    /// it (one arrives every second while optimized) changes nothing.
    tray_shown: Option<TrayState>,
}

/// The tray icon's state: shown, "Turn Optimize off" enabled, the
/// "Optimize is on" tooltip, and the language of its words.
#[derive(Clone, Copy, PartialEq, Eq)]
struct TrayState {
    visible: bool,
    can_disable: bool,
    active: bool,
    language: &'static str,
}

impl Ui {
    fn session(&self) -> MutexGuard<'_, Session> {
        self.session.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn prefs(&self) -> MutexGuard<'_, Prefs> {
        self.prefs.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn language(&self) -> Language {
        Language::new(self.prefs().language, self.system_locale.clone())
    }

    fn strings(&self) -> Strings {
        Strings::new(self.language().language)
    }

    fn save_prefs(&self, prefs: &Prefs) -> Result<(), ErrorBody> {
        let Some(path) = &self.prefs_path else {
            return Ok(());
        };
        prefs.save(path).map_err(|error| {
            tracing::warn!(%error, path = %path.display(), "cannot save the preferences");
            ErrorBody::new("cannot_save", error.to_string())
        })
    }

    /// Follows a status from the service: the tray, and notifications for
    /// what the player must hear about while the window is hidden.
    fn observe(&self, app: &AppHandle, status: &Value) {
        let mode = status["mode"].as_str().unwrap_or("idle");
        let active = matches!(mode, "connecting" | "optimized" | "failing_over");
        let ours = status["owner"].as_str() != Some("another_user");
        let notices = status["notices"].as_array().cloned().unwrap_or_default();
        let fresh: Vec<Value> = {
            let mut session = self.session();
            session.active = active;
            session.ours = ours;
            let ids: HashSet<u64> = notices.iter().filter_map(|n| n["id"].as_u64()).collect();
            let fresh = notices
                .into_iter()
                .filter(|n| {
                    n["id"]
                        .as_u64()
                        .is_some_and(|id| !session.seen_notices.contains(&id))
                })
                .collect();
            session.seen_notices = ids;
            fresh
        };
        self.update_tray(app);
        if !self.hidden.load(Ordering::Relaxed) {
            return;
        }
        let strings = self.strings();
        for notice in fresh {
            let (title, mut body) = match notice["code"].as_str() {
                Some("fell_back") => (
                    strings.tr("notify.fell_back.title"),
                    strings.tr("notify.fell_back.body"),
                ),
                Some("address_changed") => (
                    strings.tr("notify.address_changed.title"),
                    strings.tr("notify.address_changed.body"),
                ),
                _ => continue,
            };
            if let Some(cause) = notice["cause"].as_str() {
                let key = format!("notice.cause.{cause}");
                let text = strings.tr(&key);
                if text != key {
                    body = format!("{body} {text}");
                }
            }
            notify(app, &title, &body);
        }
    }

    /// The service is out of reach: whatever session there was is no
    /// longer something this window can show or turn off.
    fn forget_session(&self, app: &AppHandle) {
        {
            let mut session = self.session();
            session.active = false;
            session.seen_notices.clear();
        }
        self.update_tray(app);
    }

    /// Whether closing the window should hide it instead: while the
    /// player's own session is on, so they can still see and stop it.
    fn keep_running(&self) -> bool {
        let session = self.session();
        self.tray.is_some() && session.active && session.ours
    }

    fn update_tray(&self, app: &AppHandle) {
        let Some(tray) = self.tray.clone() else {
            return;
        };
        let language = self.language().language;
        let hidden = self.hidden.load(Ordering::Relaxed);
        let state = {
            let mut session = self.session();
            let state = TrayState {
                visible: session.active || hidden,
                can_disable: session.active && session.ours,
                active: session.active,
                language,
            };
            if session.tray_shown == Some(state) {
                return;
            }
            session.tray_shown = Some(state);
            state
        };
        let strings = Strings::new(language);
        let tooltip = if state.active {
            strings.tr("tray.tooltip_on")
        } else {
            strings.tr("app.name")
        };
        on_main_thread(app, move || {
            tray.update(state.visible, state.can_disable, &tooltip)
        });
    }
}

/// Runs `f` on the main thread without waiting for it. Tray and menu
/// changes run there anyway; posting them keeps the I/O thread from ever
/// waiting on the main thread.
fn on_main_thread(app: &AppHandle, f: impl FnOnce() + Send + 'static) {
    if let Err(error) = app.run_on_main_thread(f) {
        tracing::warn!(%error, "cannot reach the main thread");
    }
}

fn notify(app: &AppHandle, title: &str, body: &str) {
    if let Err(error) = app.notification().builder().title(title).body(body).show() {
        tracing::warn!(%error, "cannot show a notification");
    }
}

/// Events from the service connection, on its I/O thread.
fn on_event(app: &AppHandle, event: Event) {
    let Some(ui) = app.try_state::<Ui>() else {
        return;
    };
    match event {
        Event::Connection(connection) => {
            let connected = connection.is_connected();
            emit(app, "connection", &connection);
            if connected {
                // Catch up with what happened while disconnected; the page
                // asks for itself too.
                let (app, client) = (app.clone(), ui.client.clone());
                tauri::async_runtime::spawn_blocking(move || {
                    if let Ok(status) = client.call(Request::Status) {
                        if let Some(ui) = app.try_state::<Ui>() {
                            ui.observe(&app, &status);
                        }
                        emit(&app, "status", &status);
                    }
                });
            } else {
                ui.forget_session(app);
            }
        }
        Event::Status(status) => {
            ui.observe(app, &status);
            emit(app, "status", &status);
        }
        Event::Sample(sample) => emit(app, "sample", &sample),
    }
}

fn emit(app: &AppHandle, name: &str, payload: &impl Serialize) {
    if let Err(error) = app.emit(name, payload) {
        tracing::debug!(%error, name, "cannot pass an event to the page");
    }
}

/// The tray menu's items.
pub(crate) fn on_tray_menu(app: &AppHandle, id: &str) {
    match id {
        tray::OPEN => show_window(app),
        tray::DISABLE => {
            let Some(ui) = app.try_state::<Ui>() else {
                return;
            };
            tracing::info!("turning Optimize off from the tray");
            let (app, client) = (app.clone(), ui.client.clone());
            tauri::async_runtime::spawn_blocking(move || match client.call(Request::Disable) {
                Ok(status) => {
                    if let Some(ui) = app.try_state::<Ui>() {
                        ui.observe(&app, &status);
                    }
                    emit(&app, "status", &status);
                }
                Err(error) => {
                    tracing::warn!(
                        code = error.code,
                        message = error.message,
                        "cannot turn Optimize off from the tray"
                    );
                    // Show the window, where the page explains the error.
                    show_window(&app);
                    emit(&app, "action_error", &error);
                }
            });
        }
        tray::QUIT => {
            tracing::info!("quitting from the tray");
            app.exit(0);
        }
        _ => {}
    }
}

/// Brings the window back from the tray.
pub(crate) fn show_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let results = [window.show(), window.unminimize(), window.set_focus()];
        for error in results.into_iter().filter_map(Result::err) {
            tracing::warn!(%error, "cannot show the window");
        }
    }
    if let Some(ui) = app.try_state::<Ui>() {
        ui.hidden.store(false, Ordering::Relaxed);
        ui.update_tray(app);
    }
}

fn on_window_event(window: &tauri::Window, event: &WindowEvent) {
    let WindowEvent::CloseRequested { api, .. } = event else {
        return;
    };
    let app = window.app_handle();
    let Some(ui) = app.try_state::<Ui>() else {
        return;
    };
    if !ui.keep_running() {
        return;
    }
    // Optimize stays on in the service either way; hiding keeps the tray
    // icon, the notifications and "Turn Optimize off" within reach.
    api.prevent_close();
    if let Err(error) = window.hide() {
        tracing::warn!(%error, "cannot hide the window");
        return;
    }
    tracing::info!("window hidden in the tray while Optimize is on");
    ui.hidden.store(true, Ordering::Relaxed);
    ui.update_tray(app);
    let first_time = !std::mem::replace(&mut ui.session().told_about_tray, true);
    if first_time {
        let strings = ui.strings();
        notify(
            app,
            &strings.tr("notify.hidden.title"),
            &strings.tr("notify.hidden.body"),
        );
    }
}

type Reply = Result<Value, ErrorBody>;

/// Sends a request on a blocking thread, logging failures (PLAN.md
/// section 9: every failure a tester can hit leaves a log line).
async fn ask(ui: &Ui, request: Request) -> Reply {
    let method = serde_json::to_value(&request)
        .ok()
        .and_then(|v| v["method"].as_str().map(str::to_string))
        .unwrap_or_default();
    let client = ui.client.clone();
    let reply = tauri::async_runtime::spawn_blocking(move || client.call(request))
        .await
        .unwrap_or_else(|e| Err(ErrorBody::new("failed", e.to_string())));
    if let Err(error) = &reply {
        if error.code == "service_unavailable" {
            tracing::debug!(method, "request while the service is unavailable");
        } else {
            tracing::warn!(
                method,
                code = error.code,
                message = error.message,
                "request failed"
            );
        }
    }
    reply
}

#[derive(Serialize)]
struct AppInfo {
    version: &'static str,
}

#[tauri::command(rename_all = "snake_case")]
fn app_info() -> AppInfo {
    AppInfo { version: VERSION }
}

#[tauri::command(rename_all = "snake_case")]
async fn connection(ui: State<'_, Ui>) -> Result<Connection, ErrorBody> {
    Ok(ui.client.connection())
}

/// The banner's "Try again".
#[tauri::command(rename_all = "snake_case")]
async fn reconnect(ui: State<'_, Ui>) -> Result<(), ErrorBody> {
    ui.client.retry_now();
    Ok(())
}

#[tauri::command(rename_all = "snake_case")]
async fn games(ui: State<'_, Ui>) -> Reply {
    ask(&ui, Request::Games).await
}

#[tauri::command(rename_all = "snake_case")]
async fn status(app: AppHandle, ui: State<'_, Ui>) -> Reply {
    let status = ask(&ui, Request::Status).await?;
    ui.observe(&app, &status);
    Ok(status)
}

#[tauri::command(rename_all = "snake_case")]
async fn measure(ui: State<'_, Ui>, game: String, region: String) -> Reply {
    tracing::info!(game, region, "measuring");
    ask(&ui, Request::Measure { game, region }).await
}

#[tauri::command(rename_all = "snake_case")]
async fn enable(app: AppHandle, ui: State<'_, Ui>, acknowledged_warning: bool) -> Reply {
    tracing::info!(acknowledged_warning, "turning Optimize on");
    let status = ask(
        &ui,
        Request::Enable {
            acknowledged_warning,
        },
    )
    .await?;
    ui.observe(&app, &status);
    Ok(status)
}

#[tauri::command(rename_all = "snake_case")]
async fn disable(app: AppHandle, ui: State<'_, Ui>) -> Reply {
    tracing::info!("turning Optimize off");
    let status = ask(&ui, Request::Disable).await?;
    ui.observe(&app, &status);
    Ok(status)
}

#[tauri::command(rename_all = "snake_case")]
async fn dismiss(ui: State<'_, Ui>, id: u64) -> Reply {
    ask(&ui, Request::Dismiss { id }).await
}

#[tauri::command(rename_all = "snake_case")]
async fn settings(ui: State<'_, Ui>) -> Reply {
    ask(&ui, Request::Settings).await
}

/// Only verbose logging and the improvement thresholds: the protocol has
/// no way to change anything else.
#[tauri::command(rename_all = "snake_case")]
async fn set_settings(ui: State<'_, Ui>, change: SettingsChange) -> Reply {
    tracing::info!(?change, "changing the service's settings");
    ask(&ui, Request::SetSettings(change)).await
}

#[derive(Serialize)]
struct ReportSaved {
    /// Where the report was saved, if it could be.
    path: Option<String>,
    copied: bool,
}

/// "Copy diagnostic report": builds one report from the service's logs and
/// the window's, copies it and saves it to Documents\PingPath.
#[tauri::command(rename_all = "snake_case")]
async fn copy_report(app: AppHandle, ui: State<'_, Ui>) -> Result<ReportSaved, ErrorBody> {
    // Ask the service where its logs are; it may be down, which is often
    // exactly when a report is needed.
    let service_logs = match ask(&ui, Request::LogPaths).await {
        Ok(paths) => paths["service_logs"].as_str().map(PathBuf::from),
        Err(_) => None,
    }
    .unwrap_or_else(report::default_service_logs);
    let documents = app.path().document_dir().map(|d| d.join("PingPath"));
    let (text, saved) = tauri::async_runtime::spawn_blocking(move || {
        let text = report::build(
            &service_logs,
            pingpath_diagnostics::log_dir().as_deref(),
            VERSION,
        );
        let saved = match documents {
            Ok(dir) => report::save(&text, &dir).map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        (text, saved)
    })
    .await
    .map_err(|e| ErrorBody::new("failed", e.to_string()))?;
    let copied = match app.clipboard().write_text(text) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(%error, "cannot copy the diagnostic report");
            false
        }
    };
    let path = match saved {
        Ok(path) => Some(path.display().to_string()),
        Err(error) => {
            tracing::warn!(error, "cannot save the diagnostic report");
            None
        }
    };
    tracing::info!(copied, saved = path.is_some(), "diagnostic report made");
    if !copied && path.is_none() {
        return Err(ErrorBody::new(
            "report_failed",
            "the report could be neither copied nor saved",
        ));
    }
    Ok(ReportSaved { path, copied })
}

#[tauri::command(rename_all = "snake_case")]
async fn get_language(ui: State<'_, Ui>) -> Result<Language, ErrorBody> {
    Ok(ui.language())
}

#[tauri::command(rename_all = "snake_case")]
async fn set_language(
    app: AppHandle,
    ui: State<'_, Ui>,
    preference: LanguagePreference,
) -> Result<Language, ErrorBody> {
    let prefs = {
        let mut prefs = ui.prefs();
        prefs.language = preference;
        prefs.clone()
    };
    let language = ui.language();
    tracing::info!(
        ?preference,
        language = language.language,
        "language changed"
    );
    if let Some(tray) = ui.tray.clone() {
        let strings = Strings::new(language.language);
        on_main_thread(&app, move || tray.set_strings(&strings));
    }
    ui.update_tray(&app);
    ui.save_prefs(&prefs)?;
    Ok(language)
}

#[tauri::command(rename_all = "snake_case")]
async fn acknowledged_games(ui: State<'_, Ui>) -> Result<BTreeSet<String>, ErrorBody> {
    Ok(ui.prefs().acknowledged.clone())
}

/// The player accepted the warning for a game still being tested; asked
/// once per game.
#[tauri::command(rename_all = "snake_case")]
async fn acknowledge_game(ui: State<'_, Ui>, game: String) -> Result<(), ErrorBody> {
    tracing::info!(game, "warning for a game still being tested accepted");
    let prefs = {
        let mut prefs = ui.prefs();
        prefs.acknowledged.insert(game);
        prefs.clone()
    };
    ui.save_prefs(&prefs)
}

fn setup(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let handle = app.handle().clone();
    let prefs_path = match app.path().app_config_dir() {
        Ok(dir) => Some(dir.join("preferences.json")),
        Err(error) => {
            tracing::warn!(%error, "no folder for preferences; they won't be kept");
            None
        }
    };
    let prefs = prefs_path.as_deref().map(Prefs::load).unwrap_or_default();
    let system_locale = prefs::system_locale();
    let language = Language::new(prefs.language, system_locale.clone());
    tracing::info!(
        language = language.language,
        system = system_locale.as_deref().unwrap_or("unknown"),
        "window starting"
    );
    let tray = match tray::Tray::build(&handle, &Strings::new(language.language)) {
        Ok(tray) => Some(tray),
        Err(error) => {
            tracing::warn!(%error, "cannot create the tray icon; closing the window will quit");
            None
        }
    };
    let client = Client::new();
    app.manage(Ui {
        client: client.clone(),
        prefs: Mutex::new(prefs),
        prefs_path,
        system_locale,
        session: Mutex::new(Session::default()),
        hidden: AtomicBool::new(false),
        tray,
    });
    // Connect only now that the state the events need is in place.
    let sink: service::Sink = Arc::new(move |event| on_event(&handle, event));
    client.run(
        Box::new(transport::connect),
        format!("PingPath {VERSION}"),
        sink,
    )?;
    Ok(())
}

fn main() {
    let logs = Mutex::new(Some(pingpath_diagnostics::init("ui", VERSION)));
    let app = tauri::Builder::default()
        // First, as the plugin asks: a second launch (from the Start menu,
        // say, while the window waits in the tray) brings this window back
        // instead of opening another window and tray icon. One per
        // signed-in user: the lock is in the session's own namespace.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            tracing::info!("PingPath was started again; showing this window");
            show_window(app);
        }))
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_notification::init())
        .invoke_handler(tauri::generate_handler![
            app_info,
            connection,
            reconnect,
            games,
            status,
            measure,
            enable,
            disable,
            dismiss,
            settings,
            set_settings,
            copy_report,
            get_language,
            set_language,
            acknowledged_games,
            acknowledge_game,
        ])
        .on_window_event(on_window_event)
        .setup(setup)
        .build(tauri::generate_context!());
    let app = match app {
        Ok(app) => app,
        Err(error) => {
            tracing::error!(%error, "cannot start the window");
            drop(logs);
            std::process::exit(1);
        }
    };
    app.run(move |_, event| {
        if let RunEvent::Exit = event {
            tracing::info!("window closed");
            // The process may end without unwinding; flush the log first.
            drop(logs.lock().unwrap_or_else(|e| e.into_inner()).take());
        }
    });
}
