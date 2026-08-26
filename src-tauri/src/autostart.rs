//! 开机自启：包装 tauri-plugin-autostart（注册表 / launchd / .desktop），
//! Windows 下注册表 Run 键被管控软件拒绝（os error 5）时兜底改为在
//! 启动文件夹创建 .lnk 快捷方式 —— 用户自己的目录，大多数管控机器仍可写。

use tauri::AppHandle;
use tauri_plugin_autostart::ManagerExt;

/// 启用：先走插件标准路径；Windows 失败时兜底写启动文件夹快捷方式。
#[tauri::command]
pub async fn autostart_enable(app: AppHandle) -> Result<(), String> {
    match app.autolaunch().enable() {
        Ok(()) => Ok(()),
        Err(reg_err) => {
            #[cfg(windows)]
            {
                match create_startup_lnk(&app) {
                    Ok(()) => Ok(()),
                    Err(lnk_err) => Err(format!(
                        "注册表写入失败（{reg_err}）；启动文件夹快捷方式也失败（{lnk_err}）。\
                         公司管控电脑常见此情况，请让 IT 放行：允许写入注册表 Run 键或启动文件夹"
                    )),
                }
            }
            #[cfg(not(windows))]
            Err(reg_err.to_string())
        }
    }
}

/// 禁用：清掉所有启用途径。注册表值不存在（走的是兜底方式）不算失败。
#[tauri::command]
pub async fn autostart_disable(app: AppHandle) -> Result<(), String> {
    #[cfg(windows)]
    let lnk_existed = remove_startup_lnk(&app);

    let reg_result = app.autolaunch().disable().map_err(|e| e.to_string());

    #[cfg(windows)]
    {
        match (reg_result, lnk_existed) {
            (Ok(()), _) => Ok(()),
            // 注册表本来就没有（走的是兜底），快捷方式已删 → 已禁用
            (Err(_), Some(true)) => Ok(()),
            // 两处都没有可删的东西还报错 → 真错误
            (Err(e), Some(false)) => Err(e),
            (Err(e), None) => Err(format!("删除启动文件夹快捷方式失败：{e}")),
        }
    }
    #[cfg(not(windows))]
    reg_result
}

/// 状态：任一启用途径生效即算启用。注册表读不了（被管控）时只反映快捷方式。
#[tauri::command]
pub async fn autostart_is_enabled(app: AppHandle) -> Result<bool, String> {
    let plugin_on = app.autolaunch().is_enabled().unwrap_or(false);
    #[cfg(windows)]
    {
        Ok(plugin_on || startup_lnk_path(&app).map(|p| p.exists()).unwrap_or(false))
    }
    #[cfg(not(windows))]
    Ok(plugin_on)
}

// ---------------- Windows：启动文件夹 .lnk 兜底 ----------------

/// 快捷方式目标路径：<Startup>\<应用名>.lnk（应用名与插件注册表值一致）。
#[cfg(windows)]
fn startup_lnk_path(app: &AppHandle) -> Option<std::path::PathBuf> {
    startup_dir().map(|d| d.join(format!("{}.lnk", app.package_info().name)))
}

/// 用户 Startup 文件夹（经 SHGetKnownFolderPath，兼容域环境文件夹重定向）。
#[cfg(windows)]
fn startup_dir() -> Option<std::path::PathBuf> {
    use windows::Win32::UI::Shell::{FOLDERID_Startup, KNOWN_FOLDER_FLAG, SHGetKnownFolderPath};
    unsafe {
        SHGetKnownFolderPath(&FOLDERID_Startup, KNOWN_FOLDER_FLAG(0), None)
            .ok()
            .map(|p| {
                let s = p.to_string().unwrap_or_default();
                windows::Win32::System::Com::CoTaskMemFree(Some(p.as_ptr() as _));
                std::path::PathBuf::from(s)
            })
    }
}

/// 在 Startup 文件夹创建指向当前 exe 的快捷方式（覆盖已存在的同名文件）。
#[cfg(windows)]
fn create_startup_lnk(app: &AppHandle) -> Result<(), String> {
    use windows::core::{Interface, HSTRING};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};

    let Some(lnk) = startup_lnk_path(app) else {
        return Err("无法定位启动文件夹".into());
    };
    let exe = std::env::current_exe().map_err(|e| format!("取当前 exe 路径失败：{e}"))?;

    unsafe {
        // 本线程初始化 COM（已按其它模型初始化过时照样可用，只是不再反初始化）
        let co = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let must_uninit = co.is_ok();

        let result = (|| -> windows::core::Result<()> {
            let link: IShellLinkW =
                CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            link.SetPath(&HSTRING::from(exe.as_os_str()))?;
            link.SetWorkingDirectory(&HSTRING::from(
                exe.parent().unwrap_or(std::path::Path::new("")).as_os_str(),
            ))?;
            link.SetDescription(&HSTRING::from(format!("{} 开机自启", app.package_info().name)))?;
            let persist: IPersistFile = link.cast()?;
            persist.Save(&HSTRING::from(lnk.as_os_str()), false)?;
            Ok(())
        })();

        if must_uninit {
            CoUninitialize();
        }
        result.map_err(|e| format!("写入启动文件夹失败（{}）：{e}", lnk.display()))
    }
}

/// 删除兜底快捷方式。返回 Some(true)=已删，Some(false)=本来就没有，None=删除失败。
#[cfg(windows)]
fn remove_startup_lnk(app: &AppHandle) -> Option<bool> {
    let lnk = startup_lnk_path(app)?;
    if !lnk.exists() {
        return Some(false);
    }
    match std::fs::remove_file(&lnk) {
        Ok(()) => Some(true),
        Err(_) => None,
    }
}
