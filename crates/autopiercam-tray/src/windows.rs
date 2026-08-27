use std::{path::PathBuf, process::Command};

use tao::{
    event::{Event, StartCause},
    event_loop::{ControlFlow, EventLoopBuilder},
};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use tray_icon::{
    Icon, TrayIconBuilder,
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};

use crate::worker::{TrayCommand, WorkerEvent, WorkerStatus, start_placeholder_worker};

const VIEWER_FILE_NAME: &str = "AutoPierCam.Viewer.exe";
const VIEWER_DEV_RELATIVE_PATH: &str = concat!(
    "apps/AutoPierCam.Viewer/bin/Debug/",
    "net10.0-windows10.0.26100.0/win-x64/AutoPierCam.Viewer.exe"
);

#[derive(Debug)]
enum UserEvent {
    Menu(MenuEvent),
    Worker(WorkerEvent),
}

pub(crate) fn run() {
    init_tracing();

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let menu_proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = menu_proxy.send_event(UserEvent::Menu(event));
    }));

    let worker_proxy = event_loop.create_proxy();
    let worker = match start_placeholder_worker(move |event| {
        let _ = worker_proxy.send_event(UserEvent::Worker(event));
    }) {
        Ok(worker) => worker,
        Err(error) => {
            error!(%error, "failed to start the placeholder capture worker");
            return;
        }
    };

    let menu = Menu::new();
    let open_viewer = MenuItem::new("Open viewer", true, None);
    let pause_capture = CheckMenuItem::new("Pause capture", true, false, None);
    let capture_now = MenuItem::new("Capture now", true, None);
    let quit = MenuItem::new("Quit", true, None);
    if let Err(error) = menu.append_items(&[
        &open_viewer,
        &pause_capture,
        &capture_now,
        &PredefinedMenuItem::separator(),
        &quit,
    ]) {
        error!(%error, "failed to construct the tray menu");
        let _ = worker.send(TrayCommand::Shutdown);
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
                    .with_icon(generated_icon())
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
                tray.take();
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    });
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

fn apply_worker_status(
    status: &WorkerStatus,
    pause_capture: &CheckMenuItem,
    tray: &Option<tray_icon::TrayIcon>,
    worker_paused: &mut bool,
    status_summary: &mut String,
) {
    info!(paused = status.paused, status = %status.summary, "capture worker status changed");
    *worker_paused = status.paused;
    pause_capture.set_checked(status.paused);
    status_summary.clone_from(&status.summary);
    set_tooltip(tray, &tray_tooltip(status_summary));
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

fn viewer_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::with_capacity(2);
    match std::env::current_exe() {
        Ok(tray_executable) => candidates.push(tray_executable.with_file_name(VIEWER_FILE_NAME)),
        Err(error) => warn!(%error, "could not locate the tray executable"),
    }

    let repository_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");
    candidates.push(repository_root.join(VIEWER_DEV_RELATIVE_PATH));
    candidates
}

fn generated_icon() -> Icon {
    const SIZE: u32 = 32;
    let mut rgba = vec![0_u8; (SIZE * SIZE * 4) as usize];

    for y in 0..SIZE {
        for x in 0..SIZE {
            let pixel = ((y * SIZE + x) * 4) as usize;
            let camera_body = (4..=27).contains(&x) && (8..=24).contains(&y);
            let camera_top = (9..=16).contains(&x) && (5..=8).contains(&y);
            let lens_x = x as i32 - 16;
            let lens_y = y as i32 - 16;
            let lens_radius_squared = lens_x * lens_x + lens_y * lens_y;

            let color = if lens_radius_squared <= 36 {
                [226, 241, 255, 255]
            } else if lens_radius_squared <= 64 {
                [16, 55, 102, 255]
            } else if camera_body || camera_top {
                [32, 104, 184, 255]
            } else {
                [0, 0, 0, 0]
            };
            rgba[pixel..pixel + 4].copy_from_slice(&color);
        }
    }

    Icon::from_rgba(rgba, SIZE, SIZE).expect("the generated tray icon has valid RGBA dimensions")
}
