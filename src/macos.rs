#![allow(unexpected_cfgs)]

use std::ffi::CStr;

use cocoa::appkit::{
    NSApp, NSApplication, NSApplicationActivationPolicyAccessory, NSScreen, NSWindow,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use cocoa::base::{BOOL, NO, YES, id, nil};
use cocoa::foundation::{NSArray, NSPoint, NSRect, NSSize, NSString};
use objc::{class, msg_send, sel, sel_impl};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreenRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Center horizontally on the given `ScreenRect` and position the top edge at 22% from screen top.
pub fn compute_panel_frame_in_screen(
    screen: ScreenRect,
    panel_width: f64,
    panel_height: f64,
) -> ScreenRect {
    let x = screen.x + ((screen.width - panel_width) * 0.5).max(0.0);
    let top_offset = screen.height * 0.22;
    let y = screen.y + (screen.height - top_offset - panel_height).max(24.0);
    ScreenRect {
        x,
        y,
        width: panel_width,
        height: panel_height,
    }
}

pub fn is_chrome_bundle_id(bundle_id: &str) -> bool {
    bundle_id.starts_with("com.google.Chrome")
}

/// Configure the process as an Accessory app (`LSUIElement` equivalent): no Dock icon, never steals menu bar.
pub fn set_accessory_activation_policy() {
    unsafe {
        let app = NSApp();
        if app != nil {
            app.setActivationPolicy_(NSApplicationActivationPolicyAccessory);
        }
    }
}

/// Configure our GPUI floating panel so it behaves as a non-activating overlay across all Spaces and Fullscreen apps.
pub fn configure_floating_panel() {
    unsafe {
        if let Some(window) = primary_panel_window() {
            let current_mask: NSWindowStyleMask = window.styleMask();
            let non_activating = NSWindowStyleMask::from_bits_retain(1 << 7);
            window.setStyleMask_(current_mask | non_activating);
            window.setLevel_(101); // NSPopUpMenuWindowLevel
            window.setOpaque_(NO);
            window.setHasShadow_(NO);
            window.setCollectionBehavior_(
                NSWindowCollectionBehavior::NSWindowCollectionBehaviorCanJoinAllSpaces
                    | NSWindowCollectionBehavior::NSWindowCollectionBehaviorFullScreenAuxiliary
                    | NSWindowCollectionBehavior::NSWindowCollectionBehaviorIgnoresCycle,
            );
            let clear_color: id = msg_send![class!(NSColor), clearColor];
            window.setBackgroundColor_(clear_color);
        }
    }
}

/// Hide the floating panel immediately without destroying the GPUI window or Metal context.
pub fn hide_floating_panel() {
    unsafe {
        if let Some(window) = primary_panel_window() {
            let _: () = msg_send![window, setAlphaValue: 0.0f64];
            let _: () = msg_send![window, orderOut: nil];
        }
    }
}

/// Position the floating panel on the screen containing the mouse cursor and order it front immediately.
pub fn show_floating_panel(panel_width: f64, panel_height: f64) {
    unsafe {
        if let Some(window) = primary_panel_window() {
            let screen_rect = active_mouse_screen_rect();
            let target = compute_panel_frame_in_screen(screen_rect, panel_width, panel_height);
            let ns_rect = NSRect::new(
                NSPoint::new(target.x, target.y),
                NSSize::new(target.width, target.height),
            );
            let _: () = msg_send![window, setFrame: ns_rect display: YES];
            let _: () = msg_send![window, setAlphaValue: 1.0f64];
            let _: () = msg_send![window, orderFrontRegardless];
        }
    }
}

/// Resize the visible floating panel dynamically when the filtered row count changes.
pub fn update_floating_panel_size(panel_width: f64, panel_height: f64) {
    unsafe {
        if let Some(window) = primary_panel_window() {
            let screen_rect = active_mouse_screen_rect();
            let target = compute_panel_frame_in_screen(screen_rect, panel_width, panel_height);
            let ns_rect = NSRect::new(
                NSPoint::new(target.x, target.y),
                NSSize::new(target.width, target.height),
            );
            let _: () = msg_send![window, setFrame: ns_rect display: YES];
        }
    }
}

/// Check whether Google Chrome is currently the frontmost macOS application.
pub fn is_chrome_frontmost() -> bool {
    unsafe {
        let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];
        if workspace == nil {
            return false;
        }
        let front_app: id = msg_send![workspace, frontmostApplication];
        if front_app == nil {
            return false;
        }
        let bundle_ns: id = msg_send![front_app, bundleIdentifier];
        if bundle_ns == nil {
            return false;
        }
        let utf8_ptr: *const libc::c_char = msg_send![bundle_ns, UTF8String];
        if utf8_ptr.is_null() {
            return false;
        }
        let bundle_id = CStr::from_ptr(utf8_ptr).to_string_lossy();
        is_chrome_bundle_id(&bundle_id)
    }
}

/// Bring Google Chrome to the foreground immediately via `NSRunningApplication`.
pub fn activate_chrome_application() -> bool {
    const CHROME_BUNDLE_IDS: &[&str] = &[
        "com.google.Chrome",
        "com.google.Chrome.canary",
        "com.google.Chrome.dev",
        "com.google.Chrome.beta",
    ];
    // NSApplicationActivateIgnoringOtherApps = 1 << 1
    const ACTIVATE_IGNORING_OTHER_APPS: usize = 1 << 1;

    unsafe {
        for bundle_id in CHROME_BUNDLE_IDS {
            let ns_bundle = NSString::alloc(nil).init_str(bundle_id);
            let apps: id = msg_send![
                class!(NSRunningApplication),
                runningApplicationsWithBundleIdentifier: ns_bundle
            ];
            if apps != nil {
                let count: u64 = NSArray::count(apps);
                for i in 0..count {
                    let app: id = NSArray::objectAtIndex(apps, i);
                    if app != nil {
                        let ok: BOOL = msg_send![
                            app,
                            activateWithOptions: ACTIVATE_IGNORING_OTHER_APPS
                        ];
                        if ok == YES {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

unsafe fn primary_panel_window() -> Option<id> {
    unsafe {
        let app = NSApp();
        if app == nil {
            return None;
        }
        let windows: id = msg_send![app, windows];
        if windows == nil || NSArray::count(windows) == 0 {
            return None;
        }
        let win = NSArray::objectAtIndex(windows, 0);
        if win == nil { None } else { Some(win) }
    }
}

unsafe fn active_mouse_screen_rect() -> ScreenRect {
    unsafe {
        let mouse_loc: NSPoint = msg_send![class!(NSEvent), mouseLocation];
        let screens = NSScreen::screens(nil);
        if screens != nil {
            let count = NSArray::count(screens);
            for i in 0..count {
                let screen = NSArray::objectAtIndex(screens, i);
                let frame = NSScreen::frame(screen);
                let within_x =
                    mouse_loc.x >= frame.origin.x && mouse_loc.x <= frame.origin.x + frame.size.width;
                let within_y =
                    mouse_loc.y >= frame.origin.y && mouse_loc.y <= frame.origin.y + frame.size.height;
                if within_x && within_y {
                    return ScreenRect {
                        x: frame.origin.x,
                        y: frame.origin.y,
                        width: frame.size.width,
                        height: frame.size.height,
                    };
                }
            }
        }

        let main = NSScreen::mainScreen(nil);
        if main != nil {
            let frame = NSScreen::frame(main);
            return ScreenRect {
                x: frame.origin.x,
                y: frame.origin.y,
                width: frame.size.width,
                height: frame.size.height,
            };
        }

        ScreenRect {
            x: 0.0,
            y: 0.0,
            width: 1440.0,
            height: 900.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centers_panel_horizontally_and_places_top_at_22_percent_from_screen_top() {
        let screen = ScreenRect {
            x: 0.0,
            y: 0.0,
            width: 1728.0,
            height: 1117.0,
        };
        let frame = compute_panel_frame_in_screen(screen, 680.0, 400.0);
        assert_eq!(frame.width, 680.0);
        assert_eq!(frame.height, 400.0);
        assert!((frame.x - 524.0).abs() < 1e-6);
        // AppKit y is from bottom: 1117 - (1117 * 0.22) - 400 = 871.26 - 400 = 471.26
        assert!((frame.y - 471.26).abs() < 1e-4);
    }

    #[test]
    fn matches_chrome_bundle_identifiers() {
        assert!(is_chrome_bundle_id("com.google.Chrome"));
        assert!(is_chrome_bundle_id("com.google.Chrome.canary"));
        assert!(!is_chrome_bundle_id("com.apple.Safari"));
    }
}
