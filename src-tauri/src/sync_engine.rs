//! 同步引擎：单文件 / 文件夹批量同步、覆盖（新 doc + 删旧 doc）、
//! 本地删除策略（远端保留为备份）、>20 硬上限、单飞、重试、sha256 变更检测。

use crate::db::{self, now_secs};
use crate::emoo::EmooClient;
use crate::state::AppState;
use crate::task::*;
use anyhow::{anyhow, bail, Result};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

#[derive(Clone, Copy)]
pub enum TriggeredBy {
    Manual,
    Schedule,
}

#[derive(Clone, Copy)]
enum FileAction {
    Created,
    Updated,
    Skipped,
}

#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ProgressPayload {
    pub task_id: i64,
    pub phase: String,
    pub current: u32,
    pub total: u32,
    pub message: String,
}

#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct StatusPayload {
    pub task_id: i64,
    pub status: String,
    pub status_message: String,
    pub last_synced_at: Option<i64>,
}

/// 单飞守卫：任何出口（含 bail / panic）都释放任务槽。
struct ReleaseGuard<'a> {
    state: &'a AppState,
    id: i64,
}
impl Drop for ReleaseGuard<'_> {
    fn drop(&mut self) {
        self.state.release(self.id);
    }
}

/// 主入口：手动 / 监听 / 定时 / 启动 都汇到这里。
pub async fn sync_task(
    app: &AppHandle,
    state: &Arc<AppState>,
    task_id: i64,
    _by: TriggeredBy,
) -> Result<SyncOutcome> {
    // 单飞
    if !state.try_acquire(task_id) {
        emit_log(app, state, task_id, "info", "已有同步进行中，跳过本次触发", "");
        return Ok(SyncOutcome::default());
    }
    let _guard = ReleaseGuard {
        state: state.as_ref(),
        id: task_id,
    };

    // 取任务
    let task = {
        let g = state.db.lock().unwrap();
        db::get_task(&g, task_id)?
    }
    .ok_or_else(|| anyhow!("任务不存在 #{task_id}"))?;

    // 起手 clone 客户端（廉价：reqwest::Client 是 Arc），锁只在此瞬间持有
    let client = {
        state
            .emoo
            .read()
            .expect("emoo lock")
            .clone()
    };

    set_status(app, state, task_id, STATUS_SYNCING, "");

    let root = PathBuf::from(&task.local_path);
    if !root.exists() {
        let msg = "源路径找不到了";
        set_status(app, state, task_id, STATUS_PAUSED, msg);
        emit_log(app, state, task_id, "error", msg, "");
        let _ = app.emit(
            "task://paused",
            serde_json::json!({ "taskId": task_id, "reason": msg }),
        );
        bail!(msg);
    }

    let mut outcome = SyncOutcome::default();

    if task.is_dir {
        // 目录分支
        let files = match list_syncable_files(&root) {
            Some(f) => f,
            None => {
                let msg = "读取目录失败";
                set_status(app, state, task_id, STATUS_ERROR, msg);
                emit_log(app, state, task_id, "error", msg, "");
                bail!(msg);
            }
        };

        // 硬上限
        if files.len() > FOLDER_FILE_CAP {
            let msg = format!(
                "文件夹内可同步文件 {} 个，超过 {} 上限，已中止（不部分同步）",
                files.len(),
                FOLDER_FILE_CAP
            );
            set_status(app, state, task_id, STATUS_ERROR, &msg);
            emit_log(app, state, task_id, "error", &msg, "");
            bail!(msg);
        }

        let total = files.len() as u32;
        let on_disk: HashSet<String> = files.iter().cloned().collect();

        for (i, name) in files.iter().enumerate() {
            let abs = root.join(name);
            emit_progress(app, task_id, (i + 1) as u32, total, name);
            match sync_one_file(app, state, &client, task_id, task.target_folder_id, &abs, name, task.permission.as_ref())
                .await
            {
                Ok(FileAction::Created) => outcome.created += 1,
                Ok(FileAction::Updated) => outcome.updated += 1,
                Ok(FileAction::Skipped) => outcome.skipped += 1,
                Err(e) => {
                    outcome.failed += 1;
                    emit_log(
                        app,
                        state,
                        task_id,
                        "warn",
                        &format!("同步失败：{name}"),
                        &e.to_string(),
                    );
                }
            }
        }

        // 本地删除扫描：DB 里 synced 但磁盘已无 → 标 source_deleted，远端文档保留
        let recs = {
            let g = state.db.lock().unwrap();
            db::list_file_records(&g, task_id)?
        };
        for r in recs {
            if r.status == "synced" && !on_disk.contains(&r.relative_path) {
                {
                    let g = state.db.lock().unwrap();
                    let _ = db::mark_file_source_deleted(&g, task_id, &r.relative_path);
                }
                outcome.source_deleted += 1;
                emit_log(
                    app,
                    state,
                    task_id,
                    "warn",
                    &format!("本地文件已删除，远端文档保留为备份：{}", r.relative_path),
                    &format!("doc_key={}", r.doc_key),
                );
            }
        }
    } else {
        // 单文件分支
        let name = root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();
        emit_progress(app, task_id, 1, 1, &name);
        match sync_one_file(app, state, &client, task_id, task.target_folder_id, &root, &name, task.permission.as_ref())
            .await
        {
            Ok(FileAction::Created) => outcome.created += 1,
            Ok(FileAction::Updated) => outcome.updated += 1,
            Ok(FileAction::Skipped) => outcome.skipped += 1,
            Err(e) => {
                let msg = e.to_string();
                set_status(app, state, task_id, STATUS_ERROR, &msg);
                emit_log(app, state, task_id, "error", "同步失败", &msg);
                bail!(e);
            }
        }
    }

    // 成功收尾
    let now = now_secs();
    {
        let g = state.db.lock().unwrap();
        let _ = db::touch_task_synced(&g, task_id, now);
    }
    set_status(app, state, task_id, STATUS_IDLE, "");

    // 简短结果日志
    emit_log(
        app,
        state,
        task_id,
        "info",
        "同步完成",
        &format!(
            "新建 {} / 更新 {} / 跳过 {} / 本地删除 {} / 失败 {}",
            outcome.created,
            outcome.updated,
            outcome.skipped,
            outcome.source_deleted,
            outcome.failed
        ),
    );
    Ok(outcome)
}

async fn sync_one_file(
    app: &AppHandle,
    state: &Arc<AppState>,
    client: &EmooClient,
    task_id: i64,
    folder_id: Option<i64>,
    abs: &Path,
    rel: &str,
    permission: Option<&PermissionSetting>,
) -> Result<FileAction> {
    // 白名单兜底
    let (allowed, _, _) = crate::emoo::ext_info(rel);
    if !allowed {
        emit_log(app, state, task_id, "warn", &format!("非白名单扩展名，跳过：{rel}"), "");
        return Ok(FileAction::Skipped);
    }

    let md = std::fs::metadata(abs)?;
    let size = md.len() as i64;
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let prior = {
        let g = state.db.lock().unwrap();
        db::get_file_record(&g, task_id, rel)?
    };

    // 决定本次动作 + 当前应保护的 doc_key（新建=新 key；跳过/恢复=旧 key）。
    let (action, doc_key) =
        decide_action(app, state, client, task_id, folder_id, abs, rel, size, mtime, &prior)
            .await?;

    // 权限：每次同步都对当前文档重设 → Emoo 端手动改动会在下次同步被覆盖（以本工具为准）。
    // 失败只 warn（需工作区超管 + 调用者 open_id；不满足时给出明确提示）。
    if let Some(perm) = permission {
        if let Some(key) = doc_key.as_deref() {
            match client.set_kb_file_permission(key, perm).await {
                Ok(_) => emit_log(
                    app,
                    state,
                    task_id,
                    "info",
                    &format!("已设置文档权限：{rel}"),
                    "",
                ),
                Err(e) => emit_log(
                    app,
                    state,
                    task_id,
                    "warn",
                    &format!("文档权限设置失败（需工作区超管 + 调用者 open_id）：{rel}"),
                    &e.to_string(),
                ),
            }
        }
    }

    Ok(action)
}

/// 计算 (动作, 当前 doc_key)。封装预检 / hash / 新建 / 覆盖 / 记录更新。
async fn decide_action(
    app: &AppHandle,
    state: &Arc<AppState>,
    client: &EmooClient,
    task_id: i64,
    folder_id: Option<i64>,
    abs: &Path,
    rel: &str,
    size: i64,
    mtime: i64,
    prior: &Option<FileRecord>,
) -> Result<(FileAction, Option<String>)> {
    // (size,mtime) 预检：未变化直接跳过读 + hash
    if let Some(p) = prior {
        if p.status == "synced" && p.size == size && p.mtime == mtime {
            return Ok((FileAction::Skipped, Some(p.doc_key.clone())));
        }
    }

    // 读字节 + hash
    let bytes = std::fs::read(abs)?;
    let hash = sha256_hex(&bytes);

    // hash 未变（仅 mtime 变或恢复关联）
    if let Some(p) = prior {
        if p.content_hash == hash {
            let now = now_secs();
            let rec = FileRecord {
                task_id,
                relative_path: rel.to_string(),
                doc_key: p.doc_key.clone(),
                content_hash: hash,
                size,
                mtime,
                synced_at: now,
                status: "synced".to_string(),
            };
            {
                let g = state.db.lock().unwrap();
                db::upsert_file_record(&g, &rec)?;
            }
            if p.status == "source_deleted" {
                emit_log(
                    app,
                    state,
                    task_id,
                    "info",
                    &format!("文件恢复且内容未变，重新关联旧文档：{rel}"),
                    "",
                );
            }
            return Ok((FileAction::Skipped, Some(p.doc_key.clone())));
        }
    }

    // 创建新文档（带重试）
    let doc = with_retry(3, 500, || async { client.sync_file(abs, folder_id).await }).await?;

    // 覆盖：best-effort 删旧 doc_key（失败只 warn，新文档已建成功）
    if let Some(p) = prior {
        let old_key = p.doc_key.clone();
        match with_retry(2, 500, || async { client.delete_kb_file(&old_key).await }).await {
            Ok(_) => emit_log(
                app,
                state,
                task_id,
                "info",
                &format!("覆盖：已删除旧文档 {old_key}"),
                "",
            ),
            Err(e) => emit_log(
                app,
                state,
                task_id,
                "warn",
                &format!("旧文档删除失败（新文档已建立，残留旧文档需手动清理）：{old_key}"),
                &e.to_string(),
            ),
        }
    }

    let now = now_secs();
    let rec = FileRecord {
        task_id,
        relative_path: rel.to_string(),
        doc_key: doc.doc_key.clone(),
        content_hash: hash,
        size,
        mtime,
        synced_at: now,
        status: "synced".to_string(),
    };
    {
        let g = state.db.lock().unwrap();
        db::upsert_file_record(&g, &rec)?;
    }

    let action = if prior.is_some() {
        FileAction::Updated
    } else {
        FileAction::Created
    };
    emit_log(
        app,
        state,
        task_id,
        "info",
        &format!(
            "{}：{rel}",
            match action {
                FileAction::Created => "已上传",
                FileAction::Updated => "已覆盖",
                _ => "已同步",
            }
        ),
        &format!("doc_key={}", doc.doc_key),
    );
    Ok((action, Some(doc.doc_key)))
}

/// 某目录下的顶层白名单文件名（忽略子目录、隐藏文件、非白名单扩展），按名排序。
/// 读不到目录返回 None。
pub fn list_syncable_files(root: &Path) -> Option<Vec<String>> {
    let rd = std::fs::read_dir(root).ok()?;
    let mut names: Vec<String> = rd
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if !p.is_file() {
                return None;
            }
            let name = p.file_name()?.to_str()?.to_string();
            if name.starts_with('.') {
                return None;
            }
            let (allowed, _, _) = crate::emoo::ext_info(&name);
            if !allowed {
                return None;
            }
            Some(name)
        })
        .collect();
    names.sort();
    Some(names)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

fn set_status(app: &AppHandle, state: &Arc<AppState>, task_id: i64, status: &str, msg: &str) {
    {
        let g = state.db.lock().unwrap();
        let _ = db::set_task_status(&g, task_id, status, if msg.is_empty() { None } else { Some(msg) });
    }
    let last = {
        let g = state.db.lock().unwrap();
        db::get_task(&g, task_id)
            .ok()
            .flatten()
            .and_then(|t| t.last_synced_at)
    };
    let _ = app.emit(
        "task://status",
        StatusPayload {
            task_id,
            status: status.to_string(),
            status_message: msg.to_string(),
            last_synced_at: last,
        },
    );
}

fn emit_progress(app: &AppHandle, task_id: i64, current: u32, total: u32, message: &str) {
    let _ = app.emit(
        "task://progress",
        ProgressPayload {
            task_id,
            phase: "sync".to_string(),
            current,
            total,
            message: message.to_string(),
        },
    );
}

fn emit_log(
    app: &AppHandle,
    state: &Arc<AppState>,
    task_id: i64,
    level: &str,
    msg: &str,
    detail: &str,
) {
    let ts = {
        let g = state.db.lock().unwrap();
        db::append_log(&g, task_id, level, msg, detail).unwrap_or_else(|_| now_secs())
    };
    let _ = app.emit(
        "task://log",
        serde_json::json!({
            "taskId": task_id,
            "ts": ts,
            "level": level,
            "message": msg,
            "detail": detail,
        }),
    );
}

/// 指数退避重试。4xx 类业务错误（如 401）快速失败。
async fn with_retry<T, F, Fut>(max: u32, base_ms: u64, mut f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut delay = base_ms;
    let mut last: Option<anyhow::Error> = None;
    for attempt in 1..=max {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                let s = e.to_string();
                // 鉴权/业务类错误不重试
                if s.contains("HTTP 4") {
                    return Err(e);
                }
                last = Some(e);
                if attempt < max {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    delay = (delay * 2).min(8_000);
                }
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("with_retry: 无可重试结果")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vector() {
        // sha256("abc")
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
