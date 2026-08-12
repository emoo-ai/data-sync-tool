//! 任务模型与状态常量。
//! 单文件 = `is_dir=false` 的任务；文件夹 = `is_dir=true`。
//! 可新建任意多条任务并存，互不影响。

use serde::{Deserialize, Serialize};

/// 文档权限设置（可见 / 可删除；可编辑固定为 none）。复用 emoo 模块的类型。
pub use crate::emoo::PermissionSetting;

pub const STATUS_IDLE: &str = "idle";
pub const STATUS_SYNCING: &str = "syncing";
pub const STATUS_PAUSED: &str = "paused";
pub const STATUS_ERROR: &str = "error";

/// 文件夹任务的顶层白名单文件硬上限（超出 → 报错，绝不部分同步）。
pub const FOLDER_FILE_CAP: usize = 20;

/// 同步调度间隔下限（秒）。
pub const SCHEDULE_MIN_INTERVAL: i64 = 60;

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub id: i64,
    pub name: String,
    pub local_path: String,
    pub is_dir: bool,
    pub target_folder_id: Option<i64>,
    pub schedule_enabled: bool,
    pub schedule_interval_secs: i64,
    pub status: String,
    pub status_message: String,
    pub last_synced_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    /// 每任务权限设置；None = 不做权限控制（不同步权限，保留 Emoo 默认）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<PermissionSetting>,
}

fn normalize_interval(v: Option<i64>) -> i64 {
    match v {
        Some(x) => x.max(SCHEDULE_MIN_INTERVAL),
        None => 600,
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewTask {
    pub name: String,
    pub local_path: String,
    pub is_dir: bool,
    pub target_folder_id: Option<i64>,
    #[serde(default)]
    pub schedule_enabled: Option<bool>,
    #[serde(default)]
    pub schedule_interval_secs: Option<i64>,
}

impl NewTask {
    pub fn schedule(&self) -> bool {
        self.schedule_enabled.unwrap_or(false)
    }
    pub fn interval(&self) -> i64 {
        normalize_interval(self.schedule_interval_secs)
    }
}

/// 局部更新（缺省字段 = 不动）。M2 不支持改 target_folder_id，要换目标请删后重建。
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TaskPatch {
    pub name: Option<String>,
    pub schedule_enabled: Option<bool>,
    pub schedule_interval_secs: Option<i64>,
}

impl TaskPatch {
    pub fn interval(&self) -> Option<i64> {
        self.schedule_interval_secs.map(|x| x.max(SCHEDULE_MIN_INTERVAL))
    }
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FileRecord {
    pub task_id: i64,
    pub relative_path: String,
    pub doc_key: String,
    pub content_hash: String,
    pub size: i64,
    pub mtime: i64,
    pub synced_at: i64,
    pub status: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEntry {
    pub id: i64,
    pub task_id: i64,
    pub ts: i64,
    pub level: String,
    pub message: String,
    pub detail: String,
}

/// 单次同步结果统计。
#[derive(Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct SyncOutcome {
    pub created: u32,
    pub updated: u32,
    pub skipped: u32,
    pub source_deleted: u32,
    pub failed: u32,
}
