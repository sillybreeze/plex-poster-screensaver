//! 当前用户的开机自启：在“启动”文件夹创建 `.lnk` 快捷方式。
//!
//! 不写注册表，不需要管理员权限，也不依赖正在被 Windows 弃用的 VBScript；
//! release 版 exe 本身使用 `windows` 子系统，登录时由系统直接以 `--background`
//! 启动且不会闪烁控制台窗口。
//!
//! 兼容并自动清理旧版 `PlexPosterScreensaver.vbs`：检测到旧脚本时仍视为已启用，
//! 重新保存或关闭自启时会将其移除。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::logging::log_warn;

const SHORTCUT_NAME: &str = "PlexPosterScreensaver.lnk";
const LEGACY_SCRIPT_NAME: &str = "PlexPosterScreensaver.vbs";
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

struct LaunchSpec {
    exe: PathBuf,
    args: String,
    working_dir: Option<PathBuf>,
}

fn startup_dir() -> Result<PathBuf, String> {
    let appdata = std::env::var_os("APPDATA").ok_or_else(|| "没有 APPDATA 环境变量".to_string())?;
    Ok(PathBuf::from(appdata).join(r"Microsoft\Windows\Start Menu\Programs\Startup"))
}

pub fn shortcut_path() -> Result<PathBuf, String> {
    Ok(startup_dir()?.join(SHORTCUT_NAME))
}

fn legacy_script_path() -> Result<PathBuf, String> {
    Ok(startup_dir()?.join(LEGACY_SCRIPT_NAME))
}

/// 读取当前用户启动文件夹里是否存在本程序的自启快捷方式（兼容旧 `.vbs`）。
pub fn is_enabled() -> bool {
    let lnk = shortcut_path().ok();
    let legacy = legacy_script_path().ok();
    is_enabled_at(lnk.as_deref(), legacy.as_deref())
}

fn is_enabled_at(lnk: Option<&Path>, legacy: Option<&Path>) -> bool {
    lnk.is_some_and(Path::is_file) || legacy.is_some_and(Path::is_file)
}

fn launch_spec(config_path: Option<&Path>) -> Result<LaunchSpec, String> {
    let exe = std::env::current_exe().map_err(|err| format!("获取当前 exe 路径失败: {err}"))?;
    let mut args = "--background".to_string();
    if let Some(path) = config_path {
        let path = absolute_config_path(path)?;
        args.push_str(&format!(" --config \"{}\"", path.display()));
    }
    let working_dir = exe
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(Path::to_path_buf);
    Ok(LaunchSpec {
        exe,
        args,
        working_dir,
    })
}

fn absolute_config_path(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .map_err(|err| format!("解析配置文件绝对路径失败: {err}"))
}

fn to_wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
fn create_shortcut(lnk_path: &Path, spec: &LaunchSpec) -> Result<(), String> {
    use windows::core::{w, Interface, PCWSTR};
    use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, TRUE};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};

    struct ComGuard(bool);
    impl Drop for ComGuard {
        fn drop(&mut self) {
            if self.0 {
                unsafe { CoUninitialize() };
            }
        }
    }

    let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if com.is_err() && com != RPC_E_CHANGED_MODE {
        return Err(format!("初始化 COM 失败: 0x{:08X}", com.0));
    }
    let _guard = ComGuard(com.is_ok());

    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
            .map_err(|err| format!("创建快捷方式对象失败: {err}"))?;

        let exe_str = spec.exe.to_string_lossy();
        let wide_exe = to_wide(&exe_str);
        link.SetPath(PCWSTR(wide_exe.as_ptr()))
            .map_err(|err| format!("设置快捷方式目标失败: {err}"))?;

        let wide_args = to_wide(&spec.args);
        link.SetArguments(PCWSTR(wide_args.as_ptr()))
            .map_err(|err| format!("设置快捷方式参数失败: {err}"))?;

        if let Some(dir) = &spec.working_dir {
            let dir_str = dir.to_string_lossy();
            let wide_dir = to_wide(&dir_str);
            link.SetWorkingDirectory(PCWSTR(wide_dir.as_ptr()))
                .map_err(|err| format!("设置快捷方式工作目录失败: {err}"))?;
        }

        link.SetDescription(w!("Plex 海报屏保（后台常驻）"))
            .map_err(|err| format!("设置快捷方式描述失败: {err}"))?;
        let _ = link.SetIconLocation(PCWSTR(wide_exe.as_ptr()), 0);

        let persist: IPersistFile = link
            .cast()
            .map_err(|err| format!("获取 IPersistFile 失败: {err}"))?;
        let tmp = temp_shortcut_path(lnk_path);
        let tmp_str = tmp.to_string_lossy();
        let wide_tmp = to_wide(&tmp_str);
        if let Err(err) = persist.Save(PCWSTR(wide_tmp.as_ptr()), TRUE) {
            let _ = fs::remove_file(&tmp);
            return Err(format!(
                "保存开机自启快捷方式失败 {}: {err}",
                lnk_path.display()
            ));
        }
        if let Err(err) = fs::rename(&tmp, lnk_path) {
            let _ = fs::remove_file(&tmp);
            return Err(format!(
                "保存开机自启快捷方式失败 {}: {err}",
                lnk_path.display()
            ));
        }
        Ok(())
    }
}

fn temp_shortcut_path(lnk_path: &Path) -> PathBuf {
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let stem = lnk_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("PlexPosterScreensaver");
    lnk_path.with_file_name(format!(".{stem}.{}.{seq}.tmp.lnk", std::process::id()))
}

#[cfg(not(windows))]
fn create_shortcut(_lnk_path: &Path, _spec: &LaunchSpec) -> Result<(), String> {
    Err("当前平台不支持创建 Windows 快捷方式".to_string())
}

fn with_low_integrity_hint(message: String) -> String {
    if crate::platform::is_low_integrity() {
        format!("{message}\n\n{}", crate::platform::low_integrity_advice())
    } else {
        message
    }
}

fn remove_if_exists(path: &Path, kind: &str) -> Result<(), String> {
    if !path.is_file() {
        return Ok(());
    }
    fs::remove_file(path).map_err(|err| {
        with_low_integrity_hint(format!(
            "删除{kind}失败 {}: {err}。请检查启动文件夹权限，或手动删除该文件",
            path.display()
        ))
    })
}

pub fn enable(config_path: Option<&Path>) -> Result<(), String> {
    let lnk = shortcut_path()?;
    let legacy = legacy_script_path().ok();
    enable_at(&lnk, legacy.as_deref(), config_path)
}

fn enable_at(lnk: &Path, legacy: Option<&Path>, config_path: Option<&Path>) -> Result<(), String> {
    enable_at_with(lnk, legacy, config_path, |path| {
        remove_if_exists(path, "旧版自启脚本")
    })
}

fn enable_at_with(
    lnk: &Path,
    legacy: Option<&Path>,
    config_path: Option<&Path>,
    remove_legacy: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<(), String> {
    let path = config_path.ok_or_else(|| "没有配置文件位置，无法设置开机自启".to_string())?;
    let spec = launch_spec(Some(path))?;
    if let Some(parent) = lnk.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| with_low_integrity_hint(format!("创建启动文件夹失败: {err}")))?;
    }
    create_shortcut(lnk, &spec).map_err(with_low_integrity_hint)?;
    // 旧 .vbs 清理降级为 best-effort：.lnk 已写入成功，不因旧脚本删不掉而误报自启未更新。
    if let Some(legacy) = legacy.filter(|p| p.is_file()) {
        if let Err(err) = remove_legacy(legacy) {
            log_warn!("{err}；新版 .lnk 快捷方式已生效，建议手动删除旧版 .vbs 以免重复拉起");
        }
    }
    Ok(())
}

pub fn disable() -> Result<(), String> {
    let lnk = shortcut_path()?;
    let legacy = legacy_script_path().ok();
    disable_at(&lnk, legacy.as_deref())
}

fn disable_at(lnk: &Path, legacy: Option<&Path>) -> Result<(), String> {
    remove_if_exists(lnk, "开机自启快捷方式")?;
    if let Some(legacy) = legacy {
        remove_if_exists(legacy, "旧版自启脚本")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    #[test]
    fn launch_spec_has_background_and_config() {
        let spec = super::launch_spec(Some(Path::new(r"C:\tmp\config.json"))).expect("spec");
        assert!(spec.args.contains("--background"));
        assert!(spec.args.contains("--config"));
        assert!(spec.args.contains(r#"--config "C:\tmp\config.json""#));
        assert!(spec.working_dir.is_some());
    }

    #[test]
    fn launch_spec_quotes_paths_with_spaces() {
        let spec = super::launch_spec(Some(Path::new(r"C:\tmp\a b\config.json"))).expect("spec");
        assert!(spec.args.contains(r#"--config "C:\tmp\a b\config.json""#));
    }

    #[test]
    fn launch_spec_resolves_relative_config_path() {
        let spec = super::launch_spec(Some(Path::new("config.json"))).expect("spec");
        let expected = std::env::current_dir()
            .unwrap()
            .join("config.json")
            .display()
            .to_string();
        assert!(spec.args.contains(&expected));
    }

    #[test]
    fn create_shortcut_writes_valid_lnk_file() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("autostart-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let lnk = dir.join("test.lnk");
        let spec = super::launch_spec(Some(Path::new(r"C:\tmp\config.json"))).unwrap();

        super::create_shortcut(&lnk, &spec).expect("create shortcut");
        let bytes = std::fs::read(&lnk).expect("read lnk");
        // Shell Link (.lnk) 文件头前 4 字节固定为 HeaderSize = 0x0000004C。
        assert!(bytes.len() > 76);
        assert_eq!(&bytes[..4], &[0x4C, 0x00, 0x00, 0x00]);
        // 再次覆盖写入应保持原子替换且不残留临时文件。
        super::create_shortcut(&lnk, &spec).expect("overwrite shortcut");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "不应残留临时快捷方式文件");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enable_migrates_legacy_vbs_and_disable_cleans_both() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("autostart-migrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let lnk = dir.join("PlexPosterScreensaver.lnk");
        let legacy = dir.join("PlexPosterScreensaver.vbs");
        std::fs::write(&legacy, b"legacy").unwrap();

        assert!(super::is_enabled_at(Some(&lnk), Some(&legacy)));
        super::enable_at(&lnk, Some(&legacy), Some(Path::new(r"C:\tmp\config.json")))
            .expect("enable migrates legacy");
        assert!(lnk.is_file(), "应创建新的 .lnk 快捷方式");
        assert!(!legacy.exists(), "迁移后应删除旧版 .vbs");
        assert!(super::is_enabled_at(Some(&lnk), Some(&legacy)));

        // 旧版 .vbs 删除失败时，只要 .lnk 已就绪仍应视为启用成功。
        std::fs::write(&legacy, b"locked").unwrap();
        super::enable_at_with(
            &lnk,
            Some(&legacy),
            Some(Path::new(r"C:\tmp\config.json")),
            |_| Err("模拟权限不足".to_string()),
        )
        .expect("legacy cleanup failure must not fail enable");
        assert!(lnk.is_file());

        super::disable_at(&lnk, Some(&legacy)).expect("disable removes both");
        assert!(!lnk.exists());
        assert!(!legacy.exists());
        assert!(!super::is_enabled_at(Some(&lnk), Some(&legacy)));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
