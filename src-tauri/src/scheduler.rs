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

/// 日志保留窗口与清理节奏：保留近 30 天，每 30 天清理一次（启动首轮即清一次）。
const LOG_RETAIN_SECS: i64 = 30 * 24 * 3600;
const LOG_CLEANUP_INTERVAL_SECS: i64 = 30 * 24 * 3600;

pub fn scheduler_loop(state: Arc<AppState>) {
    // 0 → 启动首轮即触发一次清理；之后每 LOG_CLEANUP_INTERVAL_SECS 一次。
    let mut last_cleanup: i64 = 0;
    while !state.shutdown.load(Ordering::SeqCst) {
        let now = db::now_secs();
        if now - last_cleanup >= LOG_CLEANUP_INTERVAL_SECS {
            // 清理失败（如 db 锁中毒）不该让调度线程崩溃，吞错打日志即可。
            {
                let g = state.db.lock();
                if let Err(e) = db::cleanup_old_logs(&g, now, LOG_RETAIN_SECS) {
                    eprintln!("[scheduler] 日志清理失败: {e}");
                }
            }
            last_cleanup = now;
        }
        let due = {
            let g = state.db.lock();
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
