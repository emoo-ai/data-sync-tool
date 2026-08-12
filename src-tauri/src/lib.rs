//! Emoo Data Sync —— M2 后端入口。
//!
//! 全局状态经 `.manage(Arc<AppState>)` 注入；命令通过 `tauri::State<'_, Arc<AppState>>` 取用。
//! 安全约束：只调 Emoo Open API；`emooSearch` 原生代码仅供理解。

mod config;
mod db;
mod emoo;
mod scheduler;
mod state;
mod sync_engine;
mod task;

use crate::emoo::EmooClient;
use crate::state::AppState;
use crate::task::{NewTask, Task, TaskPatch};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use parking_lot::{Mutex, RwLock};

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, TrayIconBuilder, TrayIconEvent};
use tauri::{Manager, WindowEvent};

// ---------------- 鉴权 / 配置（M1 保留） ----------------

#[tauri::command]
async fn test_connection(base_url: String, api_key: String) -> Result<Vec<emoo::FolderItem>, String> {
    EmooClient::new(base_url, api_key, None)
        .list_folder_items(None)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn list_folder_items(
    base_url: String,
    api_key: String,
    folder_id: Option<i64>,
) -> Result<Vec<emoo::FolderItem>, String> {
    EmooClient::new(base_url, api_key, None)
        .list_folder_items(folder_id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn load_config(app: tauri::AppHandle) -> Result<config::Config, String> {
    config::load(&app).map_err(|e| e.to_string())
}

#[tauri::command]
fn save_config(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<AppState>>,
    base_url: String,
    api_key: String,
    emoo_user_id: Option<String>,
) -> Result<(), String> {
    config::save(
        &app,
        base_url.clone(),
        api_key.clone(),
        emoo_user_id.clone(),
    )
    .map_err(|e| e.to_string())?;
    // 写完 config 立即让已建好的 EmooClient 换上新的 base/key/user_id（复用 reqwest 连接池）
    let mut c = state.emoo.write();
    c.reconfigure(base_url, api_key, emoo_user_id);
    Ok(())
}

#[tauri::command]
fn set_close_behavior(app: tauri::AppHandle, close_to_tray: bool) -> Result<(), String> {
    config::set_close_behavior(&app, close_to_tray).map_err(|e| e.to_string())
}

// ---------------- 任务管理 ----------------

#[tauri::command]
fn list_tasks(state: tauri::State<'_, Arc<AppState>>) -> Result<Vec<Task>, String> {
    let g = state.db.lock();
    db::list_tasks(&g).map_err(|e| e.to_string())
}

#[tauri::command]
fn create_task(state: tauri::State<'_, Arc<AppState>>, new: NewTask) -> Result<Task, String> {
    // 文件夹任务硬上限兜底（前端已校验，后端再挡一次）
    if new.is_dir {
        if let Some(n) = sync_engine::list_syncable_files(std::path::Path::new(&new.local_path)) {
            if n.len() > task::FOLDER_FILE_CAP {
                return Err(format!(
                    "文件夹内可同步文件 {} 个，超过 {} 上限",
                    n.len(),
                    task::FOLDER_FILE_CAP
                ));
            }
        }
    }
    let g = state.db.lock();
    db::insert_task(&g, &new).map_err(|e| e.to_string())
}

#[tauri::command]
fn update_task(
    state: tauri::State<'_, Arc<AppState>>,
    id: i64,
    patch: TaskPatch,
) -> Result<Task, String> {
    let g = state.db.lock();
    db::update_task(&g, id, &patch).map_err(|e| e.to_string())
}

#[tauri::command]
fn delete_task(state: tauri::State<'_, Arc<AppState>>, id: i64) -> Result<(), String> {
    // 同步中拒绝删除
    {
        let g = state.inflight.lock();
        if g.contains(&id) {
            return Err("任务正在同步，请稍后再试".to_string());
        }
    }
    let g = state.db.lock();
    db::delete_task(&g, id).map_err(|e| e.to_string())
}

#[tauri::command]
async fn sync_task_now(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<AppState>>,
    id: i64,
) -> Result<task::SyncOutcome, String> {
    let state = state.inner().clone();
    sync_engine::sync_task(&app, &state, id, sync_engine::TriggeredBy::Manual)
        .await
        .map_err(|e| e.to_string())
}

/// 串行同步所有「非 syncing」任务（前端「全部同步」按钮）。
#[tauri::command]
async fn sync_all(app: tauri::AppHandle, state: tauri::State<'_, Arc<AppState>>) -> Result<(), String> {
    let state = state.inner().clone();
    let tasks = {
        let g = state.db.lock();
        db::list_tasks(&g).map_err(|e| e.to_string())?
    };
    for t in tasks {
        if t.status == "syncing" {
            continue;
        }
        let _ = sync_engine::sync_task(&app, &state, t.id, sync_engine::TriggeredBy::Manual).await;
    }
    Ok(())
}

// ---------------- 查询 ----------------

#[tauri::command]
fn list_log(
    state: tauri::State<'_, Arc<AppState>>,
    task_id: Option<i64>,
    limit: Option<i64>,
) -> Result<Vec<task::LogEntry>, String> {
    let g = state.db.lock();
    db::recent_log(&g, task_id, limit.unwrap_or(200)).map_err(|e| e.to_string())
}

#[tauri::command]
fn list_file_records(
    state: tauri::State<'_, Arc<AppState>>,
    task_id: i64,
) -> Result<Vec<task::FileRecord>, String> {
    let g = state.db.lock();
    db::list_file_records(&g, task_id).map_err(|e| e.to_string())
}

#[tauri::command]
fn count_folder_files(path: String) -> Result<Option<usize>, String> {
    Ok(sync_engine::list_syncable_files(std::path::Path::new(&path)).map(|v| v.len()))
}

// ---------------- 通讯录 / 文档权限 ----------------

/// 通讯录成员（权限「指定」范围选择用）。
#[tauri::command]
async fn list_ws_users(
    state: tauri::State<'_, Arc<AppState>>,
    keyword: Option<String>,
    current_page: Option<i64>,
    page_size: Option<i64>,
) -> Result<Vec<emoo::WsUser>, String> {
    let client = state.emoo.read().clone();
    client
        .list_ws_users(keyword.as_deref(), page_size, current_page)
        .await
        .map_err(|e| e.to_string())
}

/// 设置任务的文档权限；permission=null 关闭权限控制。
#[tauri::command]
fn set_task_permission(
    state: tauri::State<'_, Arc<AppState>>,
    id: i64,
    permission: Option<task::PermissionSetting>,
) -> Result<task::Task, String> {
    let g = state.db.lock();
    db::set_task_permission(&g, id, &permission).map_err(|e| e.to_string())
}

// ---------------- 启动 ----------------

fn init_state(app: &tauri::AppHandle) -> Result<Arc<AppState>, Box<dyn std::error::Error>> {
    let dir = app.path().app_data_dir()?;
    std::fs::create_dir_all(&dir)?;
    let conn = db::open(&dir)?;
    let cfg = config::load(app)?;
    let emoo = EmooClient::new(cfg.base_url, cfg.api_key, cfg.emoo_user_id);
    Ok(Arc::new(AppState {
        app: app.clone(),
        emoo: Arc::new(RwLock::new(emoo)),
        db: Arc::new(Mutex::new(conn)),
        inflight: Arc::new(Mutex::new(Default::default())),
        shutdown: Arc::new(AtomicBool::new(false)),
    }))
}

// ---------------- 托盘 / 窗口 ----------------

/// 显示并聚焦主窗口（从最小化/隐藏状态唤起）。幂等：重复调用只 show，不会 toggle 抵消。
fn show_main_window(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// 构建系统托盘：左键唤起窗口，右键菜单（显示/退出）。
fn build_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    let show_item = MenuItem::with_id(app, "show", "显示主窗口", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show_item, &quit_item])?;

    TrayIconBuilder::with_id("main-tray")
        .icon(app.default_window_icon().cloned().unwrap())
        .tooltip("Emoo 数据同步")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_main_window(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            // macOS 上 Click 在按下/松开各触发一次，toggle 会被连点抵消；
            // 改为「总是唤起」，隐藏交给窗口 ×（关闭到托盘）。
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .setup(|app| {
            // macOS：纯托盘 app，Dock 不显示图标（Windows/Linux 不受影响）
            #[cfg(target_os = "macos")]
            let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            let state = init_state(app.handle())?;
            app.manage(state.clone());
            // 定时调度后台线程：每秒查到期任务并同步
            let st = state.clone();
            std::thread::spawn(move || scheduler::scheduler_loop(st));
            // 系统托盘（左键切换窗口，右键菜单显示/退出）
            build_tray(app.handle())?;
            Ok(())
        })
        .on_window_event(|window, event| {
            // 关闭(×)按 config.close_to_tray 决定：true=隐藏到托盘，false=放行退出
            if let WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    let hide = config::load(window.app_handle())
                        .map(|c| c.close_to_tray)
                        .unwrap_or(true);
                    if hide {
                        let _ = window.hide();
                        api.prevent_close();
                    }
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            // 鉴权 / 配置
            test_connection,
            list_folder_items,
            load_config,
            save_config,
            // 任务
            list_tasks,
            create_task,
            update_task,
            delete_task,
            sync_task_now,
            sync_all,
            // 查询
            list_log,
            list_file_records,
            count_folder_files,
            // 通讯录 / 文档权限
            list_ws_users,
            set_task_permission,
            // 托盘偏好
            set_close_behavior,
        ])
        .build(tauri::generate_context!())
        .expect("构建 Tauri 应用失败")
        .run(|app_handle, event| match event {
            tauri::RunEvent::Exit => {
                if let Some(state) = app_handle.try_state::<Arc<AppState>>() {
                    state.shutdown.store(true, Ordering::SeqCst);
                }
            }
            // macOS：点 Dock 图标重新激活已运行的应用 → 唤起主窗口（hide 后才能再唤起）
            #[cfg(target_os = "macos")]
            tauri::RunEvent::Reopen { .. } => show_main_window(app_handle),
            _ => {}
        });
}
