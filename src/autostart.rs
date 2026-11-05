//! 当前用户的开机自启：在“启动”文件夹放一个隐藏运行的 VBS。
//!
//! 不写注册表，不需要管理员权限；登录时由 Windows 执行该脚本，
//! 脚本再以 `--background` 启动当前 exe。
//!
//! 不用 Run 键是因为部分受控环境会拒绝普通进程打开 Run 键；启动文件夹
//! 对普通用户始终可写，也方便用户自己在资源管理器里删除。

use std::fs;
use std::path::{Path, PathBuf};

const SCRIPT_NAME: &str = "PlexPosterScreensaver.vbs";

fn startup_dir() -> Result<PathBuf, String> {
    let appdata = std::env::var_os("APPDATA").ok_or_else(|| "没有 APPDATA 环境变量".to_string())?;
    Ok(PathBuf::from(appdata).join(r"Microsoft\Windows\Start Menu\Programs\Startup"))
}

pub fn script_path() -> Result<PathBuf, String> {
    Ok(startup_dir()?.join(SCRIPT_NAME))
}

/// 读取当前用户启动文件夹里是否存在本程序的启动脚本。
pub fn is_enabled() -> bool {
    script_path().map(|path| path.is_file()).unwrap_or(false)
}

/// 生成 VBS：窗口样式 0 = 隐藏，避免登录时闪控制台。
pub fn script(config_path: Option<&Path>) -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|err| format!("获取当前 exe 路径失败: {err}"))?;
    let mut command = format!("\"{}\" --background", exe.display());
    if let Some(path) = config_path {
        command.push_str(&format!(" --config \"{}\"", path.display()));
    }
    // VBScript 字符串里的双引号写成两个双引号。
    let escaped = command.replace('"', "\"\"");
    Ok(format!(
        "' Plex Poster Screensaver autostart (generated; delete this file to disable)\r\n\
         Set shell = CreateObject(\"WScript.Shell\")\r\n\
         shell.Run \"{escaped}\", 0, False\r\n"
    ))
}

/// 写 UTF-16 LE + BOM，避免中文用户名/路径在 VBScript 里乱码。
fn write_script(path: &Path, text: &str) -> Result<(), String> {
    let mut bytes = Vec::with_capacity(2 + text.len() * 2);
    bytes.extend_from_slice(&[0xFF, 0xFE]);
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    fs::write(path, bytes).map_err(|err| format!("写入开机自启脚本失败: {err}"))
}

pub fn enable(config_path: Option<&Path>) -> Result<(), String> {
    let path = config_path.ok_or_else(|| "没有配置文件位置，无法设置开机自启".to_string())?;
    let text = script(Some(path))?;
    let file = script_path()?;
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("创建启动文件夹失败: {err}"))?;
    }
    write_script(&file, &text)
}

pub fn disable() -> Result<(), String> {
    let file = script_path()?;
    if !file.is_file() {
        return Ok(());
    }
    fs::remove_file(&file).map_err(|err| format!("删除开机自启脚本失败: {err}"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    #[test]
    fn script_has_background_and_config() {
        let text = super::script(Some(Path::new(r"C:\tmp\config.json"))).expect("script");
        assert!(text.contains("--background"));
        assert!(text.contains("--config"));
        assert!(text.contains("config.json"));
        assert!(text.contains("shell.Run"));
    }

    #[test]
    fn script_escapes_quotes() {
        let text = super::script(Some(Path::new(r"C:\tmp\a b\config.json"))).expect("script");
        assert!(text.contains(r#"--config ""C:\tmp\a b\config.json"""#));
    }
}
