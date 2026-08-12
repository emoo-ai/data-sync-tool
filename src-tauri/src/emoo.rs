use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

const IMG_CAP: u64 = 20 * 1024 * 1024; // kb-file 对图片单文件 20MB
const FILE_CAP: u64 = 100 * 1024 * 1024; // 其它 100MB

/// Emoo Open API 客户端。绑用户的 API Key 模式：绝大部分接口仅需 Bearer；
/// 仅「设置文档权限」接口额外要求 Emoo-User-Id 头（调用者 open_id）。
#[derive(Clone)]
pub struct EmooClient {
    base: String,
    api_key: String,
    /// 调用者 open_id，仅设置文档权限时作为 Emoo-User-Id 头（None 时不带，可能被拒）。
    emoo_user_id: Option<String>,
    http: reqwest::Client,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct FolderItem {
    // API 里判别字段叫 `type`；仅反序列化时映射，对前端输出仍用 `item_type`（避开 JS 保留字）。
    #[serde(rename(deserialize = "type"))]
    pub item_type: String,
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub parent_id: Option<i64>,
    #[serde(default)]
    #[allow(dead_code)]
    pub folder_id: Option<i64>,
    /// folder 项才有：是否含子级（用于树形控件决定是否显示展开箭头）。
    #[serde(default)]
    pub has_children: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct KbFileDoc {
    pub id: i64,
    pub doc_key: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub folder_id: Option<i64>,
}

// ---- Emoo 统一响应壳 ----
#[derive(Deserialize)]
struct EmooResp<T> {
    code: i64,
    message: String,
    data: Option<T>,
}

// ---- upload-credentials ----
#[derive(Serialize)]
struct UcReq<'a> {
    files: Vec<UcItem<'a>>,
}
#[derive(Serialize)]
struct UcItem<'a> {
    file_name: &'a str,
    file_size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    mime_type: Option<&'a str>,
}
#[derive(Deserialize)]
struct UcResult {
    file_key: String,
    #[serde(default)]
    #[allow(dead_code)]
    mime_type: String,
    upload: UploadInstr,
}
#[derive(Deserialize)]
struct UploadInstr {
    method: String,
    url: String,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(default)]
    form_fields: HashMap<String, String>,
    #[serde(default)]
    file_field: Option<String>,
}

// ---- confirm ----
#[derive(Serialize)]
struct ConfirmReq<'a> {
    files: Vec<ConfirmItem<'a>>,
}
#[derive(Serialize)]
struct ConfirmItem<'a> {
    file_key: &'a str,
    file_name: &'a str,
    file_size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    mime_type: Option<&'a str>,
}
#[derive(Deserialize)]
struct ConfirmResult {
    #[allow(dead_code)]
    file_key: String,
    #[allow(dead_code)]
    url: String,
}

// ---- kb-file ----
#[derive(Serialize)]
struct KbCreateReq<'a> {
    file_key: &'a str,
    file_name: &'a str,
    file_size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    mime_type: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    folder_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'a str>,
}

// ---- folder items ----
#[derive(Deserialize)]
struct FolderItemsData {
    items: Vec<FolderItem>,
}

// ---- 文档权限 ----
/// 单个维度的权限受众。type: "all" | "none" | "specified"；
/// specified 时 user_open_ids / group_ids 至少一个非空。
/// 字段名与 Emoo API 一致（snake_case + 保留字 type），前后端共用此结构。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DocPermissionAudience {
    #[serde(rename = "type")]
    pub perm_type: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user_open_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_ids: Vec<i64>,
}

/// 任务级权限设置。仅暴露「可见 / 可删除」两个维度（Emoo 端不支持可编辑参数）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PermissionSetting {
    pub visible: DocPermissionAudience,
    pub deletable: DocPermissionAudience,
}

// ---- ws-user 通讯录（权限「指定」范围用） ----
#[derive(Clone, Serialize, Deserialize)]
pub struct WsGroup {
    pub id: i64,
    pub group_name: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct WsUser {
    pub open_id: String,
    pub user_id: i64,
    pub ws_username: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ws_group_list: Vec<WsGroup>,
}
#[derive(Deserialize)]
struct WsUserData {
    #[serde(default)]
    #[allow(dead_code)]
    total: i64,
    results: Vec<WsUser>,
}

impl EmooClient {
    pub fn new(base: String, api_key: String, emoo_user_id: Option<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(180))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            base: base.trim_end_matches('/').to_string(),
            api_key,
            emoo_user_id: emoo_user_id.filter(|s| !s.trim().is_empty()),
            http,
        }
    }

    /// 换 base/api_key/emoo_user_id，复用内部 reqwest::Client（连接池保留）。
    pub fn reconfigure(
        &mut self,
        base: String,
        api_key: String,
        emoo_user_id: Option<String>,
    ) {
        self.base = base.trim_end_matches('/').to_string();
        self.api_key = api_key;
        self.emoo_user_id = emoo_user_id.filter(|s| !s.trim().is_empty());
    }

    fn url(&self, p: &str) -> String {
        format!("{}{}", self.base, p)
    }

    /// 统一解析 Emoo 响应：检查 HTTP 状态 + body.code==200，取出 data。
    async fn parse<T: DeserializeOwned>(resp: reqwest::Response, ctx: &str) -> Result<T> {
        let status = resp.status();
        let body = resp.text().await.context("读取响应体失败")?;
        let parsed: EmooResp<T> = serde_json::from_str(&body)
            .with_context(|| format!("[{ctx}] 解析响应失败；body: {body}"))?;
        if !status.is_success() {
            bail!("[{ctx}] HTTP {status}: {body}");
        }
        if parsed.code != 200 {
            bail!("[{ctx}] emoo 业务错误 {}: {}", parsed.code, parsed.message);
        }
        parsed.data.ok_or_else(|| anyhow!("[{ctx}] emoo 返回空 data"))
    }

    /// 列出 folder 直接 children（folder/table/document 混合）。None=根。
    pub async fn list_folder_items(&self, folder_id: Option<i64>) -> Result<Vec<FolderItem>> {
        let mut req = self
            .http
            .get(self.url("/data/folder/items"))
            .bearer_auth(&self.api_key);
        if let Some(id) = folder_id {
            req = req.query(&[("folder_id", id)]);
        }
        let resp = req.send().await.context("GET /data/folder/items")?;
        let d: FolderItemsData = Self::parse(resp, "folder/items").await?;
        Ok(d.items)
    }

    /// 删除一个 kb-file 文档（按 doc_key）。覆盖用：上传新 doc 后删旧 doc。
    pub async fn delete_kb_file(&self, doc_key: &str) -> Result<bool> {
        #[derive(Serialize)]
        struct Req<'a> {
            doc_key: &'a str,
        }
        #[derive(Deserialize)]
        struct R {
            #[allow(dead_code)]
            deleted: bool,
        }
        let resp = self
            .http
            .delete(self.url("/data/kb-file"))
            .bearer_auth(&self.api_key)
            .json(&Req { doc_key })
            .send()
            .await
            .context("DELETE /data/kb-file")?;
        let r: R = Self::parse(resp, "kb-file delete").await?;
        Ok(r.deleted)
    }

    /// 仅校验 HTTP + emoo code，不取 data（用于 data 可空的接口，如设置权限）。
    async fn parse_unit(resp: reqwest::Response, ctx: &str) -> Result<()> {
        let status = resp.status();
        let body = resp.text().await.context("读取响应体失败")?;
        let parsed: EmooResp<serde_json::Value> = serde_json::from_str(&body)
            .with_context(|| format!("[{ctx}] 解析响应失败；body: {body}"))?;
        if !status.is_success() {
            bail!("[{ctx}] HTTP {status}: {body}");
        }
        if parsed.code != 200 {
            bail!("[{ctx}] emoo 业务错误 {}: {}", parsed.code, parsed.message);
        }
        Ok(())
    }

    /// 通讯录成员（用于权限「指定」范围选择）。page_size 默认/上限 100。
    pub async fn list_ws_users(
        &self,
        keyword: Option<&str>,
        page_size: Option<i64>,
        current_page: Option<i64>,
    ) -> Result<Vec<WsUser>> {
        let mut params: Vec<(&str, String)> = Vec::new();
        if let Some(k) = keyword {
            let k = k.trim();
            if !k.is_empty() {
                params.push(("keyword", k.to_string()));
            }
        }
        params.push(("page_size", page_size.unwrap_or(100).to_string()));
        params.push(("current_page", current_page.unwrap_or(1).to_string()));
        let resp = self
            .http
            .get(self.url("/ws-user"))
            .bearer_auth(&self.api_key)
            .query(&params)
            .send()
            .await
            .context("GET /ws-user")?;
        let d: WsUserData = Self::parse(resp, "ws-user").await?;
        Ok(d.results)
    }

    /// 设置单个文档权限（覆盖可见 / 可删除两个维度；Emoo 端不支持可编辑参数）。
    /// 接口要求 Emoo-User-Id 头（调用者 open_id）且调用者须为工作区超管；
    /// 若 client 未配置 emoo_user_id 则不带头（依赖绑定 key 隐含身份，可能被拒）。
    pub async fn set_kb_file_permission(
        &self,
        doc_key: &str,
        setting: &PermissionSetting,
    ) -> Result<()> {
        #[derive(Serialize)]
        struct Req<'a> {
            visible: &'a DocPermissionAudience,
            deletable: &'a DocPermissionAudience,
        }
        let body = Req {
            visible: &setting.visible,
            deletable: &setting.deletable,
        };
        let mut req = self
            .http
            .put(self.url(&format!("/data/kb-file/{doc_key}/permission")))
            .bearer_auth(&self.api_key)
            .json(&body);
        if let Some(uid) = &self.emoo_user_id {
            req = req.header("Emoo-User-Id", uid);
        }
        let resp = req
            .send()
            .await
            .context("PUT /data/kb-file/{doc_key}/permission")?;
        Self::parse_unit(resp, "kb-file/permission").await
    }

    /// 完整同步一个本地文件到知识库：upload-credentials → 直传存储 → confirm → kb-file。
    pub async fn sync_file(&self, path: &Path, folder_id: Option<i64>) -> Result<KbFileDoc> {
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow!("无法解析文件名"))?
            .to_string();

        // 同步前先确认源文件还在（可能在选中后、上传前被移动/删除）。
        if !path.exists() {
            bail!("源文件不存在，可能已被移动或删除：{path:?}");
        }

        let (allowed, mime, is_img) = ext_info(&file_name);
        if !allowed {
            bail!("扩展名不在知识库白名单内（支持 pdf/doc(x)/pptx/xls(x)/csv/txt/md/html/json/xml 与 jpg/png/gif/bmp）");
        }
        let bytes = std::fs::read(path).with_context(|| format!("读取文件失败 {path:?}"))?;
        let file_size = bytes.len() as u64;
        let cap = if is_img { IMG_CAP } else { FILE_CAP };
        if file_size > cap {
            bail!("文件过大：{file_size} 字节超过上限 {cap}（图片 20MB / 其它 100MB）");
        }
        let mime_c = mime.to_string();

        // 1) upload-credentials
        let creds = {
            let body = UcReq {
                files: vec![UcItem {
                    file_name: &file_name,
                    file_size,
                    mime_type: Some(&mime_c),
                }],
            };
            let resp = self
                .http
                .post(self.url("/data/files/upload-credentials"))
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await
                .context("POST /data/files/upload-credentials")?;
            let arr: Vec<UcResult> = Self::parse(resp, "upload-credentials").await?;
            arr.into_iter()
                .next()
                .ok_or_else(|| anyhow!("upload-credentials 返回空列表"))?
        };

        // 2) 直传对象存储
        self.upload_to_storage(&creds.upload, bytes, &file_name)
            .await?;

        // 3) confirm
        {
            let body = ConfirmReq {
                files: vec![ConfirmItem {
                    file_key: &creds.file_key,
                    file_name: &file_name,
                    file_size,
                    mime_type: Some(&mime_c),
                }],
            };
            let resp = self
                .http
                .post(self.url("/data/files/confirm"))
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await
                .context("POST /data/files/confirm")?;
            let _: Vec<ConfirmResult> = Self::parse(resp, "confirm").await?;
        }

        // 4) 登记为知识库文档（触发检索索引）
        let body = KbCreateReq {
            file_key: &creds.file_key,
            file_name: &file_name,
            file_size,
            mime_type: Some(&mime_c),
            folder_id,
            title: Some(&file_name),
        };
        let resp = self
            .http
            .post(self.url("/data/kb-file"))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .context("POST /data/kb-file")?;
        let doc: KbFileDoc = Self::parse(resp, "kb-file").await?;
        Ok(doc)
    }

    /// 按指令把字节直传对象存储（PUT presigned 或 POST multipart）。
    async fn upload_to_storage(
        &self,
        instr: &UploadInstr,
        bytes: Vec<u8>,
        file_name: &str,
    ) -> Result<()> {
        match instr.method.to_ascii_uppercase().as_str() {
            "PUT" => {
                let mut req = self.http.put(&instr.url).body(bytes);
                for (k, v) in &instr.headers {
                    req = req.header(k, v);
                }
                let resp = req.send().await.context("PUT 到对象存储")?;
                if !resp.status().is_success() {
                    let s = resp.status();
                    let t = resp.text().await.unwrap_or_default();
                    bail!("对象存储 PUT 失败：HTTP {s}；body: {t}");
                }
                Ok(())
            }
            "POST" => {
                let mut form = reqwest::multipart::Form::new();
                for (k, v) in &instr.form_fields {
                    form = form.text(k.clone(), v.clone());
                }
                let field = instr
                    .file_field
                    .clone()
                    .unwrap_or_else(|| "file".to_string());
                let part = reqwest::multipart::Part::bytes(bytes).file_name(file_name.to_string());
                form = form.part(field, part);
                let resp = self
                    .http
                    .post(&instr.url)
                    .multipart(form)
                    .send()
                    .await
                    .context("POST 到对象存储")?;
                if !resp.status().is_success() {
                    let s = resp.status();
                    let t = resp.text().await.unwrap_or_default();
                    bail!("对象存储 POST 失败：HTTP {s}；body: {t}");
                }
                Ok(())
            }
            other => bail!("不支持的上传方式：{other}"),
        }
    }
}

/// 返回 (是否白名单内, mime, 是否图片)。
pub(crate) fn ext_info(file_name: &str) -> (bool, &'static str, bool) {
    let ext = file_name
        .rsplit('.')
        .next()
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    let (mime, is_img) = match ext.as_str() {
        "pdf" => ("application/pdf", false),
        "docx" => (
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            false,
        ),
        "doc" => ("application/msword", false),
        "pptx" => (
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            false,
        ),
        "xlsx" => (
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            false,
        ),
        "xls" => ("application/vnd.ms-excel", false),
        "csv" => ("text/csv", false),
        "txt" => ("text/plain", false),
        "md" | "markdown" => ("text/markdown", false),
        "html" | "htm" => ("text/html", false),
        "json" => ("application/json", false),
        "xml" => ("application/xml", false),
        "jpg" | "jpeg" => ("image/jpeg", true),
        "png" => ("image/png", true),
        "gif" => ("image/gif", true),
        "bmp" => ("image/bmp", true),
        _ => return (false, "application/octet-stream", false),
    };
    (true, mime, is_img)
}
