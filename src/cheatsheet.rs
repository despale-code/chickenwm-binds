//! Keybind Cheat-Sheet Overlay generated dynamically from `HyprConfig.binds`.

use chickenwm_core::config::BindRule;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::OnceLock;
use std::thread;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreatePen, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint,
    FillRect, InvalidateRect, RoundRect, SelectObject, SetBkMode, SetTextColor, CLEARTYPE_QUALITY,
    CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER,
    FW_BOLD, FW_MEDIUM, FW_NORMAL, HBRUSH, HDC, OUT_DEFAULT_PRECIS, PAINTSTRUCT, PS_SOLID,
     TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_ESCAPE, VK_F1};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetSystemMetrics, IsWindow, PostMessageW, PostQuitMessage, RegisterClassW, SetForegroundWindow,
     ShowWindow, TranslateMessage, CS_HREDRAW, CS_VREDRAW, MSG, SM_CXSCREEN, SM_CYSCREEN, SW_SHOW,
    WM_CLOSE, WM_DESTROY, WM_KEYDOWN, WM_LBUTTONDOWN, WM_PAINT, WM_RBUTTONDOWN, WNDCLASSW,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheatSheetSection {
    pub title: String,
    pub entries: Vec<(String, String)>,
}

static CHEATSHEET_HWND: AtomicIsize = AtomicIsize::new(0);
static CHEATSHEET_DATA: OnceLock<Mutex<(Vec<CheatSheetSection>, String)>> = OnceLock::new();

fn data_store() -> &'static Mutex<(Vec<CheatSheetSection>, String)> {
    CHEATSHEET_DATA.get_or_init(|| Mutex::new((Vec::new(), String::new())))
}

pub fn close_cheatsheet_if_open() -> bool {
    let raw = CHEATSHEET_HWND.load(Ordering::Acquire);
    if raw != 0 {
        let hwnd = HWND(raw as *mut _);
        if unsafe { IsWindow(hwnd).as_bool() } {
            unsafe {
                let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
            }
            return true;
        }
    }
    false
}

pub fn format_chord_label(mods: &str, key: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    for token in mods
        .split(|c: char| c.is_whitespace() || c == '+' || c == '_')
        .filter(|s| !s.is_empty())
    {
        let upper = token.to_ascii_uppercase();
        let label = match upper.as_str() {
            "SUPER" | "WIN" | "MOD4" => "Super",
            "CTRL" | "CONTROL" => "Ctrl",
            "ALT" | "MOD1" => "Alt",
            "SHIFT" => "Shift",
            other => other,
        };
        parts.push(label.to_string());
    }
    let key_clean = if let Some((leader, follower)) = key.split_once('>') {
        format!("{} → {}", leader.trim(), follower.trim())
    } else {
        match key.trim().to_ascii_uppercase().as_str() {
            "MOUSE:272" | "LMB" => "Left Drag".to_string(),
            "MOUSE:273" | "RMB" => "Right Drag".to_string(),
            "RETURN" => "Enter".to_string(),
            "ESCAPE" => "Esc".to_string(),
            _ => key.trim().to_string(),
        }
    };
    if parts.is_empty() {
        key_clean
    } else {
        format!("{} + {}", parts.join(" + "), key_clean)
    }
}

pub fn format_action_description(dispatcher: &str, arg: &str) -> String {
    let d = dispatcher.trim().to_ascii_lowercase();
    let a = arg.trim();
    match d.as_str() {
        "exec" | "execr" => format!("Run {a}"),
        "killactive" | "closewindow" => "Close focused window".to_string(),
        "togglefloating" | "setfloating" => "Toggle floating / tiled".to_string(),
        "fullscreen" => "Toggle fullscreen".to_string(),
        "pseudo" => "Toggle pseudotile".to_string(),
        "togglesplit" | "swapsplit" => "Swap dwindle split".to_string(),
        "launcher" | "summonlauncher" => "Summon launcher".to_string(),
        "files" | "openfiles" => "Open file manager".to_string(),
        "settings" | "opensettings" => "Open settings".to_string(),
        "notifications" | "notificationcenter" => "Toggle notification center".to_string(),
        "cheatsheet" | "showbinds" | "binds" => "Toggle keybind cheat-sheet".to_string(),
        "movefocus" => format!("Focus {}", a.to_ascii_uppercase()),
        "movewindow" => {
            if a.is_empty() {
                "Drag window".to_string()
            } else {
                format!("Move window {}", a.to_ascii_uppercase())
            }
        }
        "resizewindow" => "Resize window (mouse)".to_string(),
        "resizeactive" => format!("Resize active ({a})"),
        "moveactive" => format!("Move active ({a})"),
        "workspace" => format!("Switch to workspace {a}"),
        "movetoworkspace" => format!("Send to workspace {a}"),
        "movetoworkspacesilent" => format!("Send silently to workspace {a}"),
        "submap" => {
            if a.is_empty() || a.eq_ignore_ascii_case("reset") {
                "Exit modal layer".to_string()
            } else {
                format!("Enter '{a}' mode")
            }
        }
        "exit" => "Exit ChickenWM".to_string(),
        _ => {
            if a.is_empty() {
                d
            } else {
                format!("{d} {a}")
            }
        }
    }
}

pub fn build_cheatsheet_sections(rules: &[BindRule], active_submap: &str) -> Vec<CheatSheetSection> {
    let mut apps = Vec::new();
    let mut windows = Vec::new();
    let mut nav = Vec::new();
    let mut chains = Vec::new();
    let mut modal_map: std::collections::BTreeMap<String, Vec<(String, String)>> =
        std::collections::BTreeMap::new();

    let mut ws_switch_seen = false;
    let mut ws_move_seen = false;

    for r in rules {
        let chord = format_chord_label(&r.mods, &r.key);
        let desc = format_action_description(&r.dispatcher, &r.arg);
        if !r.submap.is_empty() {
            modal_map
                .entry(r.submap.clone())
                .or_default()
                .push((chord, desc));
            continue;
        }
        if r.key.contains('>') || r.dispatcher.eq_ignore_ascii_case("submap") {
            chains.push((chord, desc));
            continue;
        }
        let d = r.dispatcher.trim().to_ascii_lowercase();
        match d.as_str() {
            "workspace" if r.arg.trim().parse::<u32>().is_ok() => {
                if !ws_switch_seen {
                    ws_switch_seen = true;
                    let prefix = format_chord_label(&r.mods, "1..0");
                    nav.push((prefix, "Switch to workspace 1..10".to_string()));
                }
            }
            "movetoworkspace" if r.arg.trim().parse::<u32>().is_ok() => {
                if !ws_move_seen {
                    ws_move_seen = true;
                    let prefix = format_chord_label(&r.mods, "1..0");
                    nav.push((prefix, "Move window to workspace 1..10".to_string()));
                }
            }
            "exec" | "execr" | "launcher" | "summonlauncher" | "files" | "openfiles"
            | "settings" | "opensettings" | "notifications" | "notificationcenter"
            | "cheatsheet" | "showbinds" | "binds" | "exit" => {
                apps.push((chord, desc));
            }
            "movefocus" | "workspace" | "movetoworkspacesilent" => {
                nav.push((chord, desc));
            }
            _ => {
                windows.push((chord, desc));
            }
        }
    }

    let mut sections = Vec::new();
    if !apps.is_empty() {
        sections.push(CheatSheetSection {
            title: "Apps & Shell".to_string(),
            entries: apps,
        });
    }
    if !windows.is_empty() {
        sections.push(CheatSheetSection {
            title: "Windows & Layout".to_string(),
            entries: windows,
        });
    }
    if !nav.is_empty() {
        sections.push(CheatSheetSection {
            title: "Focus & Workspaces".to_string(),
            entries: nav,
        });
    }
    if !chains.is_empty() {
        sections.push(CheatSheetSection {
            title: "Chain Chords".to_string(),
            entries: chains,
        });
    }
    for (sub, entries) in modal_map {
        let active_tag = if active_submap.eq_ignore_ascii_case(&sub) {
            " (ACTIVE)"
        } else {
            ""
        };
        sections.push(CheatSheetSection {
            title: format!("Modal Layer: {sub}{active_tag}"),
            entries,
        });
    }
    sections
}

pub fn toggle_cheatsheet_overlay(rules: Vec<BindRule>, active_submap: String) {
    if close_cheatsheet_if_open() {
        return;
    }
    let sections = build_cheatsheet_sections(&rules, &active_submap);
    *data_store().lock() = (sections, active_submap);

    let _ = thread::Builder::new()
        .name("chickenwm-binds-cheatsheet".to_string())
        .spawn(|| unsafe {
            let hinstance = match GetModuleHandleW(None) {
                Ok(h) => h,
                Err(_) => return,
            };
            let class_name = w!("ChickenWMBindsCheatSheet");
            let wc = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(cheatsheet_wnd_proc),
                hInstance: hinstance.into(),
                lpszClassName: class_name,
                ..Default::default()
            };
            let _ = RegisterClassW(&wc);

            let screen_w = GetSystemMetrics(SM_CXSCREEN).max(1024);
            let screen_h = GetSystemMetrics(SM_CYSCREEN).max(768);
            let win_w = 960.min(screen_w - 80);
            let win_h = 620.min(screen_h - 80);
            let win_x = (screen_w - win_w) / 2;
            let win_y = (screen_h - win_h) / 2;

            let Ok(hwnd) = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                class_name,
                w!("ChickenWM Keybind Cheat-Sheet"),
                WS_POPUP | WS_VISIBLE,
                win_x,
                win_y,
                win_w,
                win_h,
                None,
                None,
                hinstance,
                None,
            ) else {
                return;
            };

            CHEATSHEET_HWND.store(hwnd.0 as isize, Ordering::Release);
            let pref = DWMWCP_ROUND;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &pref as *const _ as *const _,
                std::mem::size_of_val(&pref) as u32,
            );
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
            let _ = InvalidateRect(hwnd, None, true);

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&mut msg);
            }
            CHEATSHEET_HWND.store(0, Ordering::Release);
        });
}

fn wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn draw_text_line(hdc: HDC, text: &str, mut rect: RECT, color: u32) {
    unsafe {
        SetTextColor(hdc, COLORREF(color));
        let mut buf: Vec<u16> = text.encode_utf16().collect();
        let _ = DrawTextW(
            hdc,
            &mut buf,
            &mut rect,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
    }
}

unsafe extern "system" fn cheatsheet_wnd_proc(
    hwnd: HWND,
    msg: u32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_LBUTTONDOWN | WM_RBUTTONDOWN => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_KEYDOWN => {
                let vk = w_param.0 as u16;
                if vk == VK_ESCAPE.0 || vk == VK_F1.0 {
                    let _ = DestroyWindow(hwnd);
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                SetBkMode(hdc, TRANSPARENT);

                let bg_brush = CreateSolidBrush(COLORREF(0x00201B18));
                let card_brush = CreateSolidBrush(COLORREF(0x002C2622));
                let badge_brush = CreateSolidBrush(COLORREF(0x003D352E));
                let border_pen = CreatePen(PS_SOLID, 1, COLORREF(0x004A4038));
                FillRect(hdc, &ps.rcPaint, bg_brush);

                let title_name = wide_null("Segoe UI Semibold");
                let body_name = wide_null("Segoe UI");
                let font_title = CreateFontW(
                    -22,
                    0,
                    0,
                    0,
                    FW_BOLD.0 as i32,
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET.0 as u32,
                    OUT_DEFAULT_PRECIS.0 as u32,
                    CLIP_DEFAULT_PRECIS.0 as u32,
                    CLEARTYPE_QUALITY.0 as u32,
                    0,
                    PCWSTR(title_name.as_ptr()),
                );
                let font_sec = CreateFontW(
                    -15,
                    0,
                    0,
                    0,
                    FW_BOLD.0 as i32,
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET.0 as u32,
                    OUT_DEFAULT_PRECIS.0 as u32,
                    CLIP_DEFAULT_PRECIS.0 as u32,
                    CLEARTYPE_QUALITY.0 as u32,
                    0,
                    PCWSTR(title_name.as_ptr()),
                );
                let font_body = CreateFontW(
                    -13,
                    0,
                    0,
                    0,
                    FW_MEDIUM.0 as i32,
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET.0 as u32,
                    OUT_DEFAULT_PRECIS.0 as u32,
                    CLIP_DEFAULT_PRECIS.0 as u32,
                    CLEARTYPE_QUALITY.0 as u32,
                    0,
                    PCWSTR(body_name.as_ptr()),
                );
                let font_sub = CreateFontW(
                    -13,
                    0,
                    0,
                    0,
                    FW_NORMAL.0 as i32,
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET.0 as u32,
                    OUT_DEFAULT_PRECIS.0 as u32,
                    CLIP_DEFAULT_PRECIS.0 as u32,
                    CLEARTYPE_QUALITY.0 as u32,
                    0,
                    PCWSTR(body_name.as_ptr()),
                );

                let (sections, active_submap) = data_store().lock().clone();
                let _ = SelectObject(hdc, font_title);
                let header = if active_submap.is_empty() {
                    "Keybind Cheat-Sheet".to_string()
                } else {
                    format!("Keybind Cheat-Sheet  •  Mode: {}", active_submap.to_uppercase())
                };
                draw_text_line(
                    hdc,
                    &header,
                    RECT {
                        left: 24,
                        top: 16,
                        right: ps.rcPaint.right - 260,
                        bottom: 48,
                    },
                    0x00F2EFE9,
                );

                let _ = SelectObject(hdc, font_sub);
                draw_text_line(
                    hdc,
                    "Click anywhere or press Esc / Super+F1 to close",
                    RECT {
                        left: ps.rcPaint.right - 310,
                        top: 18,
                        right: ps.rcPaint.right - 20,
                        bottom: 48,
                    },
                    0x00A49B90,
                );

                let cols = 2;
                let pad = 20;
                let col_w = (ps.rcPaint.right - pad * (cols + 1)) / cols;
                let mut col_y = [58i32; 2];

                let _ = SelectObject(hdc, border_pen);
                for sec in &sections {
                    let col: usize = if col_y[0] <= col_y[1] { 0 } else { 1 };
                    let x0 = pad + (col as i32) * (col_w + pad);
                    let y0 = col_y[col];
                    let rows = sec.entries.len().min(10) as i32;
                    let card_h = 36 + rows * 24 + 10;
                    let x1 = x0 + col_w;
                    let y1 = y0 + card_h;

                    let _ = SelectObject(hdc, HBRUSH(card_brush.0));
                    let _ = RoundRect(hdc, x0, y0, x1, y1, 12, 12);

                    let _ = SelectObject(hdc, font_sec);
                    draw_text_line(
                        hdc,
                        &sec.title,
                        RECT {
                            left: x0 + 14,
                            top: y0 + 8,
                            right: x1 - 14,
                            bottom: y0 + 32,
                        },
                        0x0065B8F6,
                    );

                    for (idx, (chord, desc)) in sec.entries.iter().take(10).enumerate() {
                        let ry = y0 + 36 + (idx as i32) * 24;
                        let badge_w = 155;
                        let _ = SelectObject(hdc, HBRUSH(badge_brush.0));
                        let _ = RoundRect(hdc, x0 + 12, ry + 2, x0 + 12 + badge_w, ry + 22, 6, 6);

                        let _ = SelectObject(hdc, font_body);
                        draw_text_line(
                            hdc,
                            chord,
                            RECT {
                                left: x0 + 18,
                                top: ry + 2,
                                right: x0 + 10 + badge_w,
                                bottom: ry + 22,
                            },
                            0x0078D2F8,
                        );

                        let _ = SelectObject(hdc, font_sub);
                        draw_text_line(
                            hdc,
                            desc,
                            RECT {
                                left: x0 + 22 + badge_w,
                                top: ry + 2,
                                right: x1 - 12,
                                bottom: ry + 22,
                            },
                            0x00DDD8D0,
                        );
                    }

                    col_y[col] = y1 + 14;
                }

                let _ = DeleteObject(font_title);
                let _ = DeleteObject(font_sec);
                let _ = DeleteObject(font_body);
                let _ = DeleteObject(font_sub);
                let _ = DeleteObject(border_pen);
                let _ = DeleteObject(badge_brush);
                let _ = DeleteObject(card_brush);
                let _ = DeleteObject(bg_brush);

                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, w_param, l_param),
        }
    }
}
