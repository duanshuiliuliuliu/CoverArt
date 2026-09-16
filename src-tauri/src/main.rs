// CoverArt 桌面版
//
// 窗口：固定尺寸（由托盘里的「缩放」决定）、无边框、不可拖边缩放，但整窗可拖动。
// 托盘：左键单击显示/隐藏；右键菜单有「显示/隐藏」「缩放 50~150%」「退出」。
// 关闭：右上角的 ✕、Alt+F4 都只是收起到托盘，真正退出走托盘菜单的「退出」。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{MouseButton, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, LogicalSize, Manager, WebviewWindow};

/// 100% 时的窗口边长（逻辑像素）。界面按同比例 zoom，所以 CSS 视口始终是 560。
const BASE: f64 = 560.0;
/// 托盘里可选的缩放档位
const SCALES: [u32; 5] = [50, 75, 100, 125, 150];

struct Prefs {
    scale: Mutex<u32>,
    ontop: Mutex<bool>,
    checks: Mutex<Vec<(u32, CheckMenuItem<tauri::Wry>)>>,
    toggle: Mutex<Option<MenuItem<tauri::Wry>>>,
    ontop_item: Mutex<Option<CheckMenuItem<tauri::Wry>>>,
}

fn prefs_file(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_config_dir().ok().map(|d| d.join("prefs.json"))
}

/// 读偏好：(缩放档位, 是否置顶)
fn load_prefs(app: &AppHandle) -> (u32, bool) {
    let Some(path) = prefs_file(app) else { return (100, false) };
    let Ok(text) = fs::read_to_string(path) else { return (100, false) };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else { return (100, false) };
    let scale = json.get("scale").and_then(|v| v.as_u64()).unwrap_or(100) as u32;
    let scale = if SCALES.contains(&scale) { scale } else { 100 };
    let ontop = json.get("alwaysOnTop").and_then(|v| v.as_bool()).unwrap_or(false);
    (scale, ontop)
}

/// 两个偏好一起写盘（改任意一个都重写整份，字段少、不折腾）
fn save_prefs(app: &AppHandle) {
    let state = app.try_state::<Prefs>();
    let scale = state
        .as_ref()
        .and_then(|s| s.scale.lock().ok().map(|v| *v))
        .unwrap_or(100);
    let ontop = state
        .as_ref()
        .and_then(|s| s.ontop.lock().ok().map(|v| *v))
        .unwrap_or(false);
    let Some(path) = prefs_file(app) else { return };
    if let Some(dir) = path.parent() { let _ = fs::create_dir_all(dir); }
    let _ = fs::write(
        path,
        format!("{{\n  \"scale\": {scale},\n  \"alwaysOnTop\": {ontop}\n}}\n"),
    );
}

/// 缩放 = 窗口尺寸等比变化 + 界面 zoom 同比例变化，
/// 于是「窗口 = 封面」这个比例在任何档位都成立，CSS 里的 560 视口也始终不变。
fn apply_scale(app: &AppHandle, scale: u32) {
    if let Some(win) = app.get_webview_window("main") {
        let factor = scale as f64 / 100.0;
        let _ = win.set_size(LogicalSize::new(BASE * factor, BASE * factor));
        let _ = win.set_zoom(factor);
    }
    if let Some(state) = app.try_state::<Prefs>() {
        if let Ok(checks) = state.checks.lock() {
            for (value, item) in checks.iter() { let _ = item.set_checked(*value == scale); }
        }
        if let Ok(mut current) = state.scale.lock() { *current = scale; }
    }
    save_prefs(app);
}

/// 置顶：窗口层级 + 托盘菜单里的勾 + 记盘
fn set_ontop(app: &AppHandle, on: bool) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.set_always_on_top(on);
    }
    if let Some(state) = app.try_state::<Prefs>() {
        if let Ok(mut current) = state.ontop.lock() { *current = on; }
        if let Ok(item) = state.ontop_item.lock() {
            if let Some(item) = item.as_ref() { let _ = item.set_checked(on); }
        }
    }
    save_prefs(app);
}

fn sync_toggle_label(app: &AppHandle) {
    let visible = app
        .get_webview_window("main")
        .map(|w| w.is_visible().unwrap_or(false))
        .unwrap_or(false);
    if let Some(state) = app.try_state::<Prefs>() {
        if let Ok(guard) = state.toggle.lock() {
            if let Some(item) = guard.as_ref() {
                let _ = item.set_text(if visible { "隐藏" } else { "显示" });
            }
        }
    }
}

fn toggle_window(app: &AppHandle) {
    let Some(win) = app.get_webview_window("main") else { return };
    if win.is_visible().unwrap_or(false) {
        let _ = win.hide();
    } else {
        let _ = win.show();
        let _ = win.set_focus();
    }
    sync_toggle_label(app);
}

/// 界面右上角的 ✕：收起到托盘（不退出）
#[tauri::command]
fn hide_to_tray(window: WebviewWindow) {
    let _ = window.hide();
    sync_toggle_label(window.app_handle());
}

/// 整窗拖动：界面判断「按住并移动超过几个像素」之后才调它，点一下仍然是点击
#[tauri::command]
fn start_drag(window: WebviewWindow) {
    let _ = window.start_dragging();
}

/// 用系统默认浏览器打开外链（背面的 Apple Music / 原图 3000）
#[tauri::command]
fn open_external(app: AppHandle, url: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    app.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())
}

fn build_tray(app: &AppHandle, current_scale: u32, current_ontop: bool) -> tauri::Result<()> {
    let toggle = MenuItem::with_id(app, "toggle", "隐藏", true, None::<&str>)?;
    let ontop = CheckMenuItem::with_id(app, "ontop", "置顶", true, current_ontop, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;

    let mut checks = Vec::new();
    for value in SCALES {
        let item = CheckMenuItem::with_id(
            app,
            format!("scale_{value}"),
            format!("{value}%"),
            true,
            value == current_scale,
            None::<&str>,
        )?;
        checks.push((value, item));
    }
    let refs: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> =
        checks.iter().map(|(_, item)| item as &dyn tauri::menu::IsMenuItem<tauri::Wry>).collect();
    let scale_menu = Submenu::with_items(app, "缩放", true, &refs)?;
    let menu = Menu::with_items(app, &[&toggle, &ontop, &scale_menu, &separator, &quit])?;

    if let Some(state) = app.try_state::<Prefs>() {
        if let Ok(mut guard) = state.toggle.lock() { *guard = Some(toggle); }
        if let Ok(mut guard) = state.ontop_item.lock() { *guard = Some(ontop); }
        if let Ok(mut guard) = state.checks.lock() { *guard = checks; }
    }

    TrayIconBuilder::with_id("coverart-tray")
        .icon(app.default_window_icon().unwrap().clone())
        .tooltip("CoverArt")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| {
            let id = event.id.as_ref();
            if let Some(value) = id.strip_prefix("scale_") {
                if let Ok(scale) = value.parse::<u32>() { apply_scale(app, scale); }
                return;
            }
            match id {
                "toggle" => toggle_window(app),
                "ontop" => {
                    let now = app
                        .try_state::<Prefs>()
                        .and_then(|s| s.ontop.lock().ok().map(|v| *v))
                        .unwrap_or(false);
                    set_ontop(app, !now);
                }
                "quit" => app.exit(0),
                _ => {}
            }
        })
        // 托盘图标：左键双击才显示窗口（单击不做任何事，避免误触）
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::DoubleClick { button: MouseButton::Left, .. } = event {
                let app = tray.app_handle();
                if let Some(win) = app.get_webview_window("main") {
                    let _ = win.show();
                    let _ = win.unminimize();
                    let _ = win.set_focus();
                }
                sync_toggle_label(app);
            }
        })
        .build(app)?;
    Ok(())
}

fn main() {
    tauri::Builder::default()
        // 单实例：再启动一次时，第二个进程会直接退出，并让已经开着的那一个冒出来
        // （插件要放在最前面注册）
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.show();
                let _ = win.unminimize();
                let _ = win.set_focus();
            }
            sync_toggle_label(app);
        }))
        .plugin(tauri_plugin_opener::init())
        .manage(Prefs {
            scale: Mutex::new(100),
            ontop: Mutex::new(false),
            checks: Mutex::new(Vec::new()),
            toggle: Mutex::new(None),
            ontop_item: Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![hide_to_tray, start_drag, open_external])
        // Alt+F4 / 关闭请求：收起到托盘，不退出
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
                sync_toggle_label(window.app_handle());
            }
        })
        .setup(|app| {
            let handle = app.handle().clone();
            let (scale, ontop) = load_prefs(&handle);
            build_tray(&handle, scale, ontop)?;
            apply_scale(&handle, scale);
            set_ontop(&handle, ontop);          /* 恢复上次的置顶状态 */
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.show();
                let _ = win.set_focus();
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("CoverArt 启动失败");
}
