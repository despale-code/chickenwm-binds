//! Phase 4: Low-Level Hotkey & Mouse Hook Daemon (`chickenwm-binds`)
//! - 4.1: Dedicated `THREAD_PRIORITY_TIME_CRITICAL` Win32 message loop with `WH_KEYBOARD_LL` & `WH_MOUSE_LL`.
//! - 4.2: Modifier & Key Chord State Machine (`SUPER`, `CTRL`, `ALT`, `SHIFT`), consuming native `Win+*` shortcuts,
//!        supporting `bind`, `bindr`, `binde`, and `bindm`.
//! - 4.3: Throttled Mouse Resize/Move Stream paced by the Phase 3 Adaptive Frame Scheduler (`AdaptiveFrameScheduler`)
//!        so 1000Hz–8000Hz mice never flood `SetWindowPos`.

// Release builds are windowed apps: no console window when launched from the shell, a
// hotkey or the launcher. Debug builds keep the console for logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod cheatsheet;

use anyhow::Result;
use crossbeam_channel::{unbounded, Sender};
use chickenwm_core::bootstrap_config_and_themes;
use chickenwm_core::config::{load_or_init_config, BindKind, BindRule, HyprConfig};
use chickenwm_core::css::{spawn_config_watcher, ConfigWatchEvent};
use chickenwm_core::failsafe::{spawn_emergency_failsafe_thread, trigger_emergency_restore};
use chickenwm_core::frame_scheduler::{AdaptiveFrameScheduler, FramePacerControl};
use chickenwm_core::handles::SafeHhook;
use chickenwm_core::init_tracing;
use chickenwm_core::ipc::{
    send_ipc_command_blocking, Action, IpcCommand, PIPE_NAME_DAEMON, PIPE_NAME_LAUNCHER,
    PIPE_NAME_NOTIFY, PIPE_NAME_WM,
};
use parking_lot::RwLock;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Instant;
use tracing::{info, warn};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_0,
    VK_1, VK_2, VK_3, VK_4, VK_5, VK_6, VK_7, VK_8, VK_9, VK_A, VK_B, VK_BACK, VK_C, VK_CONTROL,
    VK_D, VK_DELETE, VK_DOWN, VK_E, VK_END, VK_ESCAPE, VK_F, VK_F1, VK_F10, VK_F11, VK_F12, VK_F2,
    VK_F3, VK_F4, VK_F5, VK_F6, VK_F7, VK_F8, VK_F9, VK_G, VK_H, VK_HOME, VK_I, VK_INSERT, VK_J,
    VK_K, VK_L, VK_LCONTROL, VK_LEFT, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_M, VK_MENU, VK_N, VK_NEXT,
    VK_O, VK_P, VK_PRIOR, VK_Q, VK_R, VK_RCONTROL, VK_RETURN, VK_RIGHT, VK_RMENU, VK_RSHIFT,
    VK_RWIN, VK_S, VK_SHIFT, VK_SPACE, VK_T, VK_TAB, VK_U, VK_UP, VK_V, VK_W, VK_X, VK_Y, VK_Z,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, SetWindowsHookExW, KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT,
    WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

const CHAIN_CHORD_TIMEOUT_MS: u128 = 2000;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChainFollower {
    req_super: Option<bool>,
    req_ctrl: bool,
    req_alt: bool,
    req_shift: bool,
    vk: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingChain {
    req_super: bool,
    req_ctrl: bool,
    req_alt: bool,
    req_shift: bool,
    leader_vk: u32,
    submap: String,
    armed_at: Instant,
}

#[derive(Debug, Clone)]
struct CompiledBind {
    kind: BindKind,
    req_super: bool,
    req_ctrl: bool,
    req_alt: bool,
    req_shift: bool,
    vk: u32, // Virtual-Key code or mouse button id (272 = LMB, 273 = RMB)
    chain_next: Option<ChainFollower>,
    submap: String,
    action: Action,
}

#[derive(Default)]
struct HookState {
    super_down: AtomicBool,
    ctrl_down: AtomicBool,
    alt_down: AtomicBool,
    shift_down: AtomicBool,
    super_chord_used: AtomicBool,

    // Mouse drag state ($mainMod + LMB/RMB)
    dragging_move: AtomicBool,
    dragging_resize: AtomicBool,
    drag_start_x: AtomicI32,
    drag_start_y: AtomicI32,
    drag_curr_x: AtomicI32,
    drag_curr_y: AtomicI32,
    drag_dirty: AtomicBool,
    /// A `MouseDragWindow` is queued or being sent to the WM; newer cursor positions
    /// wait (coalesced) instead of piling up behind the IPC round-trip.
    drag_in_flight: AtomicBool,

    binds: RwLock<Vec<CompiledBind>>,
    raw_rules: RwLock<Vec<BindRule>>,
    active_submap: RwLock<String>,
    pending_chain: RwLock<Option<PendingChain>>,
    dispatch_tx: OnceLock<Sender<Action>>,
    pacer_control: OnceLock<Arc<FramePacerControl>>,
}

static HOOK_STATE: OnceLock<HookState> = OnceLock::new();

fn state() -> &'static HookState {
    HOOK_STATE.get_or_init(HookState::default)
}

fn parse_mods(mods_str: &str) -> (bool, bool, bool, bool) {
    let upper = mods_str.to_ascii_uppercase();
    let req_super = upper.contains("SUPER") || upper.contains("WIN") || upper.contains("MOD4");
    let req_ctrl = upper.contains("CTRL") || upper.contains("CONTROL");
    let req_alt = upper.contains("ALT") || upper.contains("MOD1");
    let req_shift = upper.contains("SHIFT");
    (req_super, req_ctrl, req_alt, req_shift)
}

fn parse_key_to_vk(key: &str) -> Option<u32> {
    let k = key.trim().to_ascii_uppercase();
    match k.as_str() {
        "MOUSE:272" | "MOUSE_LEFT" | "LMB" => Some(272),
        "MOUSE:273" | "MOUSE_RIGHT" | "RMB" => Some(273),
        "RETURN" | "ENTER" => Some(VK_RETURN.0 as u32),
        "SPACE" => Some(VK_SPACE.0 as u32),
        "TAB" => Some(VK_TAB.0 as u32),
        "ESCAPE" | "ESC" => Some(VK_ESCAPE.0 as u32),
        "BACKSPACE" | "BACK" => Some(VK_BACK.0 as u32),
        "DELETE" | "DEL" => Some(VK_DELETE.0 as u32),
        "INSERT" | "INS" => Some(VK_INSERT.0 as u32),
        "HOME" => Some(VK_HOME.0 as u32),
        "END" => Some(VK_END.0 as u32),
        "PAGEUP" | "PGUP" | "PRIOR" => Some(VK_PRIOR.0 as u32),
        "PAGEDOWN" | "PGDN" | "NEXT" => Some(VK_NEXT.0 as u32),
        "LEFT" => Some(VK_LEFT.0 as u32),
        "RIGHT" => Some(VK_RIGHT.0 as u32),
        "UP" => Some(VK_UP.0 as u32),
        "DOWN" => Some(VK_DOWN.0 as u32),
        "0" => Some(VK_0.0 as u32),
        "1" => Some(VK_1.0 as u32),
        "2" => Some(VK_2.0 as u32),
        "3" => Some(VK_3.0 as u32),
        "4" => Some(VK_4.0 as u32),
        "5" => Some(VK_5.0 as u32),
        "6" => Some(VK_6.0 as u32),
        "7" => Some(VK_7.0 as u32),
        "8" => Some(VK_8.0 as u32),
        "9" => Some(VK_9.0 as u32),
        "A" => Some(VK_A.0 as u32),
        "B" => Some(VK_B.0 as u32),
        "C" => Some(VK_C.0 as u32),
        "D" => Some(VK_D.0 as u32),
        "E" => Some(VK_E.0 as u32),
        "F" => Some(VK_F.0 as u32),
        "G" => Some(VK_G.0 as u32),
        "H" => Some(VK_H.0 as u32),
        "I" => Some(VK_I.0 as u32),
        "J" => Some(VK_J.0 as u32),
        "K" => Some(VK_K.0 as u32),
        "L" => Some(VK_L.0 as u32),
        "M" => Some(VK_M.0 as u32),
        "N" => Some(VK_N.0 as u32),
        "O" => Some(VK_O.0 as u32),
        "P" => Some(VK_P.0 as u32),
        "Q" => Some(VK_Q.0 as u32),
        "R" => Some(VK_R.0 as u32),
        "S" => Some(VK_S.0 as u32),
        "T" => Some(VK_T.0 as u32),
        "U" => Some(VK_U.0 as u32),
        "V" => Some(VK_V.0 as u32),
        "W" => Some(VK_W.0 as u32),
        "X" => Some(VK_X.0 as u32),
        "Y" => Some(VK_Y.0 as u32),
        "Z" => Some(VK_Z.0 as u32),
        "F1" => Some(VK_F1.0 as u32),
        "F2" => Some(VK_F2.0 as u32),
        "F3" => Some(VK_F3.0 as u32),
        "F4" => Some(VK_F4.0 as u32),
        "F5" => Some(VK_F5.0 as u32),
        "F6" => Some(VK_F6.0 as u32),
        "F7" => Some(VK_F7.0 as u32),
        "F8" => Some(VK_F8.0 as u32),
        "F9" => Some(VK_F9.0 as u32),
        "F10" => Some(VK_F10.0 as u32),
        "F11" => Some(VK_F11.0 as u32),
        "F12" => Some(VK_F12.0 as u32),
        // VK_SNAPSHOT
        "PRINT" | "PRINTSCREEN" | "PRTSC" | "SNAPSHOT" => Some(0x2C),
        // VK_OEM_2 ('/' / '?')
        "SLASH" | "QUESTION" | "/" | "?" => Some(0xBF),
        _ => None,
    }
}

fn parse_key_or_chain(key_spec: &str) -> Option<(u32, Option<ChainFollower>)> {
    if let Some((leader_raw, follower_raw)) = key_spec.split_once('>') {
        let leader_vk = parse_key_to_vk(leader_raw.trim())?;
        let follower_str = follower_raw.trim();
        let (f_mods, f_key) = if let Some(idx) = follower_str.rfind(|c: char| c.is_whitespace() || c == '+') {
            (&follower_str[..idx], &follower_str[idx + 1..])
        } else {
            ("", follower_str)
        };
        let (has_super, req_ctrl, req_alt, req_shift) = parse_mods(f_mods);
        let follower_vk = parse_key_to_vk(f_key)?;
        Some((
            leader_vk,
            Some(ChainFollower {
                // If the follower didn't explicitly require Super, accept it whether Super is still held from the leader or released.
                req_super: if has_super { Some(true) } else { None },
                req_ctrl,
                req_alt,
                req_shift,
                vk: follower_vk,
            }),
        ))
    } else {
        Some((parse_key_to_vk(key_spec)?, None))
    }
}

fn compile_binds(rules: &[BindRule]) -> Vec<CompiledBind> {
    let mut out = Vec::with_capacity(rules.len() + 10);
    let mut has_resize_submap = false;
    for r in rules {
        if r.submap.eq_ignore_ascii_case("resize") {
            has_resize_submap = true;
        }
        let (req_super, req_ctrl, req_alt, req_shift) = parse_mods(&r.mods);
        if let (Some((vk, chain_next)), Some(action)) = (
            parse_key_or_chain(&r.key),
            Action::parse_dispatcher(&r.dispatcher, &r.arg),
        ) {
            out.push(CompiledBind {
                kind: r.kind,
                req_super,
                req_ctrl,
                req_alt,
                req_shift,
                vk,
                chain_next,
                submap: r.submap.to_ascii_lowercase(),
                action,
            });
        }
    }
    // Built-in default `resize` modal layer if the user config hasn't defined custom `submap = resize` rules.
    if !has_resize_submap {
        let defaults = [
            (BindKind::Repeat, "LEFT", Action::ResizeActive { dx: -40, dy: 0 }),
            (BindKind::Repeat, "RIGHT", Action::ResizeActive { dx: 40, dy: 0 }),
            (BindKind::Repeat, "UP", Action::ResizeActive { dx: 0, dy: -40 }),
            (BindKind::Repeat, "DOWN", Action::ResizeActive { dx: 0, dy: 40 }),
            (BindKind::Repeat, "H", Action::ResizeActive { dx: -40, dy: 0 }),
            (BindKind::Repeat, "L", Action::ResizeActive { dx: 40, dy: 0 }),
            (BindKind::Repeat, "K", Action::ResizeActive { dx: 0, dy: -40 }),
            (BindKind::Repeat, "J", Action::ResizeActive { dx: 0, dy: 40 }),
            (BindKind::Press, "ESCAPE", Action::Submap("reset".to_string())),
            (BindKind::Press, "RETURN", Action::Submap("reset".to_string())),
        ];
        for (kind, key, action) in defaults {
            if let Some(vk) = parse_key_to_vk(key) {
                out.push(CompiledBind {
                    kind,
                    req_super: false,
                    req_ctrl: false,
                    req_alt: false,
                    req_shift: false,
                    vk,
                    chain_next: None,
                    submap: "resize".to_string(),
                    action,
                });
            }
        }
    }
    out
}

/// Sends a dummy neutral key event when `SUPER/Win` is used in a chord so the Windows Start Menu
/// never pops up upon releasing the standalone `Win` key.
unsafe fn suppress_windows_start_menu_tap() {
    let inputs = [
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0xFF),
                    wScan: 0,
                    dwFlags: Default::default(),
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0xFF),
                    wScan: 0,
                    dwFlags: KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
    ];
    unsafe {
        let _ = SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

unsafe extern "system" fn low_level_keyboard_proc(
    n_code: i32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    unsafe {
        if n_code >= 0 {
            let kb = &*(l_param.0 as *const KBDLLHOOKSTRUCT);
            let vk = kb.vkCode;
            // Ignore synthetic 0xFF key events sent by `suppress_windows_start_menu_tap`
            if vk == 0xFF {
                return CallNextHookEx(None, n_code, w_param, l_param);
            }

            let msg = w_param.0 as u32;
            let is_down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            let is_up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
            let st = state();

            // Update modifier tracking state
            if vk == VK_LWIN.0 as u32 || vk == VK_RWIN.0 as u32 {
                if is_down {
                    st.super_down.store(true, Ordering::SeqCst);
                    st.super_chord_used.store(false, Ordering::SeqCst);
                } else if is_up {
                    st.super_down.store(false, Ordering::SeqCst);
                    if !st.super_chord_used.load(Ordering::SeqCst) {
                        // Consume standalone Win key tap so Start Menu does not steal focus
                        suppress_windows_start_menu_tap();
                    }
                }
                return CallNextHookEx(None, n_code, w_param, l_param);
            }
            if vk == VK_LCONTROL.0 as u32 || vk == VK_RCONTROL.0 as u32 || vk == VK_CONTROL.0 as u32 {
                st.ctrl_down.store(is_down, Ordering::SeqCst);
                return CallNextHookEx(None, n_code, w_param, l_param);
            }
            if vk == VK_LMENU.0 as u32 || vk == VK_RMENU.0 as u32 || vk == VK_MENU.0 as u32 {
                st.alt_down.store(is_down, Ordering::SeqCst);
                return CallNextHookEx(None, n_code, w_param, l_param);
            }
            if vk == VK_LSHIFT.0 as u32 || vk == VK_RSHIFT.0 as u32 || vk == VK_SHIFT.0 as u32 {
                st.shift_down.store(is_down, Ordering::SeqCst);
                return CallNextHookEx(None, n_code, w_param, l_param);
            }

            use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
            let hw_super = (GetAsyncKeyState(VK_LWIN.0 as i32) as u16 & 0x8000) != 0
                || (GetAsyncKeyState(VK_RWIN.0 as i32) as u16 & 0x8000) != 0;
            let hw_ctrl = (GetAsyncKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0
                || (GetAsyncKeyState(VK_LCONTROL.0 as i32) as u16 & 0x8000) != 0
                || (GetAsyncKeyState(VK_RCONTROL.0 as i32) as u16 & 0x8000) != 0;
            let hw_alt = (GetAsyncKeyState(VK_MENU.0 as i32) as u16 & 0x8000) != 0
                || (GetAsyncKeyState(VK_LMENU.0 as i32) as u16 & 0x8000) != 0
                || (GetAsyncKeyState(VK_RMENU.0 as i32) as u16 & 0x8000) != 0;
            let hw_shift = (GetAsyncKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000) != 0
                || (GetAsyncKeyState(VK_LSHIFT.0 as i32) as u16 & 0x8000) != 0
                || (GetAsyncKeyState(VK_RSHIFT.0 as i32) as u16 & 0x8000) != 0;

            let cur_super = st.super_down.load(Ordering::Relaxed) || hw_super;
            let cur_ctrl = st.ctrl_down.load(Ordering::Relaxed) || hw_ctrl;
            let cur_alt = st.alt_down.load(Ordering::Relaxed) || hw_alt;
            let cur_shift = st.shift_down.load(Ordering::Relaxed) || hw_shift;

            // Normalize numpad 0..9 (0x60..=0x69) to top-row 0..9 (0x30..=0x39) for matching
            let normalized_vk = if (0x60..=0x69).contains(&vk) {
                vk - 0x30
            } else {
                vk
            };

            // Emergency Failsafe: Ctrl + Alt + Shift + Escape
            if is_down && cur_ctrl && cur_alt && cur_shift && vk == VK_ESCAPE.0 as u32 {
                trigger_emergency_restore(true);
                return LRESULT(1);
            }

            // Dismiss cheat-sheet overlay on Escape
            if is_down && vk == VK_ESCAPE.0 as u32 && cheatsheet::close_cheatsheet_if_open() {
                return LRESULT(1);
            }

            // 1. Check pending chain chord follower (`Mod+K, then key`)
            let active_chain = st.pending_chain.read().clone();
            if let Some(chain) = active_chain {
                if chain.armed_at.elapsed().as_millis() <= CHAIN_CHORD_TIMEOUT_MS {
                    if is_down {
                        *st.pending_chain.write() = None;
                        if vk == VK_ESCAPE.0 as u32 {
                            return LRESULT(1);
                        }
                        let binds = st.binds.read();
                        for b in binds.iter() {
                            if b.submap == chain.submap
                                && b.vk == chain.leader_vk
                                && b.req_super == chain.req_super
                                && b.req_ctrl == chain.req_ctrl
                                && b.req_alt == chain.req_alt
                                && b.req_shift == chain.req_shift
                            {
                                if let Some(ref f) = b.chain_next {
                                    if (f.vk == vk || f.vk == normalized_vk)
                                        && f.req_ctrl == cur_ctrl
                                        && f.req_alt == cur_alt
                                        && f.req_shift == cur_shift
                                        && f.req_super.map_or(true, |s| s == cur_super)
                                    {
                                        if cur_super {
                                            st.super_chord_used.store(true, Ordering::SeqCst);
                                            suppress_windows_start_menu_tap();
                                        }
                                        if let Some(tx) = st.dispatch_tx.get() {
                                            let _ = tx.send(b.action.clone());
                                        }
                                        return LRESULT(1);
                                    }
                                }
                            }
                        }
                        return LRESULT(1);
                    } else if is_up {
                        return LRESULT(1);
                    }
                } else {
                    *st.pending_chain.write() = None;
                }
            }

            // 2. Match against configured ChickenWM keybinds in the active modal layer (`submap`)
            let cur_submap = st.active_submap.read().clone();
            let binds = st.binds.read();
            for b in binds.iter() {
                if b.submap == cur_submap
                    && (b.vk == vk || b.vk == normalized_vk)
                    && b.req_super == cur_super
                    && b.req_ctrl == cur_ctrl
                    && b.req_alt == cur_alt
                    && b.req_shift == cur_shift
                {
                    if b.chain_next.is_some() {
                        if is_down {
                            if cur_super {
                                st.super_chord_used.store(true, Ordering::SeqCst);
                                suppress_windows_start_menu_tap();
                            }
                            *st.pending_chain.write() = Some(PendingChain {
                                req_super: cur_super,
                                req_ctrl: cur_ctrl,
                                req_alt: cur_alt,
                                req_shift: cur_shift,
                                leader_vk: b.vk,
                                submap: cur_submap,
                                armed_at: Instant::now(),
                            });
                            return LRESULT(1);
                        } else if is_up {
                            return LRESULT(1);
                        }
                    }

                    let should_fire = match b.kind {
                        BindKind::Press | BindKind::Repeat => is_down,
                        BindKind::Release => is_up,
                        BindKind::Mouse => false,
                    };
                    if should_fire {
                        if cur_super {
                            st.super_chord_used.store(true, Ordering::SeqCst);
                            suppress_windows_start_menu_tap();
                        }
                        if let Some(tx) = st.dispatch_tx.get() {
                            let _ = tx.send(b.action.clone());
                        }
                        // Consume the key event so Windows doesn't trigger native Win+E / Win+D / Win+Tab / Win+1..9
                        return LRESULT(1);
                    } else if is_up && (b.kind == BindKind::Press || b.kind == BindKind::Repeat) {
                        // Also consume the matching keyup for intercepted chords
                        return LRESULT(1);
                    }
                }
            }

            // 3. Always allow Escape to exit a non-default modal layer even if not explicitly bound
            if !cur_submap.is_empty() && is_down && vk == VK_ESCAPE.0 as u32 {
                if let Some(tx) = st.dispatch_tx.get() {
                    let _ = tx.send(Action::Submap("reset".to_string()));
                }
                return LRESULT(1);
            }
        }

        CallNextHookEx(None, n_code, w_param, l_param)
    }
}

unsafe extern "system" fn low_level_mouse_proc(
    n_code: i32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    unsafe {
        if n_code >= 0 {
            let ms = &*(l_param.0 as *const MSLLHOOKSTRUCT);
            let msg = w_param.0 as u32;
            let st = state();

            use windows::Win32::UI::Input::KeyboardAndMouse::{
                GetAsyncKeyState, VK_LBUTTON, VK_RBUTTON,
            };
            let hw_super = (GetAsyncKeyState(VK_LWIN.0 as i32) as u16 & 0x8000) != 0
                || (GetAsyncKeyState(VK_RWIN.0 as i32) as u16 & 0x8000) != 0;
            let cur_super = st.super_down.load(Ordering::Relaxed) || hw_super;

            match msg {
                WM_LBUTTONDOWN if cur_super => {
                    st.super_chord_used.store(true, Ordering::SeqCst);
                    suppress_windows_start_menu_tap();
                    st.drag_start_x.store(ms.pt.x, Ordering::SeqCst);
                    st.drag_start_y.store(ms.pt.y, Ordering::SeqCst);
                    st.drag_curr_x.store(ms.pt.x, Ordering::SeqCst);
                    st.drag_curr_y.store(ms.pt.y, Ordering::SeqCst);
                    st.dragging_move.store(true, Ordering::SeqCst);
                    st.drag_dirty.store(true, Ordering::SeqCst);
                    if let Some(tx) = st.dispatch_tx.get() {
                        let _ = tx.send(Action::MouseDragStart {
                            is_resize: false,
                            x: ms.pt.x,
                            y: ms.pt.y,
                        });
                    }
                    if let Some(pacer) = st.pacer_control.get() {
                        pacer.wake();
                    }
                    return LRESULT(1);
                }
                WM_LBUTTONUP => {
                    if st.dragging_move.swap(false, Ordering::SeqCst) {
                        if let Some(tx) = st.dispatch_tx.get() {
                            let _ = tx.send(Action::MouseDragEnd {
                                is_resize: false,
                                x: ms.pt.x,
                                y: ms.pt.y,
                            });
                        }
                        return LRESULT(1);
                    }
                }
                WM_RBUTTONDOWN if cur_super => {
                    st.super_chord_used.store(true, Ordering::SeqCst);
                    suppress_windows_start_menu_tap();
                    st.drag_start_x.store(ms.pt.x, Ordering::SeqCst);
                    st.drag_start_y.store(ms.pt.y, Ordering::SeqCst);
                    st.drag_curr_x.store(ms.pt.x, Ordering::SeqCst);
                    st.drag_curr_y.store(ms.pt.y, Ordering::SeqCst);
                    st.dragging_resize.store(true, Ordering::SeqCst);
                    st.drag_dirty.store(true, Ordering::SeqCst);
                    if let Some(tx) = st.dispatch_tx.get() {
                        let _ = tx.send(Action::MouseDragStart {
                            is_resize: true,
                            x: ms.pt.x,
                            y: ms.pt.y,
                        });
                    }
                    if let Some(pacer) = st.pacer_control.get() {
                        pacer.wake();
                    }
                    return LRESULT(1);
                }
                WM_RBUTTONUP => {
                    if st.dragging_resize.swap(false, Ordering::SeqCst) {
                        if let Some(tx) = st.dispatch_tx.get() {
                            let _ = tx.send(Action::MouseDragEnd {
                                is_resize: true,
                                x: ms.pt.x,
                                y: ms.pt.y,
                            });
                        }
                        return LRESULT(1);
                    }
                }
                WM_MOUSEMOVE => {
                    let mut is_move = st.dragging_move.load(Ordering::Relaxed);
                    let mut is_resize = st.dragging_resize.load(Ordering::Relaxed);

                    // Auto-recover if Super + LMB/RMB is held during move but button-down was before Super
                    if !is_move && !is_resize && cur_super {
                        let lmb = (GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 & 0x8000) != 0;
                        let rmb = (GetAsyncKeyState(VK_RBUTTON.0 as i32) as u16 & 0x8000) != 0;
                        if lmb {
                            st.super_chord_used.store(true, Ordering::SeqCst);
                            suppress_windows_start_menu_tap();
                            st.drag_start_x.store(ms.pt.x, Ordering::SeqCst);
                            st.drag_start_y.store(ms.pt.y, Ordering::SeqCst);
                            st.dragging_move.store(true, Ordering::SeqCst);
                            is_move = true;
                            if let Some(tx) = st.dispatch_tx.get() {
                                let _ = tx.send(Action::MouseDragStart {
                                    is_resize: false,
                                    x: ms.pt.x,
                                    y: ms.pt.y,
                                });
                            }
                        } else if rmb {
                            st.super_chord_used.store(true, Ordering::SeqCst);
                            suppress_windows_start_menu_tap();
                            st.drag_start_x.store(ms.pt.x, Ordering::SeqCst);
                            st.drag_start_y.store(ms.pt.y, Ordering::SeqCst);
                            st.dragging_resize.store(true, Ordering::SeqCst);
                            is_resize = true;
                            if let Some(tx) = st.dispatch_tx.get() {
                                let _ = tx.send(Action::MouseDragStart {
                                    is_resize: true,
                                    x: ms.pt.x,
                                    y: ms.pt.y,
                                });
                            }
                        }
                    }

                    if is_move || is_resize {
                        st.drag_curr_x.store(ms.pt.x, Ordering::Relaxed);
                        st.drag_curr_y.store(ms.pt.y, Ordering::Relaxed);
                        st.drag_dirty.store(true, Ordering::Release);
                        if let Some(pacer) = st.pacer_control.get() {
                            pacer.wake();
                        }
                    }
                }
                _ => {}
            }
        }

        CallNextHookEx(None, n_code, w_param, l_param)
    }
}

/// `ShellExecuteW(open)`; true on success (the API returns a value > 32).
fn shell_open(target: &str, params: &str) -> bool {
    use windows::core::{w, HSTRING};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let target = HSTRING::from(target);
    let params = HSTRING::from(params);
    let result = unsafe { ShellExecuteW(None, w!("open"), &target, &params, None, SW_SHOWNORMAL) };
    result.0 as isize > 32
}

fn resolve_program_path(program: &str) -> std::path::PathBuf {
    let p = std::path::Path::new(program);
    if p.is_absolute() || p.exists() {
        return p.to_path_buf();
    }
    if let Ok(mut exe_dir) = std::env::current_exe() {
        exe_dir.pop();
        let sibling = exe_dir.join(program);
        if sibling.exists() {
            return sibling;
        }
        if !program.ends_with(".exe") {
            let sibling_exe = exe_dir.join(format!("{program}.exe"));
            if sibling_exe.exists() {
                return sibling_exe;
            }
        }
    }
    p.to_path_buf()
}

fn execute_action(action: Action) {
    match action {
        Action::Exec(ref cmd_line) => {
            info!("Executing keybind command: {}", cmd_line);
            let parts: Vec<&str> = cmd_line.split_whitespace().collect();
            if let Some((program, args)) = parts.split_first() {
                let resolved = resolve_program_path(program);
                if let Err(e) = Command::new(&resolved).args(args).spawn() {
                    // Not a runnable program: let the shell open it instead, which covers URIs
                    // (`ms-screenclip:`, `ms-settings:`), documents and shortcuts.
                    if !shell_open(program, &args.join(" ")) {
                        warn!("exec failed for `{cmd_line}`: {e}");
                    }
                }
            }
        }
        Action::OpenFiles(ref path) => {
            let resolved = resolve_program_path("chickenwm-files.exe");
            let mut cmd = Command::new(resolved);
            if let Some(p) = path {
                cmd.arg(p);
            }
            if let Err(e) = cmd.spawn() {
                warn!("open files failed: {e}");
            }
        }
        Action::OpenSettings => {
            let resolved = resolve_program_path("chickenwm-settings.exe");
            if let Err(e) = Command::new(resolved).spawn() {
                warn!("open settings failed: {e}");
            }
        }
        Action::SummonLauncher => {
            if send_ipc_command_blocking(PIPE_NAME_DAEMON, &IpcCommand::SummonLauncher).is_err() {
                if send_ipc_command_blocking(PIPE_NAME_LAUNCHER, &IpcCommand::SummonLauncher).is_err() {
                    let resolved = resolve_program_path("chickenwm-launcher.exe");
                    if let Err(e) = Command::new(resolved).spawn() {
                        warn!("summon launcher failed: {e}");
                    }
                }
            }
        }
        Action::ToggleNotificationCenter => {
            if send_ipc_command_blocking(PIPE_NAME_NOTIFY, &IpcCommand::ToggleNotificationCenter).is_err() {
                if send_ipc_command_blocking(PIPE_NAME_DAEMON, &IpcCommand::ToggleNotificationCenter).is_err() {
                    let resolved = resolve_program_path("chickenwm-notify.exe");
                    if let Err(e) = Command::new(resolved).spawn() {
                        warn!("toggle notifications failed: {e}");
                    }
                }
            }
        }
        Action::Submap(ref name) => {
            let st = state();
            let trimmed = name.trim();
            if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("reset") {
                info!("Exiting modal submap layer");
                st.active_submap.write().clear();
            } else {
                let target = trimmed.to_ascii_lowercase();
                info!("Entering modal submap layer: {}", target);
                *st.active_submap.write() = target;
            }
        }
        Action::ShowBindsCheatSheet => {
            let st = state();
            let rules = st.raw_rules.read().clone();
            let active = st.active_submap.read().clone();
            cheatsheet::toggle_cheatsheet_overlay(rules, active);
        }
        // `exit` must reach the daemon so it shuts every module down; sent to the WM alone,
        // the daemon's watchdog would just respawn it.
        Action::Exit => {
            if send_ipc_command_blocking(PIPE_NAME_DAEMON, &IpcCommand::Dispatch(Action::Exit))
                .is_err()
            {
                let _ = send_ipc_command_blocking(PIPE_NAME_WM, &IpcCommand::Dispatch(Action::Exit));
            }
        }
        other => {
            if send_ipc_command_blocking(PIPE_NAME_WM, &IpcCommand::Dispatch(other.clone())).is_err()
            {
                let _ = send_ipc_command_blocking(PIPE_NAME_DAEMON, &IpcCommand::Dispatch(other));
            }
        }
    }
}

fn apply_config(cfg: &HyprConfig) {
    let st = state();
    *st.raw_rules.write() = cfg.binds.clone();
    *st.binds.write() = compile_binds(&cfg.binds);
    if let Some(pacer) = st.pacer_control.get() {
        pacer.set_fps_limit(if cfg.misc.fps_limit == 0 {
            None
        } else {
            Some(cfg.misc.fps_limit)
        });
        pacer.set_vfr(cfg.misc.vfr);
    }
}

fn main() -> Result<()> {
    init_tracing("chickenwm_binds");
    let _failsafe = spawn_emergency_failsafe_thread();

    let (_doc, initial_cfg) = bootstrap_config_and_themes()?;
    let pacer_control = FramePacerControl::new(initial_cfg.misc.fps_limit, initial_cfg.misc.vfr);

    let (tx, rx) = unbounded::<Action>();
    let st = state();
    let _ = st.dispatch_tx.set(tx.clone());
    let _ = st.pacer_control.set(Arc::clone(&pacer_control));
    apply_config(&initial_cfg);

    // Watch ~/.config/chickenwm/chickenwm.conf for <5ms hot-reload of keybindings & fps_limit
    let _watcher = spawn_config_watcher(|ev| {
        if let ConfigWatchEvent::HyprConfChanged(_) = ev {
            if let Ok((_, new_cfg)) = load_or_init_config() {
                info!(
                    "Reloaded {} keybinds from chickenwm.conf",
                    new_cfg.binds.len()
                );
                apply_config(&new_cfg);
            }
        }
    });

    // Spawn worker thread for non-blocking IPC / process dispatch
    thread::Builder::new()
        .name("chickenwm-binds-dispatcher".to_string())
        .spawn(move || {
            while let Ok(action) = rx.recv() {
                let is_drag_update = matches!(action, Action::MouseDragWindow { .. });
                execute_action(action);
                if is_drag_update {
                    let st = state();
                    st.drag_in_flight.store(false, Ordering::Release);
                    if let Some(pacer) = st.pacer_control.get() {
                        pacer.wake();
                    }
                }
            }
        })?;

    // Phase 4.3: Spawn Throttled Mouse Resize/Move Stream synced to Phase 3 Adaptive Frame Scheduler
    let tx_mouse = tx.clone();
    let pacer_mouse = Arc::clone(&pacer_control);
    thread::Builder::new()
        .name("chickenwm-binds-mouse-stream".to_string())
        .spawn(move || {
            let mut scheduler = AdaptiveFrameScheduler::new(pacer_mouse);
            loop {
                let st = state();
                let is_move = st.dragging_move.load(Ordering::Relaxed);
                let is_resize = st.dragging_resize.load(Ordering::Relaxed);
                let active = is_move || is_resize;

                let _dt = scheduler.wait_for_next_frame(active);
                // Re-read after the frame wait: the button may have been released meanwhile, and
                // an update sent after MouseDragEnd would reach the WM as a stale straggler.
                let is_resize = st.dragging_resize.load(Ordering::Acquire);
                let active = is_resize || st.dragging_move.load(Ordering::Acquire);
                if active
                    && !st.drag_in_flight.load(Ordering::Acquire)
                    && st.drag_dirty.swap(false, Ordering::AcqRel)
                {
                    // `start_*` stays fixed for the whole drag: the WM computes geometry from the
                    // total offset, so a coalesced or lost update can never desync the window.
                    let start_x = st.drag_start_x.load(Ordering::Relaxed);
                    let start_y = st.drag_start_y.load(Ordering::Relaxed);
                    let curr_x = st.drag_curr_x.load(Ordering::Relaxed);
                    let curr_y = st.drag_curr_y.load(Ordering::Relaxed);

                    st.drag_in_flight.store(true, Ordering::Release);
                    let _ = tx_mouse.send(Action::MouseDragWindow {
                        is_resize,
                        start_x,
                        start_y,
                        curr_x,
                        curr_y,
                    });
                }
            }
        })?;

    // Phase 4.1: Dedicated High-Priority Win32 Hook Thread (`THREAD_PRIORITY_TIME_CRITICAL`)
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);

        let kb_hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(low_level_keyboard_proc), None, 0)?;
        let _kb_guard = SafeHhook::new(kb_hook);

        let ms_hook = SetWindowsHookExW(WH_MOUSE_LL, Some(low_level_mouse_proc), None, 0)?;
        let _ms_guard = SafeHhook::new(ms_hook);

        info!(
            "chickenwm-binds active (TIME_CRITICAL hook thread, {} binds loaded, target {} Hz)",
            st.binds.read().len(),
            pacer_control.effective_target_fps()
        );

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            // Raw hook thread pumps messages directly
        }
    }

    warn!("chickenwm-binds message loop exited.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chickenwm_core::config::{ConfigDocument, DEFAULT_CHICKENWM_CONF};

    #[test]
    fn compile_binds_parses_chain_chords_and_modal_layers() {
        let cfg = ConfigDocument::parse(DEFAULT_CHICKENWM_CONF)
            .expect("parse default config")
            .compile();
        let compiled = compile_binds(&cfg.binds);

        // Verify chain chord `SUPER, K > R -> submap resize`
        let chain_resize = compiled
            .iter()
            .find(|b| matches!(&b.action, Action::Submap(s) if s == "resize") && b.chain_next.is_some())
            .expect("should compile K > R chain chord");
        assert!(chain_resize.req_super);
        assert_eq!(chain_resize.vk, VK_K.0 as u32);
        let follower = chain_resize.chain_next.as_ref().unwrap();
        assert_eq!(follower.vk, VK_R.0 as u32);
        assert_eq!(follower.req_super, None);

        // Verify modal layer `submap = resize` binds
        assert!(compiled.iter().any(|b| {
            b.submap == "resize"
                && b.vk == VK_H.0 as u32
                && matches!(b.action, Action::ResizeActive { dx: -40, dy: 0 })
        }));
    }

    #[test]
    fn cheatsheet_sections_group_binds_and_mark_active_submap() {
        let cfg = ConfigDocument::parse(DEFAULT_CHICKENWM_CONF)
            .expect("parse default config")
            .compile();
        let sections = cheatsheet::build_cheatsheet_sections(&cfg.binds, "resize");
        assert!(sections.iter().any(|s| s.title == "Apps & Shell"));
        assert!(sections.iter().any(|s| s.title == "Chain Chords"));
        assert!(sections
            .iter()
            .any(|s| s.title == "Modal Layer: resize (ACTIVE)"));
    }
}
