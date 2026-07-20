//! UserNotifications-framework delivery for macOS.
//!
//! notify-rust's `NSUserNotification` backend is defunct on modern macOS:
//! delivery from an unbundled process silently fails (the API still reports
//! success) and clicks are never surfaced.  The modern framework fixes both
//! and shows the system permission prompt on first use — but it throws
//! `NSInternalInconsistencyException` when the process has no bundle, so
//! everything here is gated on `NSBundle` reporting an identifier.  Running
//! the bare `target/release` binary therefore disables notifications; the
//! binary must live inside `Alacritree.app` (see `extra/osx`), though it can
//! still be launched from a terminal via `Alacritree.app/Contents/MacOS`.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, ProtocolObject};
use objc2::{AnyThread, define_class, msg_send};
use objc2_foundation::{
    NSBundle, NSDictionary, NSError, NSNumber, NSObject, NSObjectProtocol, NSString,
};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotification,
    UNNotificationDefaultActionIdentifier, UNNotificationPresentationOptions,
    UNNotificationRequest, UNNotificationResponse, UNNotificationSound, UNUserNotificationCenter,
    UNUserNotificationCenterDelegate,
};

use crate::session::SessionId;

const SESSION_ID_KEY: &str = "alacritree_session_id";

/// `None` when the process is unbundled: the framework must not be touched at
/// all in that case, not even to construct notification content.
static STATE: OnceLock<Option<egui::Context>> = OnceLock::new();

/// Distinct request identifiers so a later ping doesn't silently replace an
/// earlier one the user hasn't seen yet.
static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(0);

define_class!(
    // SAFETY: NSObject has no subclassing requirements and the type holds no
    // state, so there is nothing to drop.
    #[unsafe(super(NSObject))]
    #[name = "AlacritreeNotifyDelegate"]
    struct NotifyDelegate;

    unsafe impl NSObjectProtocol for NotifyDelegate {}

    unsafe impl UNUserNotificationCenterDelegate for NotifyDelegate {
        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            completion: &block2::DynBlock<dyn Fn()>,
        ) {
            handle_response(response);
            completion.call(());
        }

        // Without this the framework suppresses banners while the app is
        // frontmost — but a ping from a session the user isn't looking at
        // must show even when the window is focused.
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            completion: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            completion.call((UNNotificationPresentationOptions::Banner
                | UNNotificationPresentationOptions::Sound,));
        }
    }
);

impl NotifyDelegate {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

fn handle_response(response: &UNNotificationResponse) {
    let default_action = unsafe { UNNotificationDefaultActionIdentifier };
    if &*response.actionIdentifier() != default_action {
        return;
    }
    let info = response.notification().request().content().userInfo();
    let key = NSString::from_str(SESSION_ID_KEY);
    let key: &AnyObject = key.as_ref();
    let Some(value) = info.objectForKey(key) else { return };
    let Some(id) = value.downcast_ref::<NSNumber>().map(|n| n.as_u64()) else { return };
    if let Some(Some(ctx)) = STATE.get() {
        crate::app::notify_click(id as SessionId, ctx);
    }
}

/// Install the click delegate and request notification permission.  Must run
/// on the main thread before the first notification; safe to call when
/// unbundled (delivery is disabled with a warning instead of the framework's
/// exception).
pub fn init(ctx: egui::Context) {
    if NSBundle::mainBundle().bundleIdentifier().is_none() {
        log::warn!(
            "desktop notifications disabled: not running from an app bundle \
             (assemble one with alacritree/extra/osx/make-app.sh)"
        );
        let _ = STATE.set(None);
        return;
    }
    let center = UNUserNotificationCenter::currentNotificationCenter();
    let delegate = NotifyDelegate::new();
    center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    // The center holds the delegate weakly; leak our reference so it lives
    // for the rest of the process.
    let _ = Retained::into_raw(delegate);
    let completion = RcBlock::new(|granted: Bool, _error: *mut NSError| {
        if !granted.as_bool() {
            log::warn!("desktop notifications denied in System Settings");
        }
    });
    center.requestAuthorizationWithOptions_completionHandler(
        UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
        &completion,
    );
    let _ = STATE.set(Some(ctx));
}

/// Post one attention toast.  Thread-safe; a no-op until `init` ran with a
/// bundle identifier present.
pub fn notify(body: &str, id: SessionId) {
    if !matches!(STATE.get(), Some(Some(_))) {
        return;
    }
    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str("alacritree"));
    content.setBody(&NSString::from_str(body));
    content.setSound(Some(&UNNotificationSound::defaultSound()));
    let key = NSString::from_str(SESSION_ID_KEY);
    let value = NSNumber::new_u64(id);
    let value: &AnyObject = value.as_ref();
    let info = NSDictionary::from_slices(&[&*key], &[value]);
    // SAFETY: widening the key type NSString → AnyObject is sound (the
    // dictionary only ever hands keys back out), and the single NSString →
    // NSNumber pair is plist-representable as the setter requires.
    unsafe {
        let info: Retained<NSDictionary> = Retained::cast_unchecked(info);
        content.setUserInfo(&info);
    }

    let n = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let ident = NSString::from_str(&format!("attention-{id}-{n}"));
    let request =
        UNNotificationRequest::requestWithIdentifier_content_trigger(&ident, &content, None);
    let completion = RcBlock::new(|error: *mut NSError| {
        if !error.is_null() {
            log::debug!("desktop notification failed: {}", unsafe { &*error });
        }
    });
    UNUserNotificationCenter::currentNotificationCenter()
        .addNotificationRequest_withCompletionHandler(&request, Some(&completion));
}
