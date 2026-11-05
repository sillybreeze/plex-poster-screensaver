//! GUI 保存与启动加载共用的校验规则。
//!
//! 只放纯谓词和范围常量；具体字段名/给用户的文案由调用方决定，
//! 避免设置窗口和启动路径对“什么算合法”产生分歧。

pub(crate) const MIN_DISPLAY_SECONDS: f64 = 0.1;
pub(crate) const MAX_DISPLAY_SECONDS: f64 = 3600.0;
pub(crate) const MIN_TRANSITION_SECONDS: f64 = 0.1;
pub(crate) const MAX_TRANSITION_SECONDS: f64 = 3.0;
pub(crate) const MIN_IDLE_MINUTES: f64 = 5.0 / 60.0;
pub(crate) const MAX_IDLE_MINUTES: f64 = 1440.0;
pub(crate) const MIN_IDLE_SECONDS: f64 = 5.0;
pub(crate) const MAX_IDLE_SECONDS: f64 = 86_400.0;
pub(crate) const MIN_TIMEOUT_SECONDS: u64 = 1;
pub(crate) const MAX_TIMEOUT_SECONDS: u64 = 600;
pub(crate) const MIN_MOTION_FPS: u32 = 1;
pub(crate) const MAX_MOTION_FPS: u32 = 120;
pub(crate) const MIN_CACHE_DAYS: i64 = 0;
pub(crate) const MAX_CACHE_DAYS: i64 = 36_500;

pub(crate) fn finite_in_range(value: f64, min: f64, max: f64) -> bool {
    value.is_finite() && (min..=max).contains(&value)
}

pub(crate) fn valid_display_seconds(value: f64) -> bool {
    finite_in_range(value, MIN_DISPLAY_SECONDS, MAX_DISPLAY_SECONDS)
}

/// 0 是旧 `--fade 0` 的立即切换语义，仍然合法。
pub(crate) fn valid_transition_seconds(value: f64) -> bool {
    value.is_finite()
        && (value == 0.0 || (MIN_TRANSITION_SECONDS..=MAX_TRANSITION_SECONDS).contains(&value))
}

pub(crate) fn valid_idle_minutes(value: f64) -> bool {
    value.is_finite() && (MIN_IDLE_MINUTES..=MAX_IDLE_MINUTES).contains(&value)
}

pub(crate) fn valid_idle_seconds(value: f64) -> bool {
    value.is_finite() && (MIN_IDLE_SECONDS..=MAX_IDLE_SECONDS).contains(&value)
}

pub(crate) fn valid_timeout(value: u64) -> bool {
    (MIN_TIMEOUT_SECONDS..=MAX_TIMEOUT_SECONDS).contains(&value)
}

pub(crate) fn valid_motion_fps(value: u32) -> bool {
    (MIN_MOTION_FPS..=MAX_MOTION_FPS).contains(&value)
}

pub(crate) fn valid_cache_days(value: i64) -> bool {
    (MIN_CACHE_DAYS..=MAX_CACHE_DAYS).contains(&value)
}

pub(crate) fn valid_url(value: &str) -> bool {
    let url = value.trim();
    !url.is_empty() && (url.starts_with("http://") || url.starts_with("https://"))
}

pub(crate) fn valid_token(value: &str, require_token: bool) -> bool {
    !require_token || !value.trim().is_empty()
}

pub(crate) fn valid_cache_dir(cache_auto: bool, value: &str) -> bool {
    cache_auto || !value.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_boundaries_are_shared() {
        assert!(valid_display_seconds(0.1));
        assert!(valid_display_seconds(3600.0));
        assert!(!valid_display_seconds(f64::NAN));
        assert!(!valid_display_seconds(0.09));

        assert!(valid_transition_seconds(0.0));
        assert!(valid_transition_seconds(3.0));
        assert!(!valid_transition_seconds(0.09));
        assert!(!valid_transition_seconds(f64::INFINITY));

        assert!(valid_idle_minutes(5.0 / 60.0));
        assert!(valid_idle_seconds(5.0));
        assert!(!valid_idle_seconds(4.999));
        assert!(!valid_idle_minutes(f64::NAN));

        assert!(valid_timeout(1));
        assert!(valid_timeout(600));
        assert!(!valid_timeout(0));
        assert!(valid_motion_fps(120));
        assert!(!valid_motion_fps(121));
        assert!(valid_cache_days(0));
        assert!(valid_cache_days(36_500));
        assert!(!valid_cache_days(-1));
    }

    #[test]
    fn url_token_and_cache_dir_rules_are_shared() {
        assert!(valid_url(" https://plex.example "));
        assert!(!valid_url(""));
        assert!(!valid_url("plex.example"));
        assert!(valid_token("", false));
        assert!(!valid_token("", true));
        assert!(valid_token("abc", true));
        assert!(valid_cache_dir(true, ""));
        assert!(!valid_cache_dir(false, " "));
    }
}
