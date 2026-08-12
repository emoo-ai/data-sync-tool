//! SQLite：建表/迁移 + 全部 CRUD。
//! 连接由上层 `Mutex<Connection>` 保护，所有调用都是短临界区，绝不跨 `.await` 持锁。

use anyhow::{Context, Result};
use rusqlite::{params, Connection, Row};
use std::path::Path;

use crate::task::*;

pub fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

pub fn open(dir: &Path) -> Result<Connection> {
    std::fs::create_dir_all(dir).context("创建 app_data_dir 失败")?;
    let path = dir.join("sync.db");
    let conn = Connection::open(&path).with_context(|| format!("打开 sync.db 失败 {path:?}"))?;
    migrate(&conn)?;
    Ok(conn)
}

pub fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA foreign_keys = ON;
         PRAGMA synchronous = NORMAL;
         CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS tasks (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             name TEXT NOT NULL,
             local_path TEXT NOT NULL,
             is_dir INTEGER NOT NULL DEFAULT 0,
             target_folder_id INTEGER,
             schedule_enabled INTEGER NOT NULL DEFAULT 0,
             schedule_interval_secs INTEGER NOT NULL DEFAULT 3600,
             status TEXT NOT NULL DEFAULT 'idle',
             status_message TEXT NOT NULL DEFAULT '',
             last_synced_at INTEGER,
             created_at INTEGER NOT NULL,
             updated_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS file_records (
             task_id INTEGER NOT NULL,
             relative_path TEXT NOT NULL,
             doc_key TEXT NOT NULL,
             content_hash TEXT NOT NULL,
             size INTEGER NOT NULL,
             mtime INTEGER NOT NULL,
             synced_at INTEGER NOT NULL,
             status TEXT NOT NULL DEFAULT 'synced',
             PRIMARY KEY (task_id, relative_path)
         );
         CREATE INDEX IF NOT EXISTS idx_file_records_task ON file_records(task_id);
         CREATE TABLE IF NOT EXISTS sync_log (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             task_id INTEGER NOT NULL,
             ts INTEGER NOT NULL,
             level TEXT NOT NULL,
             message TEXT NOT NULL,
             detail TEXT NOT NULL DEFAULT ''
         );
         CREATE INDEX IF NOT EXISTS idx_sync_log_task_ts ON sync_log(task_id, ts DESC);
        ",
    )
    .context("迁移 sync.db 失败")?;

    let cur: Option<i64> = conn
        .query_row("SELECT version FROM schema_version LIMIT 1", [], |r| r.get(0))
        .ok();
    if cur.is_none() {
        conn.execute("INSERT INTO schema_version(version) VALUES (1)", [])?;
    }

    // 增量列：tasks.permission（JSON，可空）。CREATE TABLE IF NOT EXISTS 不会给已存在的表加列，
    // 老库需要单独 ALTER；用 PRAGMA table_info 判断是否已有该列，保证幂等。
    let has_perm = {
        let mut stmt = conn.prepare("PRAGMA table_info(tasks)")?;
        let mut rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
        let mut found = false;
        while let Some(Ok(col)) = rows.next() {
            if col == "permission" {
                found = true;
                break;
            }
        }
        found
    };
    if !has_perm {
        conn.execute("ALTER TABLE tasks ADD COLUMN permission TEXT", [])?;
    }

    Ok(())
}

const TASK_COLS: &str = "id, name, local_path, is_dir, target_folder_id, schedule_enabled, \
     schedule_interval_secs, status, status_message, last_synced_at, created_at, updated_at, \
     permission";

fn row_to_task(r: &Row) -> rusqlite::Result<Task> {
    let perm_json: Option<String> = r.get(12).unwrap_or(None);
    let permission = perm_json.and_then(|s| serde_json::from_str::<PermissionSetting>(&s).ok());
    Ok(Task {
        id: r.get(0)?,
        name: r.get(1)?,
        local_path: r.get(2)?,
        is_dir: r.get::<_, i64>(3)? != 0,
        target_folder_id: r.get(4)?,
        schedule_enabled: r.get::<_, i64>(5)? != 0,
        schedule_interval_secs: r.get(6)?,
        status: r.get(7)?,
        status_message: r.get(8)?,
        last_synced_at: r.get(9)?,
        created_at: r.get(10)?,
        updated_at: r.get(11)?,
        permission,
    })
}

fn file_row(r: &Row) -> rusqlite::Result<FileRecord> {
    Ok(FileRecord {
        task_id: r.get(0)?,
        relative_path: r.get(1)?,
        doc_key: r.get(2)?,
        content_hash: r.get(3)?,
        size: r.get(4)?,
        mtime: r.get(5)?,
        synced_at: r.get(6)?,
        status: r.get(7)?,
    })
}

const FILE_COLS: &str = "task_id, relative_path, doc_key, content_hash, size, mtime, synced_at, status";

const LOG_COLS: &str = "id, task_id, ts, level, message, detail";

fn log_row(r: &Row) -> rusqlite::Result<LogEntry> {
    Ok(LogEntry {
        id: r.get(0)?,
        task_id: r.get(1)?,
        ts: r.get(2)?,
        level: r.get(3)?,
        message: r.get(4)?,
        detail: r.get(5)?,
    })
}

// ---- tasks CRUD ----

pub fn get_task(conn: &Connection, id: i64) -> Result<Option<Task>> {
    let sql = format!("SELECT {TASK_COLS} FROM tasks WHERE id=?1");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map(params![id], row_to_task)?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

pub fn list_tasks(conn: &Connection) -> Result<Vec<Task>> {
    let sql = format!("SELECT {TASK_COLS} FROM tasks ORDER BY id");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], row_to_task)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

pub fn insert_task(conn: &Connection, n: &NewTask) -> Result<Task> {
    let now = now_secs();
    let interval = n.interval();
    conn.execute(
        "INSERT INTO tasks \
         (name, local_path, is_dir, target_folder_id, schedule_enabled, \
          schedule_interval_secs, status, status_message, created_at, updated_at) \
         VALUES (?1,?2,?3,?4,?5,?6,'idle','',?7,?7)",
        params![
            n.name,
            n.local_path,
            n.is_dir as i64,
            n.target_folder_id,
            n.schedule() as i64,
            interval,
            now
        ],
    )?;
    let id = conn.last_insert_rowid();
    get_task(conn, id)?.context("刚插入的任务查不到")
}

pub fn update_task(conn: &Connection, id: i64, p: &TaskPatch) -> Result<Task> {
    let now = now_secs();
    if let Some(name) = &p.name {
        conn.execute(
            "UPDATE tasks SET name=?1, updated_at=?2 WHERE id=?3",
            params![name, now, id],
        )?;
    }
    if let Some(s) = p.schedule_enabled {
        conn.execute(
            "UPDATE tasks SET schedule_enabled=?1, updated_at=?2 WHERE id=?3",
            params![s as i64, now, id],
        )?;
    }
    if let Some(iv) = p.interval() {
        conn.execute(
            "UPDATE tasks SET schedule_interval_secs=?1, updated_at=?2 WHERE id=?3",
            params![iv, now, id],
        )?;
    }
    get_task(conn, id)?.context("update_task: 任务不存在")
}

/// 写每任务权限设置；None 清空（关闭权限控制）。
pub fn set_task_permission(
    conn: &Connection,
    id: i64,
    perm: &Option<PermissionSetting>,
) -> Result<Task> {
    let now = now_secs();
    let json = match perm {
        Some(p) => Some(serde_json::to_string(p)?),
        None => None,
    };
    conn.execute(
        "UPDATE tasks SET permission=?1, updated_at=?2 WHERE id=?3",
        params![json, now, id],
    )?;
    get_task(conn, id)?.context("set_task_permission: 任务不存在")
}

pub fn delete_task(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM file_records WHERE task_id=?1", params![id])?;
    conn.execute("DELETE FROM sync_log WHERE task_id=?1", params![id])?;
    conn.execute("DELETE FROM tasks WHERE id=?1", params![id])?;
    Ok(())
}

pub fn set_task_status(
    conn: &Connection,
    id: i64,
    status: &str,
    msg: Option<&str>,
) -> Result<()> {
    let now = now_secs();
    conn.execute(
        "UPDATE tasks SET status=?1, status_message=?2, updated_at=?3 WHERE id=?4",
        params![status, msg.unwrap_or(""), now, id],
    )?;
    Ok(())
}

pub fn touch_task_synced(conn: &Connection, id: i64, ts: i64) -> Result<()> {
    conn.execute(
        "UPDATE tasks SET last_synced_at=?1, updated_at=?1 WHERE id=?2",
        params![ts, id],
    )?;
    Ok(())
}

/// 启用定时且到期（status∈{idle,error} 且 last_synced_at + interval ≤ now）的任务。
pub fn list_schedule_due(conn: &Connection, now: i64) -> Result<Vec<Task>> {
    let sql = format!(
        "SELECT {TASK_COLS} FROM tasks \
         WHERE schedule_enabled=1 AND status IN ('idle','error') \
           AND (last_synced_at IS NULL OR last_synced_at + schedule_interval_secs <= ?1)"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![now], row_to_task)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

// ---- file_records CRUD ----

pub fn get_file_record(conn: &Connection, task_id: i64, rel: &str) -> Result<Option<FileRecord>> {
    let sql = format!(
        "SELECT {FILE_COLS} FROM file_records WHERE task_id=?1 AND relative_path=?2"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map(params![task_id, rel], file_row)?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

pub fn upsert_file_record(conn: &Connection, r: &FileRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO file_records \
         (task_id, relative_path, doc_key, content_hash, size, mtime, synced_at, status) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8) \
         ON CONFLICT(task_id, relative_path) DO UPDATE SET \
            doc_key=excluded.doc_key, content_hash=excluded.content_hash, size=excluded.size, \
            mtime=excluded.mtime, synced_at=excluded.synced_at, status=excluded.status",
        params![
            r.task_id,
            r.relative_path,
            r.doc_key,
            r.content_hash,
            r.size,
            r.mtime,
            r.synced_at,
            r.status
        ],
    )?;
    Ok(())
}

pub fn list_file_records(conn: &Connection, task_id: i64) -> Result<Vec<FileRecord>> {
    let sql = format!("SELECT {FILE_COLS} FROM file_records WHERE task_id=?1");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![task_id], file_row)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

pub fn mark_file_source_deleted(conn: &Connection, task_id: i64, rel: &str) -> Result<()> {
    conn.execute(
        "UPDATE file_records SET status='source_deleted' WHERE task_id=?1 AND relative_path=?2",
        params![task_id, rel],
    )?;
    Ok(())
}

// ---- sync_log ----

pub fn append_log(
    conn: &Connection,
    task_id: i64,
    level: &str,
    msg: &str,
    detail: &str,
) -> Result<i64> {
    let ts = now_secs();
    conn.execute(
        "INSERT INTO sync_log (task_id, ts, level, message, detail) VALUES (?1,?2,?3,?4,?5)",
        params![task_id, ts, level, msg, detail],
    )?;
    Ok(ts)
}

pub fn recent_log(conn: &Connection, task_id: Option<i64>, limit: i64) -> Result<Vec<LogEntry>> {
    match task_id {
        Some(tid) => {
            let sql = format!(
                "SELECT {LOG_COLS} FROM sync_log WHERE task_id=?1 ORDER BY ts DESC, id DESC LIMIT ?2"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params![tid, limit], log_row)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        }
        None => {
            let sql = format!("SELECT {LOG_COLS} FROM sync_log ORDER BY ts DESC, id DESC LIMIT ?1");
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params![limit], log_row)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        }
    }
}

/// 删除 ts 早于 now - retain_secs 的日志，并收缩 WAL，避免长期运行累积膨胀。返回删除行数。
pub fn cleanup_old_logs(conn: &Connection, now: i64, retain_secs: i64) -> Result<usize> {
    let cutoff = now - retain_secs;
    let n = conn.execute("DELETE FROM sync_log WHERE ts < ?1", params![cutoff])?;
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c
    }

    fn sample_new(name: &str, path: &str, is_dir: bool) -> NewTask {
        NewTask {
            name: name.to_string(),
            local_path: path.to_string(),
            is_dir,
            target_folder_id: Some(5),
            schedule_enabled: Some(false),
            schedule_interval_secs: None,
        }
    }

    #[test]
    fn insert_get_list_delete() {
        let c = mem();
        let t = insert_task(&c, &sample_new("a", "/x", false)).unwrap();
        assert_eq!(t.name, "a");
        assert!(!t.is_dir);
        assert_eq!(t.schedule_interval_secs, 3600);
        let got = get_task(&c, t.id).unwrap().unwrap();
        assert_eq!(got.id, t.id);
        assert_eq!(list_tasks(&c).unwrap().len(), 1);
        delete_task(&c, t.id).unwrap();
        assert!(list_tasks(&c).unwrap().is_empty());
    }

    #[test]
    fn upsert_file_record_updates_on_conflict() {
        let c = mem();
        let t = insert_task(&c, &sample_new("a", "/x", false)).unwrap();
        let r = FileRecord {
            task_id: t.id,
            relative_path: "f.txt".into(),
            doc_key: "k1".into(),
            content_hash: "h1".into(),
            size: 1,
            mtime: 1,
            synced_at: 1,
            status: "synced".into(),
        };
        upsert_file_record(&c, &r).unwrap();
        let mut r2 = r.clone();
        r2.doc_key = "k2".into();
        r2.content_hash = "h2".into();
        upsert_file_record(&c, &r2).unwrap();
        let recs = list_file_records(&c, t.id).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].doc_key, "k2");
        assert_eq!(recs[0].content_hash, "h2");
    }

    #[test]
    fn schedule_due_picks_unsynced_and_expired() {
        let c = mem();
        let mut n = sample_new("a", "/x", false);
        n.schedule_enabled = Some(true);
        let t = insert_task(&c, &n).unwrap();
        let due = list_schedule_due(&c, now_secs()).unwrap();
        assert!(due.iter().any(|x| x.id == t.id)); // 从未同步 → 到期
        touch_task_synced(&c, t.id, now_secs()).unwrap();
        let due2 = list_schedule_due(&c, now_secs()).unwrap();
        assert!(!due2.iter().any(|x| x.id == t.id)); // 刚同步未到期
    }

    #[test]
    fn interval_floors_to_1_hour() {
        let mut n = sample_new("a", "/x", false);
        n.schedule_interval_secs = Some(10);
        assert_eq!(n.interval(), 3600);
    }

    #[test]
    fn cleanup_old_logs_deletes_by_ts() {
        let c = mem();
        let t = insert_task(&c, &sample_new("a", "/x", false)).unwrap();
        // append_log 用当前时间，无法直接造老数据；手动插一条 ts=1000 的老日志。
        c.execute(
            "INSERT INTO sync_log (task_id, ts, level, message, detail) \
             VALUES (?1, 1000, 'info', 'old', '')",
            params![t.id],
        )
        .unwrap();
        let now = 1_000_000_000_i64;
        let n = cleanup_old_logs(&c, now, 100).unwrap(); // cutoff = now-100 → 删 ts<now-100
        assert_eq!(n, 1);
        assert_eq!(recent_log(&c, Some(t.id), 10).unwrap().len(), 0);
    }
}
