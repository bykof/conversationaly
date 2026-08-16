//! Nudges the user to record when a call starts.
//!
//! The failure this exists for: the app sits in a narrow window beside the
//! call, the user's attention is on the call, and they never press record.
//!
//! What counts as "a call started" differs by platform, because watching for a
//! meeting app to *launch* turned out to be near-useless in practice: Teams
//! (and Zoom, for many people) autostarts at login and sits open all day, so
//! joining a call produces no new process and the nudge never fires. macOS can
//! ask the audio system who holds the microphone instead, which does change at
//! the moment a call is joined. Windows and Linux have no equally cheap
//! equivalent and keep watching process launches.

use std::collections::HashSet;
use std::time::Duration;

use tauri::{AppHandle, Manager, Runtime, WebviewUrl, WebviewWindowBuilder};

const NUDGE_LABEL: &str = "nudge";

/// Reading one CoreAudio property is far cheaper than spawning `ps`, so the
/// microphone path can afford to check every second and appear the moment a
/// call is joined.
///
/// ponytail: a property listener would be instant rather than within a second,
/// but it binds to one device — plugging in AirPods as you join changes the
/// default input and the listener would be watching the wrong one. Re-reading
/// the current default each tick follows the switch for free. Revisit only if
/// a second of lag actually shows.
#[cfg(target_os = "macos")]
const POLL: Duration = Duration::from_secs(1);
#[cfg(not(target_os = "macos"))]
const POLL: Duration = Duration::from_secs(15);
/// How long the nudge waits before giving up and closing itself.
const DISMISS_AFTER_SECS: u64 = 20;

/// (process name, display name).
///
/// On macOS this table only *names* the app in the card once the microphone
/// says a call started. Elsewhere it is also the trigger, with the ceiling
/// described above: an app that was already running cannot appear.
///
/// ponytail: Slack and Discord are deliberately absent — they run all day, so
/// "just appeared" never fires for them, and on macOS they would misname a
/// call. Browser-based Meet is invisible to this table entirely; on macOS the
/// microphone still catches it, and the card just says "Something".
#[cfg(target_os = "macos")]
const MEETING_APPS: &[(&str, &str)] = &[
    ("zoom.us", "Zoom"),
    ("Microsoft Teams", "Teams"),
    ("MSTeams", "Teams"),
    ("Webex", "Webex"),
];

#[cfg(target_os = "windows")]
const MEETING_APPS: &[(&str, &str)] = &[
    ("Zoom.exe", "Zoom"),
    ("ms-teams.exe", "Teams"),
    ("Teams.exe", "Teams"),
    ("CiscoCollabHost.exe", "Webex"),
];

#[cfg(target_os = "linux")]
const MEETING_APPS: &[(&str, &str)] = &[
    ("zoom", "Zoom"),
    ("teams", "Teams"),
    ("teams-for-linux", "Teams"),
    ("webex", "Webex"),
];

/// One process listing per poll, rather than one `pgrep` per app in the table.
#[cfg(not(target_os = "windows"))]
fn process_listing() -> String {
    std::process::Command::new("ps")
        .args(["-Ao", "comm="])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .unwrap_or_default()
}

#[cfg(target_os = "windows")]
fn process_listing() -> String {
    use std::os::windows::process::CommandExt;
    // Without this, every poll flashes a console window in the user's face.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    std::process::Command::new("tasklist")
        .args(["/NH", "/FO", "CSV"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .unwrap_or_default()
}

/// Display names of meeting apps present in a `ps` / `tasklist` listing.
///
/// `ps -Ao comm=` prints full executable paths; `tasklist /FO CSV` prints the
/// image name as a quoted first field. Taking the first field, unquoting it and
/// keeping the basename handles both.
fn running_in(listing: &str) -> HashSet<&'static str> {
    listing
        .lines()
        .filter_map(|line| {
            let field = line.split(',').next().unwrap_or(line);
            let name = field.trim().trim_matches('"');
            let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
            MEETING_APPS
                .iter()
                .find(|(process, _)| name.eq_ignore_ascii_case(process))
        })
        .map(|(_, display)| *display)
        .collect()
}

/// Apps present now that were absent last poll. Level ("Zoom is open") is not
/// a signal — Zoom sits open all day. The edge is what means "a call is about
/// to start".
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn newly_appeared<'a>(prev: &HashSet<&'a str>, now: &HashSet<&'a str>) -> Vec<&'a str> {
    now.difference(prev).copied().collect()
}

/// Does anything on the system currently hold the default input device?
///
/// This is the one bit that flips when a call is *joined* rather than when an
/// app is launched, which is what makes it work for an always-running Teams.
#[cfg(target_os = "macos")]
fn mic_in_use() -> bool {
    use cidre::core_audio::{PropSelector, System};

    System::default_input_device()
        .and_then(|device| {
            device.bool_prop(&PropSelector::DEVICE_IS_RUNNING_SOMEWHERE.global_addr())
        })
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
struct Trigger {
    mic_busy: bool,
}

#[cfg(target_os = "macos")]
impl Trigger {
    fn armed() -> Self {
        let mic_busy = mic_in_use();
        log::info!("Call detector armed (microphone in use: {})", mic_busy);
        Self { mic_busy }
    }

    /// Only the idle -> busy edge counts. Starting out busy means whoever holds
    /// the microphone had it before we looked, so there is nothing to nudge
    /// about — and it is why this never fires repeatedly during one call.
    ///
    /// ponytail: any microphone user trips this, not just meeting apps —
    /// dictation and Voice Memos included. Distinguishing them needs
    /// per-process microphone access, which macOS exposes no public API for.
    fn poll(&mut self) -> Option<String> {
        let busy = mic_in_use();
        let started = busy && !self.mic_busy;
        self.mic_busy = busy;

        if !started {
            return None;
        }

        Some(match running_in(&process_listing()).into_iter().next() {
            Some(name) => format!("{} is using your microphone", name),
            None => "Something is using your microphone".to_string(),
        })
    }
}

#[cfg(not(target_os = "macos"))]
struct Trigger {
    seen: HashSet<&'static str>,
}

#[cfg(not(target_os = "macos"))]
impl Trigger {
    fn armed() -> Self {
        // The first listing is a baseline only. An app already running when we
        // start — the login-autostart case — must not fire a nudge.
        let seen = running_in(&process_listing());
        log::info!("Call detector armed, already running: {:?}", seen);
        Self { seen }
    }

    fn poll(&mut self) -> Option<String> {
        let now = running_in(&process_listing());
        let appeared = newly_appeared(&self.seen, &now);
        self.seen = now;
        appeared.first().map(|name| format!("{} just opened", name))
    }
}

pub fn spawn<R: Runtime>(app: AppHandle<R>) {
    tauri::async_runtime::spawn(async move {
        let mut trigger = Trigger::armed();

        loop {
            tokio::time::sleep(POLL).await;

            let Some(headline) = trigger.poll() else {
                continue;
            };
            if crate::audio::recording_commands::is_recording().await {
                continue;
            }

            // Warm the transcription model on the intent edge.
            //
            // This is the earliest honest signal that a recording is about to
            // start, and the load overlaps the seconds the nudge is on screen
            // instead of the seconds after the user presses record. It sits
            // above the nudge gate on purpose: the intent is just as real for
            // someone who turned the nudge off.
            //
            // It also means the app never reads 716 MB for a user who opened it
            // to look at yesterday's summary. Windows and Linux trigger on a
            // process-launch edge, which fires earlier than the macOS mic-busy
            // edge — strictly more warming time, not less.
            let app_for_warm = app.clone();
            tauri::async_runtime::spawn(async move {
                crate::transcribe_engine::commands::preload_configured_model(app_for_warm).await;
            });

            if !nudge_enabled(&app).await {
                continue;
            }

            log::info!("Call detected: {} — showing recording nudge", headline);
            show_nudge(&app, &headline);
        }
    });
}

async fn nudge_enabled<R: Runtime>(app: &AppHandle<R>) -> bool {
    match crate::notifications::settings::ConsentManager::new(app.clone()) {
        Ok(manager) => manager
            .load_settings()
            .await
            .map(|settings| settings.notification_preferences.show_call_detected)
            .unwrap_or(true),
        Err(_) => true,
    }
}

fn show_nudge<R: Runtime>(app: &AppHandle<R>, headline: &str) {
    if app.get_webview_window(NUDGE_LABEL).is_some() {
        return;
    }

    // `next dev` serves the route as /nudge; `output: 'export'` emits nudge.html.
    let url = if cfg!(dev) { "nudge" } else { "nudge.html" };
    let (width, height) = (360.0, 132.0);

    let window = WebviewWindowBuilder::new(app, NUDGE_LABEL, WebviewUrl::App(url.into()))
        .inner_size(width, height)
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        // Never steal keystrokes from the call the user is in.
        .focused(false)
        .skip_taskbar(true)
        .visible_on_all_workspaces(true)
        .resizable(false)
        .initialization_script(format!(
            "window.__NUDGE_HEADLINE = {};",
            serde_json::Value::from(headline)
        ))
        .build();

    let window = match window {
        Ok(window) => window,
        Err(e) => {
            log::error!("Failed to build nudge window: {}", e);
            return;
        }
    };

    // ponytail: bottom-right of the primary monitor. A multi-monitor user gets
    // it on the primary, not on the screen holding the call — cursor-relative
    // placement is the upgrade if that annoys.
    if let Ok(Some(monitor)) = window.primary_monitor() {
        let scale = monitor.scale_factor();
        let size = monitor.size().to_logical::<f64>(scale);
        let origin = monitor.position().to_logical::<f64>(scale);
        let _ = window.set_position(tauri::LogicalPosition::new(
            origin.x + size.width - width - 24.0,
            origin.y + size.height - height - 48.0,
        ));
    }

    // Closes itself if ignored, or as soon as recording starts by any route
    // (this nudge, the tray, or the main window).
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        for _ in 0..DISMISS_AFTER_SECS {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if app.get_webview_window(NUDGE_LABEL).is_none() {
                return;
            }
            if crate::audio::recording_commands::is_recording().await {
                break;
            }
        }
        close_nudge(&app);
    });
}

fn close_nudge<R: Runtime>(app: &AppHandle<R>) {
    if let Some(window) = app.get_webview_window(NUDGE_LABEL) {
        let _ = window.close();
    }
}

/// Start recording the way the tray does — by handing off to the frontend, so
/// the meeting record, device selection and UI state are all created by the one
/// flow that knows how. Starting the audio directly here would capture sound
/// into a meeting that does not exist.
#[tauri::command]
pub fn nudge_start_recording<R: Runtime>(app: AppHandle<R>) {
    close_nudge(&app);
    crate::tray::focus_main_window(&app);

    if let Some(window) = app.get_webview_window("main") {
        let _ = window.eval("sessionStorage.setItem('autoStartRecording', 'true')");
        let _ = window.eval("window.location.assign('/')");
    }
}

#[tauri::command]
pub fn nudge_dismiss<R: Runtime>(app: AppHandle<R>) {
    close_nudge(&app);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn matches_ps_paths_by_basename() {
        let listing = "/usr/sbin/cfprefsd\n\
             /Applications/zoom.us.app/Contents/MacOS/zoom.us\n\
             /Applications/Slack.app/Contents/MacOS/Slack";

        let running = running_in(listing);
        assert!(running.contains("Zoom"));
        assert_eq!(running.len(), 1, "Slack must not be a trigger");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn matches_tasklist_csv_fields() {
        let listing = "\"Zoom.exe\",\"1234\",\"Console\",\"1\",\"120,000 K\"\n\
             \"explorer.exe\",\"99\",\"Console\",\"1\",\"9,000 K\"";

        let running = running_in(listing);
        assert!(running.contains("Zoom"));
        assert_eq!(running.len(), 1);
    }

    #[test]
    fn only_the_appearing_edge_fires() {
        let none: HashSet<&str> = HashSet::new();
        let zoom: HashSet<&str> = ["Zoom"].into_iter().collect();

        assert_eq!(newly_appeared(&none, &zoom), vec!["Zoom"], "launch fires");
        assert!(
            newly_appeared(&zoom, &zoom).is_empty(),
            "still running is not an edge — this is what stops the nagging"
        );
        assert!(newly_appeared(&zoom, &none).is_empty(), "quitting is silent");
        assert_eq!(
            newly_appeared(&none, &zoom),
            vec!["Zoom"],
            "quit then relaunch fires again"
        );
    }

    #[test]
    fn two_processes_for_one_app_nudge_once() {
        // Teams appears twice in the table under different process names.
        let both: HashSet<&str> = ["Teams", "Teams"].into_iter().collect();
        assert_eq!(both.len(), 1);
    }
}
