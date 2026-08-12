use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tauri::Manager;

pub const DEFAULT_BASE_URL: &str = "https://app.emooai.com/open-api/v1";

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Config {
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    /// 调用者 open_id，仅在「设置文档权限」时作为 Emoo-User-Id 请求头（需工作区超管）。
    #[serde(default)]
    pub emoo_user_id: Option<String>,
}

fn config_path(app: &tauri::AppHandle) -> Result<std::path::PathBuf> {
    let dir = app.path().app_data_dir().context("解析 app_data_dir 失败")?;
    std::fs::create_dir_all(&dir).context("创建 app_data_dir 失败")?;
    Ok(dir.join("config.json"))
}

pub fn load(app: &tauri::AppHandle) -> Result<Config> {
    let p = config_path(app)?;
    if !p.exists() {
        return Ok(Config {
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key: String::new(),
            emoo_user_id: None,
        });
    }
    let raw = std::fs::read_to_string(&p).context("读取 config.json 失败")?;
    let mut c: Config = serde_json::from_str(&raw).unwrap_or_default();
    if c.base_url.trim().is_empty() {
        c.base_url = DEFAULT_BASE_URL.to_string();
    }
    Ok(c)
}

pub fn save(
    app: &tauri::AppHandle,
    base_url: String,
    api_key: String,
    emoo_user_id: Option<String>,
) -> Result<()> {
    let p = config_path(app)?;
    let uid = emoo_user_id.filter(|s| !s.trim().is_empty());
    let c = Config {
        base_url,
        api_key,
        emoo_user_id: uid,
    };
    let raw = serde_json::to_string_pretty(&c).context("序列化 config 失败")?;
    std::fs::write(&p, raw).context("写入 config.json 失败")?;
    Ok(())
}
