use std::{
    path::{Path, PathBuf},
    process::Command,
};

use autopiercam_core::ConfigStore;
use autopiercam_protocol::{AgentState, AgentStatus};
use tao::{
    event::{Event, StartCause},
    event_loop::{ControlFlow, EventLoopBuilder},
};
use tracing::{error, info, warn};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{EnvFilter, fmt::writer::MakeWriterExt};
use tray_icon::{
    Icon, TrayIconBuilder,
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};

use crate::{
    Options,
    ipc::ControlServer,
    preview::PreviewServer,
    worker::{TrayCommand, WorkerEvent, WorkerOptions, start_capture_worker},
};

const VIEWER_FILE_NAME: &str = "AutoPierCam.Viewer.exe";
#[cfg(debug_assertions)]
const VIEWER_DEV_RELATIVE_PATH: &str = concat!(
    "apps/AutoPierCam.Viewer/bin/Debug/",
    "net10.0-windows10.0.26100.0/win-x64/AutoPierCam.Viewer.exe"
);

#[derive(Debug)]
enum UserEvent {
    Menu(MenuEvent),
    Worker(WorkerEvent),
}

pub(crate) fn run(options: Options) {
    init_tracing(&options.config);

    let config_store = match ConfigStore::open(&options.config) {
        Ok(store) => store,
        Err(error) => {
            error!(path = %options.config.display(), %error, "failed to open managed configuration");
            return;
        }
    };

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let menu_proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = menu_proxy.send_event(UserEvent::Menu(event));
    }));

    let worker_proxy = event_loop.create_proxy();
    let worker_options = WorkerOptions {
        config_path: config_store.path().to_path_buf(),
        sdk_path: options.sdk,
    };
    let worker = match start_capture_worker(worker_options, move |event| {
        let _ = worker_proxy.send_event(UserEvent::Worker(event));
    }) {
        Ok(worker) => worker,
        Err(error) => {
            error!(%error, "failed to start the capture worker supervisor");
            return;
        }
    };
    let agent_monitor = worker.monitor();
    let menu_config_store = config_store.clone();
    let mut control_server = match ControlServer::start(worker.clone(), agent_monitor, config_store)
    {
        Ok(server) => Some(server),
        Err(error) => {
            error!(%error, "failed to start the local control pipe");
            // Failure often means another tray instance owns the well-known
            // first pipe. Do not leave a second camera owner running headless.
            if let Err(shutdown_error) = worker.shutdown_and_join() {
                error!(%shutdown_error, "failed to stop the capture worker after pipe startup failure");
            }
            return;
        }
    };
    let mut preview_server = match PreviewServer::start(worker.preview()) {
        Ok(server) => Some(server),
        Err(error) => {
            error!(%error, "failed to start the local preview pipe");
            if let Some(mut server) = control_server.take()
                && let Err(stop_error) = server.stop_and_join()
            {
                error!(%stop_error, "failed to stop the local control pipe after preview startup failure");
            }
            if let Err(shutdown_error) = worker.shutdown_and_join() {
                error!(%shutdown_error, "failed to stop the capture worker after preview startup failure");
            }
            return;
        }
    };

    let menu = Menu::new();
    let open_viewer = MenuItem::new("Open viewer", true, None);
    let open_captures = MenuItem::new("Open captures", true, None);
    let open_logs = MenuItem::new("Open logs", true, None);
    let pause_capture = CheckMenuItem::new("Pause capture", true, false, None);
    let capture_now = MenuItem::new("Capture now", true, None);
    let quit = MenuItem::new("Quit", true, None);
    if let Err(error) = menu.append_items(&[
        &open_viewer,
        &open_captures,
        &open_logs,
        &PredefinedMenuItem::separator(),
        &pause_capture,
        &capture_now,
        &PredefinedMenuItem::separator(),
        &quit,
    ]) {
        error!(%error, "failed to construct the tray menu");
        if let Some(mut server) = preview_server.take()
            && let Err(stop_error) = server.stop_and_join()
        {
            error!(%stop_error, "failed to stop the local preview pipe after menu failure");
        }
        if let Some(mut server) = control_server.take()
            && let Err(stop_error) = server.stop_and_join()
        {
            error!(%stop_error, "failed to stop the local control pipe after menu failure");
        }
        if let Err(shutdown_error) = worker.shutdown_and_join() {
            error!(%shutdown_error, "failed to stop the capture worker after menu failure");
        }
        return;
    }

    let mut tray = None;
    let mut quitting = false;
    let mut worker_paused = false;
    let mut status_summary = "Starting capture worker".to_owned();

    info!("AutoPierCam tray event loop starting");
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::NewEvents(StartCause::Init) => {
                let tooltip = tray_tooltip(&status_summary);
                match TrayIconBuilder::new()
                    .with_menu(Box::new(menu.clone()))
                    .with_tooltip(tooltip)
                    .with_icon(application_icon())
                    .build()
                {
                    Ok(icon) => tray = Some(icon),
                    Err(error) => {
                        error!(%error, "failed to create the Windows notification icon");
                        quitting = true;
                        let _ = worker.send(TrayCommand::Shutdown);
                    }
                }
            }
            Event::UserEvent(UserEvent::Menu(event))
                if event.id == open_viewer.id() && !quitting =>
            {
                launch_viewer();
            }
            Event::UserEvent(UserEvent::Menu(event))
                if event.id == open_captures.id() && !quitting =>
            {
                open_capture_directory(&menu_config_store);
            }
            Event::UserEvent(UserEvent::Menu(event)) if event.id == open_logs.id() && !quitting => {
                open_log_directory(&menu_config_store);
            }
            Event::UserEvent(UserEvent::Menu(event))
                if event.id == pause_capture.id() && !quitting =>
            {
                // muda updates CheckMenuItem before it emits MenuEvent.
                let requested = pause_capture.is_checked();
                if let Err(error) = worker.send(TrayCommand::SetPaused(requested)) {
                    error!(%error, "failed to send pause state to the capture worker");
                    pause_capture.set_checked(worker_paused);
                }
            }
            Event::UserEvent(UserEvent::Menu(event))
                if event.id == capture_now.id() && !quitting =>
            {
                if let Err(error) = worker.send(TrayCommand::CaptureNow) {
                    error!(%error, "failed to request an immediate capture");
                }
            }
            Event::UserEvent(UserEvent::Menu(event)) if event.id == quit.id() => {
                if !quitting {
                    quitting = true;
                    open_viewer.set_enabled(false);
                    open_captures.set_enabled(false);
                    open_logs.set_enabled(false);
                    pause_capture.set_enabled(false);
                    capture_now.set_enabled(false);
                    quit.set_enabled(false);
                    set_tooltip(&tray, "AutoPierCam — stopping");

                    if let Err(error) = worker.send(TrayCommand::Shutdown) {
                        // A disconnected worker has already emitted, or is about to emit,
                        // WorkerStopped. Waiting for that event preserves ordered shutdown.
                        error!(%error, "capture worker command channel is disconnected");
                    }
                }
            }
            Event::UserEvent(UserEvent::Worker(WorkerEvent::StatusChanged(status))) => {
                apply_worker_status(
                    &status,
                    &pause_capture,
                    &tray,
                    &mut worker_paused,
                    &mut status_summary,
                );
            }
            Event::UserEvent(UserEvent::Worker(WorkerEvent::WorkerStopped)) => {
                info!("capture worker stopped; exiting tray process");
                if let Some(mut server) = preview_server.take()
                    && let Err(error) = server.stop_and_join()
                {
                    error!(%error, "failed to stop the local preview pipe cleanly");
                }
                if let Some(mut server) = control_server.take()
                    && let Err(error) = server.stop_and_join()
                {
                    error!(%error, "failed to stop the local control pipe cleanly");
                }
                if let Err(error) = worker.join() {
                    error!(%error, "failed to join the capture supervisor cleanly");
                }
                tray.take();
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    });
}

fn init_tracing(config_path: &Path) {
    let log_directory = config_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join("logs");
    let file_appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("autopiercam")
        .filename_suffix("log")
        .max_log_files(14)
        .build(&log_directory);

    match file_appender {
        Ok(file_appender) => {
            let writer = std::io::stderr.and(file_appender);
            let _ = tracing_subscriber::fmt()
                .with_env_filter(default_log_filter())
                .with_target(false)
                .with_ansi(false)
                .with_writer(writer)
                .try_init();
            info!(directory = %log_directory.display(), "file logging initialized");
        }
        Err(error) => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(default_log_filter())
                .with_target(false)
                .with_writer(std::io::stderr)
                .try_init();
            error!(directory = %log_directory.display(), %error, "file logging is unavailable");
        }
    }
}

fn default_log_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())
}

fn apply_worker_status(
    status: &AgentStatus,
    pause_capture: &CheckMenuItem,
    tray: &Option<tray_icon::TrayIcon>,
    worker_paused: &mut bool,
    status_summary: &mut String,
) {
    let paused = status.state == AgentState::Paused;
    let summary = status_summary_text(status);
    info!(paused, state = ?status.state, status = %summary, "capture worker status changed");
    *worker_paused = paused;
    pause_capture.set_checked(paused);
    status_summary.clone_from(&summary);
    set_tooltip(tray, &tray_tooltip(status_summary));
}

fn status_summary_text(status: &AgentStatus) -> String {
    let camera = status
        .camera
        .as_ref()
        .map(|camera| camera.name.as_str())
        .unwrap_or("no camera");
    match status.state {
        AgentState::Starting => format!("starting — {camera}"),
        AgentState::Idle => format!("idle — {camera}"),
        AgentState::Capturing => format!("capturing — {camera}"),
        AgentState::Paused => format!("paused — {camera}"),
        AgentState::Stopping => "stopping".to_owned(),
        AgentState::Faulted => status
            .last_error
            .as_deref()
            .map(compact_tooltip_error)
            .unwrap_or_else(|| "capture fault".to_owned()),
    }
}

fn compact_tooltip_error(error: &str) -> String {
    let compact = error.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= 80 {
        return compact;
    }
    compact.chars().take(79).collect::<String>() + "…"
}

fn set_tooltip(tray: &Option<tray_icon::TrayIcon>, tooltip: &str) {
    if let Some(tray) = tray
        && let Err(error) = tray.set_tooltip(Some(tooltip))
    {
        warn!(%error, "failed to update tray tooltip");
    }
}

fn tray_tooltip(status: &str) -> String {
    format!("AutoPierCam — {status}")
}

fn launch_viewer() {
    let candidates = viewer_candidates();
    let Some(viewer) = candidates.iter().find(|candidate| candidate.is_file()) else {
        let searched = candidates
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join("; ");
        error!(%searched, "AutoPierCam viewer executable was not found; build the WinUI project first");
        return;
    };

    match Command::new(viewer).spawn() {
        Ok(child) => {
            info!(path = %viewer.display(), pid = child.id(), "launched AutoPierCam viewer")
        }
        Err(error) => {
            error!(path = %viewer.display(), %error, "failed to launch AutoPierCam viewer")
        }
    }
}

fn open_capture_directory(config_store: &ConfigStore) {
    let snapshot = match config_store.snapshot() {
        Ok(snapshot) => snapshot,
        Err(error) => {
            error!(%error, "could not read the capture directory from configuration");
            return;
        }
    };
    let directory = resolve_configured_directory(
        config_store.path(),
        snapshot.config.capture.directory.as_path(),
    );
    launch_directory("capture", &directory);
}

fn open_log_directory(config_store: &ConfigStore) {
    let directory = config_store
        .path()
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("logs");
    launch_directory("log", &directory);
}

fn resolve_configured_directory(config_path: &Path, configured: &Path) -> PathBuf {
    if configured.is_absolute() {
        configured.to_path_buf()
    } else {
        config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(configured)
    }
}

fn launch_directory(label: &'static str, directory: &Path) {
    if let Err(error) = std::fs::create_dir_all(directory) {
        error!(kind = label, path = %directory.display(), %error, "could not create directory before opening it");
        return;
    }
    match Command::new("explorer.exe").arg(directory).spawn() {
        Ok(child) => {
            info!(kind = label, path = %directory.display(), pid = child.id(), "opened directory")
        }
        Err(error) => {
            error!(kind = label, path = %directory.display(), %error, "could not open directory")
        }
    }
}

fn viewer_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::with_capacity(2);
    match std::env::current_exe() {
        Ok(tray_executable) => {
            candidates.push(tray_executable.with_file_name(VIEWER_FILE_NAME));
            candidates.push(
                tray_executable
                    .with_file_name("Viewer")
                    .join(VIEWER_FILE_NAME),
            );
        }
        Err(error) => warn!(%error, "could not locate the tray executable"),
    }

    #[cfg(debug_assertions)]
    {
        let repository_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..");
        candidates.push(repository_root.join(VIEWER_DEV_RELATIVE_PATH));
    }
    candidates
}

fn application_icon() -> Icon {
    Icon::from_resource(1, Some((32, 32)))
        .expect("the AutoPierCam tray icon resource should be embedded in this executable")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_application_icon_is_loadable() {
        drop(application_icon());
    }

    #[test]
    fn relative_capture_directory_is_resolved_beside_configuration() {
        assert_eq!(
            resolve_configured_directory(
                Path::new(r"C:\Users\Alice\AppData\Local\AutoPierCam\autopiercam.toml"),
                Path::new("captures"),
            ),
            PathBuf::from(r"C:\Users\Alice\AppData\Local\AutoPierCam\captures")
        );
    }

    #[test]
    fn absolute_capture_directory_is_preserved() {
        assert_eq!(
            resolve_configured_directory(
                Path::new(r"C:\Users\Alice\AppData\Local\AutoPierCam\autopiercam.toml"),
                Path::new(r"D:\Pier Captures"),
            ),
            PathBuf::from(r"D:\Pier Captures")
        );
    }
}
