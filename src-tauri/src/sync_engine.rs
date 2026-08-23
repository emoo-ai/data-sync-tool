//! 同步引擎：单文件 / 文件夹递归同步（远端按子目录结构镜像，find-or-create）、
//! 覆盖（新 doc + 删旧 doc）、本地删除策略（远端保留为备份）、≤500 硬上限、
//! 单飞、重试、sha256 变更检测、内置临时文件排除。

use crate::db::{self, now_secs};
use crate::emoo::{EmooClient, FolderItem};
use crate::state::AppState;
use crate::task::*;
use anyhow::{anyhow, bail, Result};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
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
        let g = state.db.lock();
        db::get_task(&g, task_id)?
    }
    .ok_or_else(|| anyhow!("任务不存在 #{task_id}"))?;

    // 起手 clone 客户端（廉价：reqwest::Client 是 Arc），锁只在此瞬间持有
    let client = state.emoo.read().clone();

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
        // 目录分支（递归收集 + 远端子目录镜像）
        let files = match list_syncable_files(&root) {
            Some(f) => f,
            None => {
                let msg = "读取目录失败";
                set_status(app, state, task_id, STATUS_ERROR, msg);
                emit_log(app, state, task_id, "error", msg, "");
                bail!(msg);
            }
        };

        // 硬上限（含子目录）
        if files.len() > FOLDER_FILE_CAP {
            let msg = format!(
                "文件夹内（含子目录）可同步文件 {} 个，超过 {} 上限，已中止（不部分同步）",
                files.len(),
                FOLDER_FILE_CAP
            );
            set_status(app, state, task_id, STATUS_ERROR, &msg);
            emit_log(app, state, task_id, "error", &msg, "");
            bail!(msg);
        }

        // 先解析全部唯一子目录（find-or-create 镜像）。放在上传前：
        // 建不了文件夹（如非超管 key）时快速失败，不产生「半镜像」。
        let mut resolver = FolderResolver::new(&client, task.target_folder_id);

        // 包含文件夹本身：在目标下 find-or-create 与本地根同名的文件夹，整体镜像进它。
        // （路径无文件名的极端情况跳过，退回直接镜像内容）
        if task.include_root {
            if let Some(root_name) = root.file_name().and_then(|n| n.to_str()) {
                if let Err(e) = resolver.embed_root(app, state, task_id, root_name).await {
                    let msg = e.to_string();
                    set_status(app, state, task_id, STATUS_ERROR, &msg);
                    emit_log(
                        app,
                        state,
                        task_id,
                        "error",
                        "解析/创建远端文件夹失败，同步中止",
                        &msg,
                    );
                    bail!(msg);
                }
            }
        }
        {
            let mut dirs: Vec<&str> = files
                .iter()
                .map(|f| parent_rel(f))
                .filter(|d| !d.is_empty())
                .collect();
            dirs.sort_unstable();
            dirs.dedup();
            for d in dirs {
                if let Err(e) = resolver.resolve(app, state, task_id, d).await {
                    let msg = e.to_string();
                    set_status(app, state, task_id, STATUS_ERROR, &msg);
                    emit_log(
                        app,
                        state,
                        task_id,
                        "error",
                        "解析/创建远端文件夹失败，同步中止",
                        &msg,
                    );
                    bail!(msg);
                }
            }
        }

        let total = files.len() as u32;
        let on_disk: HashSet<String> = files.iter().cloned().collect();

        for (i, rel) in files.iter().enumerate() {
            let abs = root.join(rel);
            emit_progress(app, task_id, (i + 1) as u32, total, rel);
            // 该文件应落入的远端文件夹：根文件 = 任务目标；子目录文件 = 镜像出的子文件夹。
            // 预解析后此处为缓存命中（零网络）；防御性处理 Err → 计入该文件失败。
            let file_folder = match resolver.resolve(app, state, task_id, parent_rel(rel)).await {
                Ok(id) => id,
                Err(e) => {
                    outcome.failed += 1;
                    emit_log(
                        app,
                        state,
                        task_id,
                        "warn",
                        &format!("解析远端文件夹失败：{rel}"),
                        &e.to_string(),
                    );
                    continue;
                }
            };
            match sync_one_file(app, state, &client, task_id, file_folder, &abs, rel, task.permission.as_ref())
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
                        &format!("同步失败：{rel}"),
                        &e.to_string(),
                    );
                }
            }
        }

        // 本地删除扫描：DB 里 synced 但磁盘已无 → 标 source_deleted，远端文档保留
        let recs = {
            let g = state.db.lock();
            db::list_file_records(&g, task_id)?
        };
        for r in recs {
            if r.status == "synced" && !on_disk.contains(&r.relative_path) {
                {
                    let g = state.db.lock();
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
        let g = state.db.lock();
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

/// 在远端直接 children 里按名找 folder：先精确匹配，再大小写不敏感兜底
/// （远端若用 ci 排序规则，避免建出仅大小写不同的重复文件夹）。
fn find_folder_by_name<'a>(items: &'a [FolderItem], name: &str) -> Option<&'a FolderItem> {
    let mut folders = items.iter().filter(|it| it.item_type == "folder");
    folders
        .clone()
        .find(|it| it.name == name)
        .or_else(|| folders.find(|it| it.name.eq_ignore_ascii_case(name)))
}

/// 本次同步运行的远端文件夹解析器：本地相对目录 → 远端 folder_id（子目录镜像）。
/// find-or-create（无 DB 缓存，纯状态化 list-and-match，每轮同步重建）；
/// 删除策略与文档一致：本地删目录后远端文件夹保留，重建同名目录会重新接上。
struct FolderResolver<'a> {
    client: &'a EmooClient,
    /// 任务目标文件夹（None=远端根）。
    root: Option<i64>,
    /// 已解析子目录（相对目录路径 → folder id）。
    cache: HashMap<String, i64>,
}

impl<'a> FolderResolver<'a> {
    fn new(client: &'a EmooClient, root: Option<i64>) -> Self {
        Self {
            client,
            root,
            cache: HashMap::new(),
        }
    }

    /// include_root 模式：在任务目标下 find-or-create 与本地根同名的文件夹，
    /// 并把解析根切到它 —— 之后所有子目录/根文件都镜像进这个文件夹。
    async fn embed_root(
        &mut self,
        app: &AppHandle,
        state: &Arc<AppState>,
        task_id: i64,
        name: &str,
    ) -> Result<()> {
        let parent = self.root; // 先拷值，避免接收者 &mut self 与参数读取的借用冲突
        let id = self
            .ensure_folder(app, state, task_id, name, name, parent)
            .await?;
        self.root = Some(id);
        Ok(())
    }

    /// 解析 rel_dir（""=任务目标根，直接返回 root）。逐段解析祖先链；
    /// 全部命中缓存则零网络请求。
    async fn resolve(
        &mut self,
        app: &AppHandle,
        state: &Arc<AppState>,
        task_id: i64,
        rel_dir: &str,
    ) -> Result<Option<i64>> {
        if rel_dir.is_empty() {
            return Ok(self.root);
        }
        if let Some(id) = self.cache.get(rel_dir) {
            return Ok(Some(*id));
        }
        let mut parent = self.root;
        let mut prefix = String::new();
        for seg in rel_dir.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(seg);
            if let Some(id) = self.cache.get(&prefix) {
                parent = Some(*id);
                continue;
            }
            let id = self
                .ensure_folder(app, state, task_id, &prefix, seg, parent)
                .await?;
            parent = Some(id);
        }
        Ok(parent)
    }

    /// find-or-create 单段。列 children（GET 幂等，带重试）→ 命中即复用；
    /// 未命中 POST 新建（带重试）；创建失败再列一次兜底并发/远端已建（如 4206）。
    async fn ensure_folder(
        &mut self,
        app: &AppHandle,
        state: &Arc<AppState>,
        task_id: i64,
        prefix: &str,
        name: &str,
        parent: Option<i64>,
    ) -> Result<i64> {
        let client = self.client; // 先拷引用，避免闭包与 &mut self 的借用冲突
        let items = with_retry(3, 500, || async { client.list_folder_items(parent).await })
            .await
            .map_err(|e| anyhow!("列出远端文件夹失败：{prefix}（父 folder_id={parent:?}）：{e}"))?;
        if let Some(found) = find_folder_by_name(&items, name) {
            self.cache.insert(prefix.to_string(), found.id);
            return Ok(found.id);
        }
        match with_retry(3, 500, || async { client.create_folder(name, parent).await }).await {
            Ok(info) => {
                emit_log(
                    app,
                    state,
                    task_id,
                    "info",
                    &format!("已创建远端文件夹：{prefix}"),
                    &format!("folder_id={}", info.id),
                );
                self.cache.insert(prefix.to_string(), info.id);
                Ok(info.id)
            }
            Err(e) => {
                // 兜底：并发的另一任务/他人刚建了同名文件夹（业务错误 4206 等）→ 再列一次
                let relist =
                    with_retry(2, 500, || async { client.list_folder_items(parent).await }).await;
                if let Ok(items2) = relist {
                    if let Some(found) = find_folder_by_name(&items2, name) {
                        self.cache.insert(prefix.to_string(), found.id);
                        return Ok(found.id);
                    }
                }
                Err(anyhow!(
                    "创建远端文件夹失败：{prefix}（创建文件夹需工作区超管权限；远端文件夹树最深 10 层）：{e}"
                ))
            }
        }
    }
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
        let g = state.db.lock();
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
                let g = state.db.lock();
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
        let g = state.db.lock();
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

/// 内置临时/系统文件排除规则（仅内置，不开放用户配置）。扩展名白名单本身已排除
/// 绝大多数临时文件，这里主要兜底「白名单扩展名的锁文件」（如 Office 的 ~$xx.docx）。
const BUILTIN_TEMP_PREFIXES: &[&str] = &["~$"];
const BUILTIN_TEMP_NAMES: &[&str] = &["thumbs.db", "desktop.ini"];
const BUILTIN_TEMP_EXTS: &[&str] = &["tmp", "temp", "part", "crdownload", "download"];

/// 是否命中内置排除规则（大小写不敏感：Thumbs.db 来自 Windows，macOS 上大小写不定）。
fn is_builtin_temp_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    BUILTIN_TEMP_NAMES.contains(&lower.as_str())
        || BUILTIN_TEMP_PREFIXES.iter().any(|p| lower.starts_with(p))
        || BUILTIN_TEMP_EXTS.contains(&lower.rsplit('.').next().unwrap_or(""))
}

/// 递归收集 root 下全部白名单文件（剪隐藏目录、跳隐藏/内置临时文件/非白名单扩展）。
/// 返回以 '/' 连接的相对路径（根级文件为裸文件名，兼容历史记录），按路径排序。
/// 根目录读不到返回 None；子目录读不动（权限等）仅跳过该子树。
pub fn list_syncable_files(root: &Path) -> Option<Vec<String>> {
    let mut out = Vec::new();
    walk_dir(root, "", &mut out)?;
    out.sort();
    Some(out)
}

/// 单层收集。dir_rel 为当前目录相对路径（""=根）。
fn walk_dir(dir: &Path, dir_rel: &str, out: &mut Vec<String>) -> Option<()> {
    let rd = std::fs::read_dir(dir).ok()?;
    for entry in rd.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // 目录判定用 entry.file_type()（不跟随符号链接 → 不会绕回成环）
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            if name.starts_with('.') {
                continue; // 隐藏目录整棵剪掉
            }
            let child_rel = join_rel(dir_rel, name);
            let _ = walk_dir(&path, &child_rel, out); // 子目录失败仅跳过该子树
        } else if path.is_file() {
            // 文件判定跟随符号链接，与旧实现一致（链接到普通文件可同步）
            if name.starts_with('.') || is_builtin_temp_file(name) {
                continue;
            }
            let (allowed, _, _) = crate::emoo::ext_info(name);
            if !allowed {
                continue;
            }
            out.push(join_rel(dir_rel, name));
        }
    }
    Some(())
}

/// 相对路径拼接（恒用 '/'，不依赖平台分隔符；root.join(rel) 在 Windows 也接受 '/'）。
fn join_rel(dir_rel: &str, name: &str) -> String {
    if dir_rel.is_empty() {
        name.to_string()
    } else {
        format!("{dir_rel}/{name}")
    }
}

/// 相对路径的目录部分："子目录/嵌套/f.pdf" → "子目录/嵌套"；根级文件 → ""。
fn parent_rel(rel: &str) -> &str {
    match rel.rfind('/') {
        Some(i) => &rel[..i],
        None => "",
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

fn set_status(app: &AppHandle, state: &Arc<AppState>, task_id: i64, status: &str, msg: &str) {
    {
        let g = state.db.lock();
        let _ = db::set_task_status(&g, task_id, status, if msg.is_empty() { None } else { Some(msg) });
    }
    let last = {
        let g = state.db.lock();
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
        let g = state.db.lock();
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

    #[test]
    fn builtin_temp_rules() {
        // 命中：Office 锁文件（白名单扩展也拦）、系统文件、临时扩展（大小写不敏感）
        for n in [
            "~$report.docx",
            "~$",
            "Thumbs.db",
            "thumbs.db",
            "desktop.ini",
            "a.tmp",
            "b.TEMP",
            "c.part",
            "d.crdownload",
        ] {
            assert!(is_builtin_temp_file(n), "应命中排除规则：{n}");
        }
        // 不命中：正常白名单文件
        for n in ["report.pdf", "data.csv", "笔记.md", "Thumbs.dbx", "照片.JPG"] {
            assert!(!is_builtin_temp_file(n), "不应命中排除规则：{n}");
        }
    }

    #[test]
    fn parent_rel_cases() {
        assert_eq!(parent_rel("a/b/f.pdf"), "a/b");
        assert_eq!(parent_rel("f.pdf"), "");
        assert_eq!(parent_rel("a/b/c"), "a/b");
    }

    fn folder(id: i64, name: &str) -> FolderItem {
        FolderItem {
            item_type: "folder".to_string(),
            id,
            name: name.to_string(),
            parent_id: None,
            folder_id: None,
            has_children: None,
        }
    }

    #[test]
    fn find_folder_by_name_cases() {
        let items = vec![
            folder(1, "报告"),
            FolderItem {
                item_type: "document".to_string(),
                id: 2,
                name: "报告".to_string(), // 非文件夹项，应被忽略
                parent_id: None,
                folder_id: Some(9),
                has_children: None,
            },
            folder(3, "Photos"),
        ];
        // 精确命中
        assert_eq!(find_folder_by_name(&items, "报告").map(|f| f.id), Some(1));
        // 大小写不敏感兜底
        assert_eq!(find_folder_by_name(&items, "photos").map(|f| f.id), Some(3));
        // 未命中
        assert!(find_folder_by_name(&items, "不存在").is_none());
    }

    #[test]
    fn list_syncable_files_recursive() {
        let root = tempfile::tempdir().unwrap();
        let p = root.path();
        let sub = p.join("子目录");
        let nested = sub.join("嵌套");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(p.join(".git")).unwrap();
        for (dir, name) in [
            (p.to_path_buf(), "a.md"),
            (p.to_path_buf(), "b.jpg"),
            (p.to_path_buf(), "c.exe"),        // 非白名单扩展
            (p.to_path_buf(), "~$x.docx"),     // Office 锁文件
            (p.to_path_buf(), "Thumbs.db"),    // 系统文件
            (p.to_path_buf(), ".hidden.txt"),  // 隐藏文件
            (p.to_path_buf(), "x.tmp"),        // 临时扩展
            (sub.clone(), "n.pdf"),
            (sub.clone(), ".DS_Store"),
            (nested.clone(), "d.docx"),
            (nested.clone(), "~$lock.docx"),
            (p.join(".git"), "x.pdf"), // 隐藏目录整棵剪掉
        ] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }

        let got = list_syncable_files(p).unwrap();
        assert_eq!(
            got,
            vec![
                "a.md".to_string(),
                "b.jpg".to_string(),
                "子目录/n.pdf".to_string(),
                "子目录/嵌套/d.docx".to_string(),
            ]
        );

        // 根不存在 → None
        assert!(list_syncable_files(Path::new("/nonexistent/zzz")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn list_syncable_files_skips_dir_symlink() {
        // 目录符号链接不跟随（防成环）：root/loop -> root，walk 必须终止且不含 loop 下内容
        let root = tempfile::tempdir().unwrap();
        let p = root.path();
        std::fs::create_dir_all(p.join("real")).unwrap();
        std::fs::write(p.join("real/a.md"), b"x").unwrap();
        std::os::unix::fs::symlink(p, p.join("loop")).unwrap();

        let got = list_syncable_files(p).unwrap();
        assert_eq!(got, vec!["real/a.md".to_string()]);
    }
}
