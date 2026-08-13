use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tauri::Manager;

pub const DEFAULT_BASE_URL: &str = "https://app.emooai.com/open-api/v1";

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    /// 调用者 open_id，仅在「设置文档权限」时作为 Emoo-User-Id 请求头（需工作区超管）。
    #[serde(default)]
    pub emoo_user_id: Option<String>,
    /// 关闭主窗口(×)时是否隐藏到托盘而非退出；默认 true。老 config 无此字段回落 true。
    #[serde(default = "default_close_to_tray")]
    pub close_to_tray: bool,
}

/// close_to_tray 的 serde / Default 默认值。
fn default_close_to_tray() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            api_key: String::new(),
            emoo_user_id: None,
            close_to_tray: default_close_to_tray(),
        }
    }
}

fn config_path(app: &tauri::AppHandle) -> Result<std::path::PathBuf> {
    let dir = app.path().app_data_dir().context("解析 app_data_dir 失败")?;
    std::fs::create_dir_all(&dir).context("创建 app_data_dir 失败")?;
    Ok(dir.join("config.json"))
}

/// 序列化写入 config.json（所有写入路径经此，保证字段不丢）。
fn write(app: &tauri::AppHandle, c: &Config) -> Result<()> {
    let p = config_path(app)?;
    let raw = serde_json::to_string_pretty(c).context("序列化 config 失败")?;
    std::fs::write(&p, raw).context("写入 config.json 失败")?;
    Ok(())
}

pub fn load(app: &tauri::AppHandle) -> Result<Config> {
    let p = config_path(app)?;
    if !p.exists() {
        return Ok(Config::default());
    }
    let raw = std::fs::read_to_string(&p).context("读取 config.json 失败")?;
    let mut c: Config = serde_json::from_str(&raw).unwrap_or_default();
    if c.base_url.trim().is_empty() {
        c.base_url = DEFAULT_BASE_URL.to_string();
    }
    Ok(c)
}

/// 保存鉴权配置；先载入现有以保留 close_to_tray 等其他字段，避免互相覆盖。
pub fn save(
    app: &tauri::AppHandle,
    base_url: String,
    api_key: String,
    emoo_user_id: Option<String>,
) -> Result<()> {
    let mut c = load(app).unwrap_or_default();
    let uid = emoo_user_id.filter(|s| !s.trim().is_empty());
    c.base_url = base_url;
    c.api_key = api_key;
    c.emoo_user_id = uid;
    write(app, &c)
}

/// 单独设置「关闭到托盘」偏好，保留其余字段。
pub fn set_close_behavior(app: &tauri::AppHandle, close_to_tray: bool) -> Result<()> {
    let mut c = load(app).unwrap_or_default();
    c.close_to_tray = close_to_tray;
    write(app, &c)
}

/// 单独设置「工作区超管 open_id」，保留其余字段。返回写回后的完整 Config，
/// 供调用方据此 reconfigure 已建好的 EmooClient（复用 reqwest 连接池）。
pub fn set_emoo_user_id(app: &tauri::AppHandle, emoo_user_id: Option<String>) -> Result<Config> {
    let mut c = load(app).unwrap_or_default();
    c.emoo_user_id = emoo_user_id.filter(|s| !s.trim().is_empty());
    write(app, &c)?;
    Ok(c)
}
