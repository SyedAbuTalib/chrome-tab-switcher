#![allow(unexpected_cfgs)]

use std::ffi::CStr;
use std::io::Cursor;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use cocoa::appkit::{
    NSApp, NSApplication, NSApplicationActivationPolicyAccessory, NSScreen, NSStatusBar,
    NSVariableStatusItemLength, NSWindow, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use cocoa::base::{BOOL, NO, YES, id, nil};
use cocoa::foundation::{NSArray, NSPoint, NSRect, NSSize, NSString};
use crossbeam_channel::Sender;
use image::{ImageBuffer, Rgba};
use objc::declare::ClassDecl;
use objc::runtime::{Class, Object, Sel};
use objc::{class, msg_send, sel, sel_impl};
use parking_lot::RwLock;

use crate::hotkey::HotkeyEvent;

static STATUS_BAR_HOTKEY_TX: OnceLock<Sender<HotkeyEvent>> = OnceLock::new();
static PANEL_WINDOW_PTR: AtomicUsize = AtomicUsize::new(0);
static VISIBLE_PANEL_FRAME: RwLock<Option<ScreenRect>> = RwLock::new(None);
static ACTIVE_CARD_HEIGHT: AtomicUsize = AtomicUsize::new(492);

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

/// Signed distance to an axis-aligned rounded rectangle centered at `((x0+x1)/2, (y0+y1)/2)`.
fn sd_rounded_box(px: f32, py: f32, x0: f32, y0: f32, x1: f32, y1: f32, radius: f32) -> f32 {
    let cx = (x0 + x1) * 0.5;
    let cy = (y0 + y1) * 0.5;
    let hx = (x1 - x0) * 0.5 - radius;
    let hy = (y1 - y0) * 0.5 - radius;
    let dx = (px - cx).abs() - hx;
    let dy = (py - cy).abs() - hy;
    let outside = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt();
    let inside = dx.max(dy).min(0.0);
    outside + inside - radius
}

/// Generate a crisp 36×36 (@2x Retina for 18×18pt) monochrome template PNG of the
/// "Layered Tab Cards (AltTab Style)" icon for the macOS menu bar.
pub fn build_layered_tabs_menubar_png() -> Vec<u8> {
    let width = 36u32;
    let height = 36u32;
    let mut img: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::new(width, height);

    for y in 0..height {
        for x in 0..width {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;

            // Front browser card: [3.0..26.0] x [11.0..32.0], corner radius 4.2, stroke 3.0px
            let d_front = sd_rounded_box(px, py, 3.0, 11.0, 26.0, 32.0, 4.2);
            let front_stroke_dist = d_front.abs() - 1.5;

            // Header bar line inside front card at y = 17.6
            let header_line_dist = if d_front <= 0.5 {
                (py - 17.6).abs() - 1.25
            } else {
                100.0
            };

            // Two window dots inside the front card's top header strip
            let dot1_dist = ((px - 8.0).powi(2) + (py - 14.3).powi(2)).sqrt() - 1.35;
            let dot2_dist = ((px - 12.2).powi(2) + (py - 14.3).powi(2)).sqrt() - 1.35;

            // Back overlapping card: [11.0..33.0] x [3.0..23.0], masked with a crisp halo around the front card
            let d_back = sd_rounded_box(px, py, 11.0, 3.0, 33.0, 23.0, 4.2);
            let back_stroke_dist = if d_front > 2.2 {
                d_back.abs() - 1.45
            } else {
                100.0
            };

            let min_dist = front_stroke_dist
                .min(header_line_dist)
                .min(dot1_dist)
                .min(dot2_dist)
                .min(back_stroke_dist);

            // Smooth 1px anti-aliased edge coverage
            let coverage = (0.5 - min_dist).clamp(0.0, 1.0);
            let alpha = (coverage * 255.0).round() as u8;
            img.put_pixel(x, y, Rgba([0, 0, 0, alpha]));
        }
    }

    let mut png_bytes = Vec::new();
    img.write_to(
        &mut Cursor::new(&mut png_bytes),
        image::ImageFormat::Png,
    )
    .expect("failed to encode menu bar icon PNG");
    png_bytes
}

extern "C" fn on_menu_switch_tabs(_this: &Object, _cmd: Sel, _sender: id) {
    if let Some(tx) = STATUS_BAR_HOTKEY_TX.get() {
        let _ = tx.try_send(HotkeyEvent::OpenLatchedSearch);
    }
}

extern "C" fn on_menu_open_accessibility(_this: &Object, _cmd: Sel, _sender: id) {
    let _ = std::process::Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
        .spawn();
}

fn status_menu_handler_class() -> &'static Class {
    static CLASS: OnceLock<&'static Class> = OnceLock::new();
    CLASS.get_or_init(|| unsafe {
        let superclass = class!(NSObject);
        let mut decl = ClassDecl::new("CTSStatusMenuHandler", superclass)
            .expect("failed to declare CTSStatusMenuHandler");
        decl.add_method(
            sel!(onSwitchTabs:),
            on_menu_switch_tabs as extern "C" fn(&Object, Sel, id),
        );
        decl.add_method(
            sel!(onOpenAccessibility:),
            on_menu_open_accessibility as extern "C" fn(&Object, Sel, id),
        );
        decl.register()
    })
}

/// Install the persistent macOS menu bar icon (`NSStatusItem`) with the Layered Tab Cards template icon
/// and native dropdown menu (similar to Rectangle / AltTab).
pub fn install_status_bar_item(hotkey_tx: Sender<HotkeyEvent>) {
    let _ = STATUS_BAR_HOTKEY_TX.set(hotkey_tx);

    unsafe {
        let status_bar = NSStatusBar::systemStatusBar(nil);
        let status_item = status_bar.statusItemWithLength_(NSVariableStatusItemLength);
        if status_item == nil {
            return;
        }
        // Retain the status item so it stays alive in the menu bar for the lifetime of the daemon
        let _: id = msg_send![status_item, retain];

        let png_bytes = build_layered_tabs_menubar_png();
        let ns_data: id = msg_send![
            class!(NSData),
            dataWithBytes: png_bytes.as_ptr() as *const libc::c_void
            length: png_bytes.len()
        ];
        if ns_data != nil {
            let ns_image: id = msg_send![class!(NSImage), alloc];
            let ns_image: id = msg_send![ns_image, initWithData: ns_data];
            if ns_image != nil {
                let _: () = msg_send![ns_image, setSize: NSSize::new(18.0, 18.0)];
                let _: () = msg_send![ns_image, setTemplate: YES];
                let button: id = msg_send![status_item, button];
                if button != nil {
                    let _: () = msg_send![button, setImage: ns_image];
                }
            }
        }

        let handler_cls = status_menu_handler_class();
        let handler: id = msg_send![handler_cls, new];

        let menu: id = msg_send![class!(NSMenu), alloc];
        let menu_title = NSString::alloc(nil).init_str("Chrome Tab Switcher");
        let menu: id = msg_send![menu, initWithTitle: menu_title];

        // 1. Header Status Item (disabled label)
        let header_item: id = msg_send![class!(NSMenuItem), alloc];
        let header_title =
            NSString::alloc(nil).init_str("Chrome Tab Switcher · Active (⌘`)");
        let empty_key = NSString::alloc(nil).init_str("");
        let header_item: id = msg_send![
            header_item,
            initWithTitle: header_title
            action: nil
            keyEquivalent: empty_key
        ];
        let _: () = msg_send![header_item, setEnabled: NO];
        let _: () = msg_send![menu, addItem: header_item];

        let sep1: id = msg_send![class!(NSMenuItem), separatorItem];
        let _: () = msg_send![menu, addItem: sep1];

        // 2. Open Tab Switcher
        let open_item: id = msg_send![class!(NSMenuItem), alloc];
        let open_title = NSString::alloc(nil).init_str("Open Tab Switcher (⌘` / ⌥`)");
        let open_item: id = msg_send![
            open_item,
            initWithTitle: open_title
            action: sel!(onSwitchTabs:)
            keyEquivalent: empty_key
        ];
        let _: () = msg_send![open_item, setTarget: handler];
        let _: () = msg_send![menu, addItem: open_item];

        // 3. Accessibility Settings
        let access_item: id = msg_send![class!(NSMenuItem), alloc];
        let access_title = NSString::alloc(nil).init_str("Accessibility Settings…");
        let access_item: id = msg_send![
            access_item,
            initWithTitle: access_title
            action: sel!(onOpenAccessibility:)
            keyEquivalent: empty_key
        ];
        let _: () = msg_send![access_item, setTarget: handler];
        let _: () = msg_send![menu, addItem: access_item];

        let sep2: id = msg_send![class!(NSMenuItem), separatorItem];
        let _: () = msg_send![menu, addItem: sep2];

        // 4. Quit Chrome Tab Switcher
        let quit_item: id = msg_send![class!(NSMenuItem), alloc];
        let quit_title = NSString::alloc(nil).init_str("Quit Chrome Tab Switcher");
        let quit_key = NSString::alloc(nil).init_str("q");
        let quit_item: id = msg_send![
            quit_item,
            initWithTitle: quit_title
            action: sel!(terminate:)
            keyEquivalent: quit_key
        ];
        let _: () = msg_send![quit_item, setTarget: NSApp()];
        let _: () = msg_send![menu, addItem: quit_item];

        let _: () = msg_send![status_item, setMenu: menu];
    }
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
    *VISIBLE_PANEL_FRAME.write() = None;
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
            *VISIBLE_PANEL_FRAME.write() = Some(target);
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
            *VISIBLE_PANEL_FRAME.write() = Some(target);
            let ns_rect = NSRect::new(
                NSPoint::new(target.x, target.y),
                NSSize::new(target.width, target.height),
            );
            let _: () = msg_send![window, setFrame: ns_rect display: YES];
        }
    }
}

/// Record the rendered height of the visible switcher card inside the GPUI window so clicks in
/// empty space below a shorter card are also treated as outside clicks.
pub fn set_active_card_height(card_height: f64) {
    ACTIVE_CARD_HEIGHT.store(card_height.round().max(1.0) as usize, Ordering::Release);
}

/// Compute the AppKit screen rectangle of the top-anchored switcher card within `window_rect`.
pub fn active_card_rect_in_window(window_rect: ScreenRect, card_height: f64) -> ScreenRect {
    let clamped_h = card_height.clamp(1.0, window_rect.height);
    let top = window_rect.y + window_rect.height;
    ScreenRect {
        x: window_rect.x,
        y: top - clamped_h,
        width: window_rect.width,
        height: clamped_h,
    }
}

pub fn is_point_in_rect(x: f64, y: f64, rect: ScreenRect) -> bool {
    x >= rect.x && x <= rect.x + rect.width && y >= rect.y && y <= rect.y + rect.height
}

/// Returns `true` if the current global mouse location is inside the visible switcher card.
pub fn is_mouse_inside_floating_panel() -> bool {
    let Some(window_rect) = *VISIBLE_PANEL_FRAME.read() else {
        return false;
    };
    let card_h = ACTIVE_CARD_HEIGHT.load(Ordering::Acquire) as f64;
    let card_rect = active_card_rect_in_window(window_rect, card_h);
    unsafe {
        let mouse_loc: NSPoint = msg_send![class!(NSEvent), mouseLocation];
        is_point_in_rect(mouse_loc.x, mouse_loc.y, card_rect)
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
    let cached = PANEL_WINDOW_PTR.load(Ordering::Acquire);
    if cached != 0 {
        return Some(cached as id);
    }

    unsafe {
        let app = NSApp();
        if app == nil {
            return None;
        }
        let windows: id = msg_send![app, windows];
        if windows == nil {
            return None;
        }
        let count = NSArray::count(windows);
        for i in 0..count {
            let win = NSArray::objectAtIndex(windows, i);
            if win == nil {
                continue;
            }
            // Skip tiny system windows such as NSStatusBarWindow (which is ~24px wide)
            let frame: NSRect = msg_send![win, frame];
            if frame.size.width >= 400.0 {
                PANEL_WINDOW_PTR.store(win as usize, Ordering::Release);
                return Some(win);
            }
        }
        None
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

    #[test]
    fn generates_valid_retina_layered_tabs_menubar_png() {
        let png = build_layered_tabs_menubar_png();
        let decoded = image::load_from_memory(&png).expect("should decode generated PNG");
        assert_eq!(decoded.width(), 36);
        assert_eq!(decoded.height(), 36);
        let rgba = decoded.into_rgba8();
        // Corner pixel (0, 0) must be transparent
        assert_eq!(rgba.get_pixel(0, 0)[3], 0);
        // Some pixels along the front card border must be opaque
        let has_opaque = rgba.pixels().any(|p| p[3] > 200);
        assert!(has_opaque);
    }

    #[test]
    fn computes_top_anchored_card_bounds_for_outside_click_detection() {
        let win = ScreenRect {
            x: 300.0,
            y: 400.0,
            width: 864.0,
            height: 492.0,
        };
        // When card height is shorter (e.g. 276px for 1 row), it hangs down from top (400 + 492 = 892)
        let card = active_card_rect_in_window(win, 276.0);
        assert_eq!(card.x, 300.0);
        assert_eq!(card.y, 892.0 - 276.0);
        assert_eq!(card.height, 276.0);

        // Point inside the top card is inside
        assert!(is_point_in_rect(500.0, 750.0, card));
        // Point in the empty space below the 1-row card (e.g. y = 500) is outside
        assert!(!is_point_in_rect(500.0, 500.0, card));
        // Point to the left of the panel is outside
        assert!(!is_point_in_rect(100.0, 750.0, card));
    }
}
