//! 全局应用状态，经 `.manage(Arc<AppState>)` 注入，命令通过 `tauri::State<'_, Arc<AppState>>` 取用。

use crate::emoo::EmooClient;
use rusqlite::Connection;
use std::collections::HashSet;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, RwLock};
use tauri::AppHandle;

pub struct AppState {
    pub app: AppHandle,
    /// EmooClient：同步起手短暂读锁 clone（廉价），绝不长持有；save_config 用写锁 reconfigure。
    pub emoo: Arc<RwLock<EmooClient>>,
    /// SQLite：短临界区，绝不跨 `.await` 持锁。
    pub db: Arc<Mutex<Connection>>,
    /// 单飞：正在同步的任务 id 集合，手动 / 定时两路汇此去重。
    pub inflight: Arc<Mutex<HashSet<i64>>>,
    /// 退出标志：后台调度循环据此优雅停止。
    pub shutdown: Arc<AtomicBool>,
}

impl AppState {
    /// 尝试占用任务同步槽；已被占用返回 false（跳过本次触发）。
    pub fn try_acquire(&self, task_id: i64) -> bool {
        if let Ok(mut g) = self.inflight.lock() {
            if g.contains(&task_id) {
                return false;
            }
            g.insert(task_id);
            return true;
        }
        false
    }

    pub fn release(&self, task_id: i64) {
        if let Ok(mut g) = self.inflight.lock() {
            g.remove(&task_id);
        }
    }
}
