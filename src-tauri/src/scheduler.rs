//! 定时调度循环：每秒查「启用定时且到期」的任务，到点 spawn 一次同步。
//!
//! 任务 `last_synced_at IS NULL`（从未同步）也算到期，因此新建/启用定时的任务
//! 会在约 1 秒内被调度一次（即「开始定时同步」的即时反馈）。
//! 单飞由 sync_engine 兜底，定时 / 手动不会重复跑同一任务。

use crate::db;
use crate::state::AppState;
use crate::sync_engine::{self, TriggeredBy};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

pub fn scheduler_loop(state: Arc<AppState>) {
    while !state.shutdown.load(Ordering::SeqCst) {
        let now = db::now_secs();
        let due = {
            let g = state.db.lock().unwrap();
            db::list_schedule_due(&g, now).unwrap_or_default()
        };
        for t in due {
            let app = state.app.clone();
            let st = state.clone();
            tauri::async_runtime::spawn(async move {
                let _ = sync_engine::sync_task(&app, &st, t.id, TriggeredBy::Schedule).await;
            });
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}
