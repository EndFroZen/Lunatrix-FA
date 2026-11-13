#![windows_subsystem = "windows"]

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, PhysicalPosition, State, WebviewWindow, WindowEvent, Wry};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use tauri_plugin_notification::NotificationExt;
use windows::Win32::Foundation::{COLORREF, HWND};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

const HIDDEN: u8 = 0; // overlay + toolbar hidden
const DRAW: u8 = 1; // overlay catches the mouse, pen cursor
const PASS: u8 = 2; // drawing stays, mouse clicks reach the screen below
static MODE: AtomicU8 = AtomicU8::new(HIDDEN);

const DEFAULT_HOTKEY: &str = "Ctrl+Alt+P";
struct Hotkey(Mutex<String>);
struct DrawItem(MenuItem<Wry>);

fn set_mode(app: &AppHandle, m: u8) {
    let (Some(ov), Some(tb)) = (app.get_webview_window("main"), app.get_webview_window("toolbar")) else { return };
    MODE.store(m, Ordering::Relaxed);
    let _ = ov.set_ignore_cursor_events(m == PASS);
    match m {
        HIDDEN => {
            let _ = ov.eval("clearAll()");
            let _ = ov.hide();
            let _ = tb.hide();
        }
        DRAW => {
            let _ = tb.show();
            let _ = ov.show();
            let _ = ov.set_focus();
            let _ = ov.eval("if (st.tool === 'zoom') startZoom()"); // magnifier: re-grab the screen
        }
        _ => {}
    }
    not_occluding(&ov);
    raise(&tb); // overlay.show() put itself above the toolbar
    let name = ["hidden", "draw", "pass"][m as usize];
    let _ = tb.eval(&format!("setMode('{name}')"));
}

fn hwnd(w: &WebviewWindow) -> Option<HWND> {
    w.hwnd().ok().map(|h| HWND(h.0))
}

/// Chrome/Edge stop painting video (white box) when a full-screen opaque window covers them.
/// Chromium ignores layered windows with alpha < 255, so mark ours 254 (invisible difference).
/// Re-applied every mode change because set_ignore_cursor_events rewrites the ex-style.
fn not_occluding(w: &WebviewWindow) {
    let Some(h) = hwnd(w) else { return };
    unsafe {
        let ex = GetWindowLongW(h, GWL_EXSTYLE);
        SetWindowLongW(h, GWL_EXSTYLE, ex | WS_EX_LAYERED.0 as i32);
        let _ = SetLayeredWindowAttributes(h, COLORREF(0), 254, LWA_ALPHA);
    }
}

/// Bring to the top of the topmost band without stealing focus.
fn raise(w: &WebviewWindow) {
    let Some(h) = hwnd(w) else { return };
    unsafe {
        let _ = SetWindowPos(h, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    }
}

fn hotkey_file(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_config_dir().ok().map(|d| d.join("hotkey.txt"))
}

fn load_hotkey(app: &AppHandle) -> String {
    hotkey_file(app)
        .and_then(|f| std::fs::read_to_string(f).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| Shortcut::from_str(s).is_ok())
        .unwrap_or_else(|| DEFAULT_HOTKEY.into())
}

fn draw_label(hotkey: &str) -> String {
    format!("วาด / ใช้จอ ({hotkey})")
}

#[tauri::command]
fn toggle(app: AppHandle) {
    let next = if MODE.load(Ordering::Relaxed) == DRAW { PASS } else { DRAW };
    set_mode(&app, next);
}

#[tauri::command]
fn start_draw(app: AppHandle) {
    set_mode(&app, DRAW);
}

#[tauri::command]
fn close(app: AppHandle) {
    set_mode(&app, HIDDEN);
}

/// Set a layered window's opacity (254 = visible, see not_occluding; 0 = gone, still on top).
fn set_alpha(w: &WebviewWindow, a: u8) {
    if let Some(h) = hwnd(w) {
        unsafe {
            let _ = SetLayeredWindowAttributes(h, COLORREF(0), a, LWA_ALPHA);
        }
    }
}

/// Screen pixels under the calling window as [w u32][h u32][RGBA...], *without* our windows:
/// overlay + toolbar go fully transparent for the grab (DWM still composites layered windows),
/// so the page can composite its own canvas on top without zooming/doubling itself.
#[tauri::command]
async fn capture(app: AppHandle, window: WebviewWindow) -> Result<tauri::ipc::Response, String> {
    let ours: Vec<_> = ["main", "toolbar"].iter().filter_map(|l| app.get_webview_window(l)).collect();
    ours.iter().for_each(|w| set_alpha(w, 0));
    // ponytail: fixed wait for DWM to drop our windows (~3 frames), DwmFlush if this ever blinks wrong
    std::thread::sleep(std::time::Duration::from_millis(50));
    let shot = grab(&window);
    ours.iter().for_each(|w| set_alpha(w, 254));
    shot
}

fn grab(window: &WebviewWindow) -> Result<tauri::ipc::Response, String> {
    let p = window.outer_position().map_err(|e| e.to_string())?;
    let s = window.outer_size().map_err(|e| e.to_string())?;
    let (w, h) = (s.width as i32, s.height as i32);
    let mut buf = vec![0u8; 8 + (w * h * 4) as usize];
    buf[..4].copy_from_slice(&(w as u32).to_le_bytes());
    buf[4..8].copy_from_slice(&(h as u32).to_le_bytes());
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(mem, bmp.into());
        let ok = BitBlt(mem, 0, 0, w, h, Some(screen), p.x, p.y, ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0)); // incl. other apps' layered windows
        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        GetDIBits(mem, bmp, 0, h as u32, Some(buf[8..].as_mut_ptr().cast()), &mut bi, DIB_RGB_COLORS);
        SelectObject(mem, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        ok.map_err(|e| e.to_string())?;
    }
    for px in buf[8..].chunks_exact_mut(4) {
        px.swap(0, 2); // BGRA -> RGBA
        px[3] = 255;
    }
    Ok(tauri::ipc::Response::new(buf))
}

/// Raw PNG body, file name in the "name" header. Saved to Pictures\Lunatrix.
#[tauri::command]
fn save_png(app: AppHandle, request: tauri::ipc::Request) -> Result<String, String> {
    let tauri::ipc::InvokeBody::Raw(png) = request.body() else { return Err("ไม่มีรูป".into()) };
    let name = request
        .headers()
        .get("name")
        .and_then(|v| v.to_str().ok())
        .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')) // no paths
        .ok_or("ชื่อไฟล์ไม่ถูกต้อง")?;
    let dir = app.path().picture_dir().map_err(|e| e.to_string())?.join("Lunatrix");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let f = dir.join(format!("{name}.png"));
    std::fs::write(&f, png).map_err(|e| format!("บันทึกไม่สำเร็จ: {e}"))?;
    Ok(f.display().to_string())
}

#[tauri::command]
fn get_hotkey(hk: State<Hotkey>) -> String {
    hk.0.lock().unwrap().clone()
}

#[tauri::command]
fn set_hotkey(app: AppHandle, hk: State<Hotkey>, hotkey: String) -> Result<String, String> {
    let new = Shortcut::from_str(&hotkey).map_err(|e| format!("รูปแบบปุ่มไม่ถูกต้อง: {e}"))?;
    let mut cur = hk.0.lock().unwrap();
    let gs = app.global_shortcut();
    let _ = gs.unregister_all();
    if let Err(e) = gs.register(new) {
        let _ = gs.register(Shortcut::from_str(&cur).unwrap()); // put the old one back
        return Err(format!("ใช้ปุ่มนี้ไม่ได้ อาจมีโปรแกรมอื่นใช้อยู่ ({e})"));
    }
    *cur = hotkey.clone();
    if let Some(f) = hotkey_file(&app) {
        let _ = std::fs::create_dir_all(f.parent().unwrap());
        std::fs::write(f, &hotkey).map_err(|e| format!("บันทึกไม่สำเร็จ: {e}"))?;
    }
    let _ = app.state::<DrawItem>().0.set_text(draw_label(&hotkey));
    Ok(hotkey)
}

#[tauri::command]
fn get_autostart(app: AppHandle) -> bool {
    app.autolaunch().is_enabled().unwrap_or(false)
}

#[tauri::command]
fn set_autostart(app: AppHandle, on: bool) -> Result<(), String> {
    let al = app.autolaunch();
    if on { al.enable() } else { al.disable() }.map_err(|e| format!("ตั้งค่าไม่สำเร็จ: {e}"))
}

/// First run: turn autostart on. Every run: if on, re-register so the entry points at this exe
/// (e.g. after reinstalling to another folder).
fn init_autostart(app: &AppHandle) {
    let al = app.autolaunch();
    let marker = app.path().app_config_dir().ok().map(|d| d.join("autostart-initialized"));
    let first_run = marker.as_ref().is_some_and(|m| !m.exists());
    if first_run || al.is_enabled().unwrap_or(false) {
        let _ = al.enable();
    }
    if let (true, Some(m)) = (first_run, marker) {
        let _ = std::fs::create_dir_all(m.parent().unwrap());
        let _ = std::fs::write(m, "");
    }
}

#[test]
fn settings_page_hotkeys_parse() {
    // strings exactly as settings.html builds them from KeyboardEvent.code
    for s in ["Ctrl+Alt+P", "Ctrl+Shift+1", "Alt+F5", "F9", "Super+Shift+D", "Ctrl+Minus", "Ctrl+Alt+ArrowUp"] {
        assert!(Shortcut::from_str(s).is_ok(), "{s} should parse");
    }
    assert!(Shortcut::from_str("Ctrl+Alt+NotAKey").is_err());
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, None))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                // only one shortcut is ever registered: the user's hotkey
                .with_handler(|app, _, e| {
                    if e.state() == ShortcutState::Pressed {
                        toggle(app.clone());
                    }
                })
                .build(),
        )
        .invoke_handler(tauri::generate_handler![toggle, start_draw, close, capture, save_png, get_hotkey, set_hotkey, get_autostart, set_autostart])
        .on_window_event(|w, e| {
            // closing settings just hides it; the app lives in the tray
            if let WindowEvent::CloseRequested { api, .. } = e {
                if w.label() == "settings" {
                    api.prevent_close();
                    let _ = w.hide();
                }
            }
        })
        .setup(|app| {
            let handle = app.handle();
            init_autostart(handle);
            let hotkey = load_hotkey(handle);
            if let Err(e) = app.global_shortcut().register(Shortcut::from_str(&hotkey)?) {
                eprintln!("hotkey {hotkey} not registered: {e}");
            }

            // toolbar: left edge, vertically centered on the primary screen
            let tb = app.get_webview_window("toolbar").unwrap();
            let ov = app.get_webview_window("main").unwrap();
            if let (Some(t), Some(o)) = (hwnd(&tb), hwnd(&ov)) {
                unsafe {
                    // owned by the overlay => always stacked above it, even after the overlay is clicked
                    SetWindowLongPtrW(t, GWLP_HWNDPARENT, o.0 as isize);
                    // clicking the toolbar never steals focus => overlay keeps its keyboard shortcuts
                    let ex = GetWindowLongW(t, GWL_EXSTYLE);
                    SetWindowLongW(t, GWL_EXSTYLE, ex | WS_EX_NOACTIVATE.0 as i32);
                }
            }
            not_occluding(&tb); // layered => capture() can fade it out
            if let Some(mon) = tb.primary_monitor()? {
                let y = mon.position().y + (mon.size().height as i32 - tb.outer_size()?.height as i32) / 2;
                tb.set_position(PhysicalPosition::new(mon.position().x, y))?;
            }

            let draw = MenuItem::with_id(app, "draw", draw_label(&hotkey), true, None::<&str>)?;
            let settings = MenuItem::with_id(app, "settings", "ตั้งค่า…", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "ออก (Quit)", true, None::<&str>)?;
            TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("Lunatrix — คลิกเพื่อวาด")
                .menu(&Menu::with_items(app, &[&draw, &settings, &quit])?)
                .show_menu_on_left_click(false) // left click = pen, right click = menu
                .on_tray_icon_event(|tray, e| {
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                        toggle(tray.app_handle().clone());
                    }
                })
                .on_menu_event(|app, e| match e.id().as_ref() {
                    "draw" => toggle(app.clone()),
                    "settings" => {
                        if let Some(w) = app.get_webview_window("settings") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            let _ = app
                .notification()
                .builder()
                .title("Lunatrix พร้อมใช้งาน")
                .body(format!("กด {hotkey} หรือคลิกไอคอนที่ถาดระบบเพื่อวาดบนจอ"))
                .show();

            app.manage(Hotkey(Mutex::new(hotkey)));
            app.manage(DrawItem(draw));
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("failed to start");
}
