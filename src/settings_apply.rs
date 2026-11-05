//! 设置保存的应用层：只负责“写配置、同步开机自启、请求重启”的顺序和失败语义。
//!
//! 原生窗口只负责读取控件和展示错误。把这一层做成可注入依赖的纯 Rust 模块后，
//! 保存失败、自启失败、重启请求失败等路径可以不启动 Windows GUI 直接测试。

use std::path::Path;

use crate::config::EditableSettings;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplyOutcome {
    /// 是否已经向运行中的后台实例发出重启请求。
    pub restarted: bool,
    /// 配置已写入，但开机自启状态同步失败时的错误。
    pub autostart_error: Option<String>,
}

trait Applier {
    fn save(&self, path: Option<&Path>, settings: &EditableSettings) -> Result<(), String>;
    fn autostart_enabled(&self) -> bool;
    fn set_autostart(&self, enabled: bool, path: Option<&Path>) -> Result<(), String>;
    fn request_restart(&self) -> bool;
}

struct SystemApplier;

impl Applier for SystemApplier {
    fn save(&self, path: Option<&Path>, settings: &EditableSettings) -> Result<(), String> {
        crate::config::save_editable_settings(path, settings)
    }

    fn autostart_enabled(&self) -> bool {
        crate::autostart::is_enabled()
    }

    fn set_autostart(&self, enabled: bool, path: Option<&Path>) -> Result<(), String> {
        if enabled {
            crate::autostart::enable(path)
        } else {
            crate::autostart::disable()
        }
    }

    fn request_restart(&self) -> bool {
        crate::platform::request_restart()
    }
}

/// 保存配置；自启同步失败不回滚配置，也不阻止重启请求。
pub fn apply(
    path: Option<&Path>,
    settings: &EditableSettings,
    autostart: bool,
) -> Result<ApplyOutcome, String> {
    apply_with(&SystemApplier, path, settings, autostart)
}

fn apply_with(
    applier: &impl Applier,
    path: Option<&Path>,
    settings: &EditableSettings,
    autostart: bool,
) -> Result<ApplyOutcome, String> {
    applier.save(path, settings)?;

    let autostart_error = if autostart {
        applier.set_autostart(true, path).err()
    } else if applier.autostart_enabled() {
        applier.set_autostart(false, path).err()
    } else {
        None
    };

    let restarted = applier.request_restart();
    Ok(ApplyOutcome {
        restarted,
        autostart_error,
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    #[derive(Default)]
    struct FakeApplier {
        save_ok: bool,
        enabled: bool,
        set_autostart_ok: bool,
        restart: bool,
        calls: RefCell<Vec<String>>,
    }

    impl FakeApplier {
        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }

        fn record(&self, call: impl Into<String>) {
            self.calls.borrow_mut().push(call.into());
        }
    }

    impl Applier for FakeApplier {
        fn save(&self, _path: Option<&Path>, _settings: &EditableSettings) -> Result<(), String> {
            self.record("save");
            if self.save_ok {
                Ok(())
            } else {
                Err("save failed".to_string())
            }
        }

        fn autostart_enabled(&self) -> bool {
            self.enabled
        }

        fn set_autostart(&self, enabled: bool, _path: Option<&Path>) -> Result<(), String> {
            self.record(if enabled {
                "autostart-on"
            } else {
                "autostart-off"
            });
            if self.set_autostart_ok {
                Ok(())
            } else {
                Err("autostart failed".to_string())
            }
        }

        fn request_restart(&self) -> bool {
            self.record("restart");
            self.restart
        }
    }

    fn settings() -> EditableSettings {
        EditableSettings::default()
    }

    #[test]
    fn save_failure_stops_before_autostart_and_restart() {
        let applier = FakeApplier::default();
        let err = apply_with(&applier, Some(Path::new("config.json")), &settings(), true)
            .expect_err("save failure");
        assert_eq!(err, "save failed");
        assert_eq!(applier.calls(), ["save"]);
    }

    #[test]
    fn autostart_failure_does_not_block_restart() {
        let applier = FakeApplier {
            save_ok: true,
            set_autostart_ok: false,
            restart: true,
            ..FakeApplier::default()
        };
        let outcome =
            apply_with(&applier, Some(Path::new("config.json")), &settings(), true).expect("saved");
        assert_eq!(outcome.autostart_error.as_deref(), Some("autostart failed"));
        assert!(outcome.restarted);
        assert_eq!(applier.calls(), ["save", "autostart-on", "restart"]);
    }

    #[test]
    fn disabling_autostart_only_when_currently_enabled() {
        let applier = FakeApplier {
            save_ok: true,
            enabled: true,
            set_autostart_ok: true,
            restart: false,
            ..FakeApplier::default()
        };
        let outcome = apply_with(&applier, None, &settings(), false).expect("saved");
        assert_eq!(outcome.autostart_error, None);
        assert!(!outcome.restarted);
        assert_eq!(applier.calls(), ["save", "autostart-off", "restart"]);

        let disabled = FakeApplier {
            save_ok: true,
            enabled: false,
            ..FakeApplier::default()
        };
        let outcome = apply_with(&disabled, None, &settings(), false).expect("saved");
        assert!(!outcome.restarted);
        assert_eq!(disabled.calls(), ["save", "restart"]);
    }

    #[test]
    fn successful_save_reports_restart() {
        let applier = FakeApplier {
            save_ok: true,
            set_autostart_ok: true,
            restart: true,
            ..FakeApplier::default()
        };
        let outcome = apply_with(&applier, None, &settings(), true).expect("saved");
        assert_eq!(outcome.autostart_error, None);
        assert!(outcome.restarted);
        assert_eq!(applier.calls(), ["save", "autostart-on", "restart"]);
    }
}
