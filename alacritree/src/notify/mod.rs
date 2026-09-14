//! Desktop notifications for sessions the user isn't looking at, and the way
//! back to the UI thread when one is clicked.
//!
//! Every platform notifier here is synchronous, and the freedesktop one blocks
//! until the toast is dismissed, so posting runs on a throwaway thread.  That
//! thread holds no handle on the app, which is why a click travels back
//! through a static channel instead of a callback: it only ever carries a
//! session id, and the UI drains the channel on its next frame.
//!
//! The backend is per-platform: freedesktop on Unix, WinRT on Windows, the
//! UserNotifications framework on macOS.  Only macOS needs a module of its
//! own, because there a click arrives through a long-lived delegate rather
//! than through the worker thread that posted the notification.

#[cfg(target_os = "macos")]
pub(crate) mod macos;

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, OnceLock};

use crate::repaint::Repaint;
use crate::session::{Session, SessionId};

/// Channel from notification-worker threads back to the app.  Set once by
/// `channel`; each worker reads it to deliver the session the user clicked
/// on.  Static because the worker has no other handle to the app and there's
/// only ever one app instance per process.
static NOTIFY_TX: OnceLock<Mutex<Sender<SessionId>>> = OnceLock::new();

/// Open the click channel, keeping the sending half for the workers and
/// handing the receiving half to the app.
pub(crate) fn channel() -> Receiver<SessionId> {
    let (notify_tx, notify_rx) = mpsc::channel();
    // `set` may fail only if a previous instance already initialized the
    // static (e.g. tests).  In that case the old sender points at a dead
    // app, so overwriting via `Mutex` would be ideal — but since we only
    // ever spawn one app per process, ignoring the error is fine.
    let _ = NOTIFY_TX.set(Mutex::new(notify_tx));
    notify_rx
}

/// Drain every queued notification click, keeping only the newest.  Clicks
/// can pile up while the window is unfocused; the user most likely meant
/// the latest one.
pub(crate) fn latest_click(rx: &Receiver<SessionId>) -> Option<SessionId> {
    let mut latest = None;
    while let Ok(id) = rx.try_recv() {
        latest = Some(id);
    }
    latest
}

/// Spawn a throwaway thread so the platform notifier's synchronous calls
/// don't stall the paint loop.  The thread posts the session's id back
/// through `NOTIFY_TX` when the user clicks the notification.
pub(crate) fn attention(session: &Session<impl Repaint>, repaint: &impl Repaint) {
    let where_label = session
        .working_directory
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| session.title.clone());
    let body = if where_label.is_empty() {
        "Session is waiting for input".to_string()
    } else {
        format!("{where_label} is waiting for input")
    };
    let id = session.id;
    let repaint = repaint.clone();
    std::thread::Builder::new()
        .name("alacritree-notify".into())
        .spawn(move || worker(body, id, repaint))
        .ok();
}

/// Deliver a clicked notification's session id to the UI thread.
pub(crate) fn click(id: SessionId, repaint: &impl Repaint) {
    if let Some(lock) = NOTIFY_TX.get() {
        if let Ok(tx) = lock.lock() {
            let _ = tx.send(id);
            repaint.wake();
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn worker(body: String, id: SessionId, repaint: impl Repaint) {
    // `default` is the action id freedesktop notifiers fire on body-click.
    let result = notify_rust::Notification::new()
        .summary("alacritree")
        .body(&body)
        .action("default", "Open")
        .show();
    let handle = match result {
        Ok(h) => h,
        Err(e) => {
            log::debug!("desktop notification failed: {e}");
            return;
        },
    };
    handle.wait_for_action(|action| {
        if action == "__closed" {
            return;
        }
        click(id, &repaint);
    });
}

#[cfg(windows)]
fn worker(body: String, id: SessionId, repaint: impl Repaint) {
    use tauri_winrt_notification::Toast;
    // notify-rust doesn't surface WinRT activation, so drive its own backend
    // crate directly.  `show` returns immediately; the WinRT runtime holds
    // the activation handler, so this worker thread can exit right away.
    let result = Toast::new(Toast::POWERSHELL_APP_ID)
        .title("alacritree")
        .text1(&body)
        .on_activated(move |_action| {
            click(id, &repaint);
            Ok(())
        })
        .show();
    if let Err(e) = result {
        log::debug!("desktop notification failed: {e}");
    }
}

#[cfg(target_os = "macos")]
fn worker(body: String, id: SessionId, _repaint: impl Repaint) {
    // Clicks come back through the UNUserNotificationCenter delegate that
    // `macos::init` installed, not through this worker.
    macos::notify(&body, id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pile_of_notification_clicks_resolves_to_the_newest() {
        let (tx, rx) = mpsc::channel();
        assert_eq!(latest_click(&rx), None);
        tx.send(3).unwrap();
        tx.send(7).unwrap();
        tx.send(5).unwrap();
        assert_eq!(latest_click(&rx), Some(5));
        // The drain consumed everything, not just the returned click.
        assert_eq!(latest_click(&rx), None);
    }
}
