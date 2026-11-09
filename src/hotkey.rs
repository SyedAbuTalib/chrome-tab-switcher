use std::ffi::c_void;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use core_foundation::base::{CFRelease, CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::runloop::{CFRunLoopAddSource, CFRunLoopGetCurrent, CFRunLoopRun, kCFRunLoopCommonModes};
use core_foundation::string::CFString;
use core_foundation_sys::mach_port::{CFMachPortCreateRunLoopSource, CFMachPortRef};
use core_foundation_sys::runloop::CFRunLoopSourceRef;
use crossbeam_channel::Sender;

pub const KEYCODE_GRAVE: u16 = 50;
pub const KEYCODE_ESCAPE: u16 = 53;
pub const KEYCODE_RETURN: u16 = 36;
pub const KEYCODE_KEYPAD_ENTER: u16 = 76;
pub const KEYCODE_DELETE: u16 = 51;
pub const KEYCODE_TAB: u16 = 48;
pub const KEYCODE_LEFT_ARROW: u16 = 123;
pub const KEYCODE_RIGHT_ARROW: u16 = 124;
pub const KEYCODE_UP_ARROW: u16 = 126;
pub const KEYCODE_DOWN_ARROW: u16 = 125;
pub const KEYCODE_ANSI_Q: u16 = 12;
pub const KEYCODE_ANSI_W: u16 = 13;
pub const KEYCODE_ANSI_N: u16 = 45;
pub const KEYCODE_ANSI_P: u16 = 35;
pub const KEYCODE_ANSI_V: u16 = 9;

pub const FLAG_MASK_SHIFT: u64 = 1 << 17;
pub const FLAG_MASK_CONTROL: u64 = 1 << 18;
pub const FLAG_MASK_ALTERNATE: u64 = 1 << 19;
pub const FLAG_MASK_COMMAND: u64 = 1 << 20;

const CG_EVENT_LEFT_MOUSE_DOWN: u32 = 1;
const CG_EVENT_RIGHT_MOUSE_DOWN: u32 = 3;
const CG_EVENT_KEY_DOWN: u32 = 10;
const CG_EVENT_KEY_UP: u32 = 11;
const CG_EVENT_FLAGS_CHANGED: u32 = 12;
const CG_EVENT_OTHER_MOUSE_DOWN: u32 = 25;
const CG_EVENT_TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
const CG_EVENT_TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;

const CG_SESSION_EVENT_TAP: u32 = 1;
const CG_HEAD_INSERT_EVENT_TAP: u32 = 0;
const CG_EVENT_TAP_OPTION_DEFAULT: u32 = 0;
const KEYBOARD_EVENT_KEYCODE_FIELD: u32 = 9;

type CGEventRef = *mut c_void;
type CGEventTapProxy = *mut c_void;

type CGEventTapCallBack = extern "C" fn(
    proxy: CGEventTapProxy,
    event_type: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrustedWithOptions(options: CFTypeRef) -> bool;

    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: CGEventTapCallBack,
        user_info: *mut c_void,
    ) -> CFMachPortRef;

    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGEventGetFlags(event: CGEventRef) -> u64;
    fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    fn CGEventKeyboardGetUnicodeString(
        event: CGEventRef,
        max_string_length: libc::c_ulong,
        actual_string_length: *mut libc::c_ulong,
        unicode_string: *mut u16,
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyEvent {
    CycleNext,
    CyclePrev,
    OpenLatchedSearch,
    ModifierReleased,
    CharInput(char),
    Backspace,
    /// `⌥⌫` — delete the trailing word of the query (macOS `deleteWordBackward:`).
    BackspaceWord,
    /// `⌘⌫` — clear the entire query (macOS `deleteToBeginningOfLine:`).
    BackspaceLine,
    /// `⌘V` — paste the general pasteboard into the query. The tap only signals intent;
    /// the clipboard is read on the GPUI thread in `SwitcherView::dispatch_hotkey`.
    Paste,
    NavigateLeft,
    NavigateRight,
    NavigateUp,
    NavigateDown,
    Confirm,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapDecision {
    pub intercept: bool,
    pub emit: Option<HotkeyEvent>,
}

/// Check macOS Accessibility trust state, optionally prompting the user to grant permission.
pub fn ensure_accessibility_permission(prompt: bool) -> bool {
    unsafe {
        let key = CFString::new("AXTrustedCheckOptionPrompt");
        let value = if prompt {
            CFBoolean::true_value()
        } else {
            CFBoolean::false_value()
        };
        let dict = CFDictionary::from_CFType_pairs(&[(key.as_CFType(), value.as_CFType())]);
        AXIsProcessTrustedWithOptions(dict.as_CFTypeRef())
    }
}

/// Pure, unit-testable key classification state step for the `CGEventTap` callback.
pub fn classify_key_event(
    keycode: u16,
    flags: u64,
    is_key_down: bool,
    is_flags_changed: bool,
    cmd_was_down: &mut bool,
    switcher_visible: bool,
    unicode_char: Option<char>,
) -> TapDecision {
    let cmd_is_down = (flags & FLAG_MASK_COMMAND) != 0;
    let shift_is_down = (flags & FLAG_MASK_SHIFT) != 0;
    let ctrl_is_down = (flags & FLAG_MASK_CONTROL) != 0;
    let alt_is_down = (flags & FLAG_MASK_ALTERNATE) != 0;

    if is_flags_changed {
        let released_cmd = *cmd_was_down && !cmd_is_down;
        *cmd_was_down = cmd_is_down;
        if released_cmd && switcher_visible {
            return TapDecision {
                intercept: false,
                emit: Some(HotkeyEvent::ModifierReleased),
            };
        }
        return TapDecision {
            intercept: false,
            emit: None,
        };
    }

    *cmd_was_down = cmd_is_down;

    // Global shortcut: Cmd + ` or Cmd + Shift + `
    if keycode == KEYCODE_GRAVE && cmd_is_down && !ctrl_is_down && !alt_is_down {
        if is_key_down {
            let evt = if shift_is_down {
                HotkeyEvent::CyclePrev
            } else {
                HotkeyEvent::CycleNext
            };
            return TapDecision {
                intercept: true,
                emit: Some(evt),
            };
        } else {
            // Absorb KeyUp of ` when Cmd is held as well
            return TapDecision {
                intercept: true,
                emit: None,
            };
        }
    }

    // Global shortcut: Option + ` to open directly in LatchedSearch mode
    if keycode == KEYCODE_GRAVE && alt_is_down && !cmd_is_down && !ctrl_is_down {
        if is_key_down {
            return TapDecision {
                intercept: true,
                emit: Some(HotkeyEvent::OpenLatchedSearch),
            };
        } else {
            return TapDecision {
                intercept: true,
                emit: None,
            };
        }
    }

    if !switcher_visible {
        return TapDecision {
            intercept: false,
            emit: None,
        };
    }

    // While switcher is visible, let Cmd+Q / Cmd+W pass through untouched
    if cmd_is_down && (keycode == KEYCODE_ANSI_Q || keycode == KEYCODE_ANSI_W) {
        return TapDecision {
            intercept: false,
            emit: None,
        };
    }

    // Absorb KeyUp events while switcher is visible so the underlying app never sees stray key-ups
    if !is_key_down {
        return TapDecision {
            intercept: true,
            emit: None,
        };
    }

    match keycode {
        KEYCODE_ESCAPE => TapDecision {
            intercept: true,
            emit: Some(HotkeyEvent::Cancel),
        },
        KEYCODE_RETURN | KEYCODE_KEYPAD_ENTER => TapDecision {
            intercept: true,
            emit: Some(HotkeyEvent::Confirm),
        },
        KEYCODE_LEFT_ARROW => TapDecision {
            intercept: true,
            emit: Some(HotkeyEvent::NavigateLeft),
        },
        KEYCODE_RIGHT_ARROW => TapDecision {
            intercept: true,
            emit: Some(HotkeyEvent::NavigateRight),
        },
        KEYCODE_UP_ARROW => TapDecision {
            intercept: true,
            emit: Some(HotkeyEvent::NavigateUp),
        },
        KEYCODE_DOWN_ARROW => TapDecision {
            intercept: true,
            emit: Some(HotkeyEvent::NavigateDown),
        },
        KEYCODE_TAB if !cmd_is_down => TapDecision {
            intercept: true,
            emit: Some(if shift_is_down {
                HotkeyEvent::NavigateLeft
            } else {
                HotkeyEvent::NavigateRight
            }),
        },
        // Modifier-aware backspace mirrors the Cocoa text system: ⌘⌫ clears the line,
        // ⌥⌫ deletes a word, bare ⌫ deletes one character.
        KEYCODE_DELETE => TapDecision {
            intercept: true,
            emit: Some(if cmd_is_down {
                HotkeyEvent::BackspaceLine
            } else if alt_is_down {
                HotkeyEvent::BackspaceWord
            } else {
                HotkeyEvent::Backspace
            }),
        },
        KEYCODE_ANSI_V if cmd_is_down && !ctrl_is_down && !alt_is_down => TapDecision {
            intercept: true,
            emit: Some(HotkeyEvent::Paste),
        },
        KEYCODE_ANSI_N if ctrl_is_down => TapDecision {
            intercept: true,
            emit: Some(HotkeyEvent::NavigateDown),
        },
        KEYCODE_ANSI_P if ctrl_is_down => TapDecision {
            intercept: true,
            emit: Some(HotkeyEvent::NavigateUp),
        },
        _ => {
            if let Some(ch) = unicode_char.or_else(|| ansi_keycode_to_char(keycode, shift_is_down))
            {
                if !ch.is_control() && !ctrl_is_down && !alt_is_down {
                    return TapDecision {
                        intercept: true,
                        emit: Some(HotkeyEvent::CharInput(ch)),
                    };
                }
            }
            TapDecision {
                intercept: true,
                emit: None,
            }
        }
    }
}

fn ansi_keycode_to_char(keycode: u16, shift: bool) -> Option<char> {
    let base = match keycode {
        0 => 'a',
        1 => 's',
        2 => 'd',
        3 => 'f',
        4 => 'h',
        5 => 'g',
        6 => 'z',
        7 => 'x',
        8 => 'c',
        9 => 'v',
        11 => 'b',
        12 => 'q',
        13 => 'w',
        14 => 'e',
        15 => 'r',
        16 => 'y',
        17 => 't',
        18 => '1',
        19 => '2',
        20 => '3',
        21 => '4',
        22 => '6',
        23 => '5',
        24 => '=',
        25 => '9',
        26 => '7',
        27 => '-',
        28 => '8',
        29 => '0',
        31 => 'o',
        32 => 'u',
        34 => 'i',
        35 => 'p',
        37 => 'l',
        38 => 'j',
        40 => 'k',
        45 => 'n',
        46 => 'm',
        47 => '.',
        44 => '/',
        49 => ' ',
        _ => return None,
    };
    Some(if shift {
        base.to_ascii_uppercase()
    } else {
        base
    })
}

struct TapContext {
    tx: Sender<HotkeyEvent>,
    switcher_visible: Arc<AtomicBool>,
    cmd_was_down: bool,
    tap_port: CFMachPortRef,
}

unsafe impl Send for TapContext {}

extern "C" fn event_tap_callback(
    _proxy: CGEventTapProxy,
    event_type: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef {
    if user_info.is_null() {
        return event;
    }

    let ctx = unsafe { &mut *(user_info as *mut TapContext) };

    if event_type == CG_EVENT_TAP_DISABLED_BY_TIMEOUT
        || event_type == CG_EVENT_TAP_DISABLED_BY_USER_INPUT
    {
        if !ctx.tap_port.is_null() {
            unsafe {
                CGEventTapEnable(ctx.tap_port, true);
            }
        }
        return event;
    }

    let is_mouse_down = event_type == CG_EVENT_LEFT_MOUSE_DOWN
        || event_type == CG_EVENT_RIGHT_MOUSE_DOWN
        || event_type == CG_EVENT_OTHER_MOUSE_DOWN;
    if is_mouse_down {
        if ctx.switcher_visible.load(Ordering::Acquire)
            && !crate::macos::is_mouse_inside_floating_panel()
        {
            ctx.switcher_visible.store(false, Ordering::Release);
            let _ = ctx.tx.try_send(HotkeyEvent::Cancel);
        }
        return event;
    }

    let is_key_down = event_type == CG_EVENT_KEY_DOWN;
    let is_key_up = event_type == CG_EVENT_KEY_UP;
    let is_flags_changed = event_type == CG_EVENT_FLAGS_CHANGED;

    if !is_key_down && !is_key_up && !is_flags_changed {
        return event;
    }

    let flags = unsafe { CGEventGetFlags(event) };
    let keycode =
        unsafe { CGEventGetIntegerValueField(event, KEYBOARD_EVENT_KEYCODE_FIELD) as u16 };
    let unicode_char = if is_key_down {
        extract_event_unicode_char(event)
    } else {
        None
    };
    let visible = ctx.switcher_visible.load(Ordering::Acquire);

    let decision = classify_key_event(
        keycode,
        flags,
        is_key_down,
        is_flags_changed,
        &mut ctx.cmd_was_down,
        visible,
        unicode_char,
    );

    if let Some(hotkey_event) = decision.emit {
        let _ = ctx.tx.try_send(hotkey_event);
    }

    if decision.intercept {
        ptr::null_mut()
    } else {
        event
    }
}

fn extract_event_unicode_char(event: CGEventRef) -> Option<char> {
    let mut buf = [0u16; 4];
    let mut actual_len: libc::c_ulong = 0;
    unsafe {
        CGEventKeyboardGetUnicodeString(
            event,
            buf.len() as libc::c_ulong,
            &mut actual_len,
            buf.as_mut_ptr(),
        );
    }
    if actual_len == 0 {
        return None;
    }
    let slice = &buf[..(actual_len as usize).min(buf.len())];
    std::char::decode_utf16(slice.iter().copied())
        .next()
        .and_then(|r| r.ok())
}

pub fn spawn_hotkey_tap_thread(
    tx: Sender<HotkeyEvent>,
    switcher_visible: Arc<AtomicBool>,
) {
    std::thread::Builder::new()
        .name("cgeventtap-runloop".into())
        .spawn(move || unsafe {
            let ctx = Box::into_raw(Box::new(TapContext {
                tx,
                switcher_visible,
                cmd_was_down: false,
                tap_port: ptr::null_mut(),
            }));

            let event_mask: u64 = (1u64 << CG_EVENT_KEY_DOWN)
                | (1u64 << CG_EVENT_KEY_UP)
                | (1u64 << CG_EVENT_FLAGS_CHANGED)
                | (1u64 << CG_EVENT_LEFT_MOUSE_DOWN)
                | (1u64 << CG_EVENT_RIGHT_MOUSE_DOWN)
                | (1u64 << CG_EVENT_OTHER_MOUSE_DOWN);

            let mut logged_waiting = false;
            let tap_port = loop {
                let port = CGEventTapCreate(
                    CG_SESSION_EVENT_TAP,
                    CG_HEAD_INSERT_EVENT_TAP,
                    CG_EVENT_TAP_OPTION_DEFAULT,
                    event_mask,
                    event_tap_callback,
                    ctx as *mut c_void,
                );
                if !port.is_null() {
                    break port;
                }
                if !logged_waiting {
                    log::warn!(
                        "Waiting for Accessibility permission to create CGEventTap (grant permission to your terminal or chrome-tab-switcher in System Settings -> Privacy & Security -> Accessibility)..."
                    );
                    logged_waiting = true;
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            };

            log::info!("Global CGEventTap active (Cmd + ` and Option + ` ready)");
            (*ctx).tap_port = tap_port;

            let runloop_source: CFRunLoopSourceRef =
                CFMachPortCreateRunLoopSource(ptr::null(), tap_port, 0);
            if runloop_source.is_null() {
                log::error!("CFMachPortCreateRunLoopSource returned null");
                CFRelease(tap_port as CFTypeRef);
                let _ = Box::from_raw(ctx);
                return;
            }

            let current_loop = CFRunLoopGetCurrent();
            CFRunLoopAddSource(current_loop, runloop_source, kCFRunLoopCommonModes);
            CGEventTapEnable(tap_port, true);

            CFRunLoopRun();
        })
        .expect("failed to spawn cgeventtap-runloop thread");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intercepts_cmd_backtick_and_cmd_shift_backtick_globally() {
        let mut cmd_down = true;
        let d1 = classify_key_event(
            KEYCODE_GRAVE,
            FLAG_MASK_COMMAND,
            true,
            false,
            &mut cmd_down,
            false,
            Some('`'),
        );
        assert_eq!(
            d1,
            TapDecision {
                intercept: true,
                emit: Some(HotkeyEvent::CycleNext),
            }
        );

        let d2 = classify_key_event(
            KEYCODE_GRAVE,
            FLAG_MASK_COMMAND | FLAG_MASK_SHIFT,
            true,
            false,
            &mut cmd_down,
            true,
            Some('~'),
        );
        assert_eq!(
            d2,
            TapDecision {
                intercept: true,
                emit: Some(HotkeyEvent::CyclePrev),
            }
        );

        let d3 = classify_key_event(
            KEYCODE_GRAVE,
            FLAG_MASK_ALTERNATE,
            true,
            false,
            &mut cmd_down,
            false,
            Some('`'),
        );
        assert_eq!(
            d3,
            TapDecision {
                intercept: true,
                emit: Some(HotkeyEvent::OpenLatchedSearch),
            }
        );
    }

    #[test]
    fn detects_cmd_release_via_flags_changed_when_switcher_visible() {
        let mut cmd_down = false;
        // Press Command
        let d_press =
            classify_key_event(55, FLAG_MASK_COMMAND, false, true, &mut cmd_down, true, None);
        assert!(cmd_down);
        assert_eq!(
            d_press,
            TapDecision {
                intercept: false,
                emit: None
            }
        );

        // Release Command while switcher is visible
        let d_release = classify_key_event(55, 0, false, true, &mut cmd_down, true, None);
        assert!(!cmd_down);
        assert_eq!(
            d_release,
            TapDecision {
                intercept: false,
                emit: Some(HotkeyEvent::ModifierReleased)
            }
        );
    }

    #[test]
    fn absorbs_typing_and_navigation_keys_only_when_switcher_visible() {
        let mut cmd_down = false;

        // When hidden, normal typing passes through untouched
        let hidden_char = classify_key_event(4, 0, true, false, &mut cmd_down, false, Some('h'));
        assert_eq!(
            hidden_char,
            TapDecision {
                intercept: false,
                emit: None
            }
        );

        // When visible, typing even with Command still held (except Cmd+Q/Cmd+W) latches search
        let visible_char_with_cmd = classify_key_event(
            5,
            FLAG_MASK_COMMAND,
            true,
            false,
            &mut cmd_down,
            true,
            Some('g'),
        );
        assert_eq!(
            visible_char_with_cmd,
            TapDecision {
                intercept: true,
                emit: Some(HotkeyEvent::CharInput('g'))
            }
        );

        // Cmd+Q is not absorbed as a search character
        let cmd_q = classify_key_event(
            KEYCODE_ANSI_Q,
            FLAG_MASK_COMMAND,
            true,
            false,
            &mut cmd_down,
            true,
            Some('q'),
        );
        assert_eq!(
            cmd_q,
            TapDecision {
                intercept: false,
                emit: None
            }
        );

        // Navigation & confirm/cancel keys when visible
        assert_eq!(
            classify_key_event(KEYCODE_DOWN_ARROW, 0, true, false, &mut cmd_down, true, None),
            TapDecision {
                intercept: true,
                emit: Some(HotkeyEvent::NavigateDown)
            }
        );
        assert_eq!(
            classify_key_event(
                KEYCODE_ANSI_N,
                FLAG_MASK_CONTROL,
                true,
                false,
                &mut cmd_down,
                true,
                None
            ),
            TapDecision {
                intercept: true,
                emit: Some(HotkeyEvent::NavigateDown)
            }
        );
        assert_eq!(
            classify_key_event(KEYCODE_RETURN, 0, true, false, &mut cmd_down, true, Some('\r')),
            TapDecision {
                intercept: true,
                emit: Some(HotkeyEvent::Confirm)
            }
        );
        assert_eq!(
            classify_key_event(KEYCODE_ESCAPE, 0, true, false, &mut cmd_down, true, None),
            TapDecision {
                intercept: true,
                emit: Some(HotkeyEvent::Cancel)
            }
        );
    }

    #[test]
    fn routes_modified_backspace_and_cmd_v_to_text_editing_events() {
        let mut cmd_down = false;
        let intercepted = |emit: HotkeyEvent| TapDecision {
            intercept: true,
            emit: Some(emit),
        };

        // Bare ⌫ still deletes a single character
        assert_eq!(
            classify_key_event(KEYCODE_DELETE, 0, true, false, &mut cmd_down, true, None),
            intercepted(HotkeyEvent::Backspace)
        );
        // ⌥⌫ deletes a word, ⌘⌫ clears the line (⌘ wins if both are held)
        assert_eq!(
            classify_key_event(
                KEYCODE_DELETE,
                FLAG_MASK_ALTERNATE,
                true,
                false,
                &mut cmd_down,
                true,
                None
            ),
            intercepted(HotkeyEvent::BackspaceWord)
        );
        assert_eq!(
            classify_key_event(
                KEYCODE_DELETE,
                FLAG_MASK_COMMAND,
                true,
                false,
                &mut cmd_down,
                true,
                None
            ),
            intercepted(HotkeyEvent::BackspaceLine)
        );
        assert_eq!(
            classify_key_event(
                KEYCODE_DELETE,
                FLAG_MASK_COMMAND | FLAG_MASK_ALTERNATE,
                true,
                false,
                &mut cmd_down,
                true,
                None
            ),
            intercepted(HotkeyEvent::BackspaceLine)
        );

        // ⌘V pastes; a plain `v` still types into the query
        assert_eq!(
            classify_key_event(
                KEYCODE_ANSI_V,
                FLAG_MASK_COMMAND,
                true,
                false,
                &mut cmd_down,
                true,
                Some('v')
            ),
            intercepted(HotkeyEvent::Paste)
        );
        assert_eq!(
            classify_key_event(
                KEYCODE_ANSI_V,
                0,
                true,
                false,
                &mut cmd_down,
                true,
                Some('v')
            ),
            intercepted(HotkeyEvent::CharInput('v'))
        );

        // When the switcher is hidden, ⌘V and ⌘⌫ pass through to the frontmost app untouched
        for flags in [FLAG_MASK_COMMAND, FLAG_MASK_ALTERNATE] {
            assert_eq!(
                classify_key_event(
                    KEYCODE_DELETE,
                    flags,
                    true,
                    false,
                    &mut cmd_down,
                    false,
                    None
                ),
                TapDecision {
                    intercept: false,
                    emit: None
                }
            );
        }
        assert_eq!(
            classify_key_event(
                KEYCODE_ANSI_V,
                FLAG_MASK_COMMAND,
                true,
                false,
                &mut cmd_down,
                false,
                Some('v')
            ),
            TapDecision {
                intercept: false,
                emit: None
            }
        );
    }
}
