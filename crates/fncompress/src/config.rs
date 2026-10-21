//! Configuration changes with validation: global and server settings,
//! saved profiles, monitored directories. Every change takes effect at once
//! (ADR 0003): new tasks snapshot the new values, the scheduler re-reads its
//! slot counts, the monitor reloads its directories.

use std::path::PathBuf;

use fncompress_core::settings::{GlobalSettings, Profile};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{
    app::App,
    error::{AppError, AppResult},
    model::{MonitorDir, SavedProfile, ServerSettings},
    scan::{self, Excludes},
};

/// Everything under `/api/settings`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Settings {
    pub global: GlobalSettings,
    pub server: ServerSettings,
    /// Files the observation mode has held so far.
    pub observed: u32,
    /// The user has confirmed the risk notice the UI shows on first open
    /// (test software, transcodes replace the source). Kept here rather than
    /// in the browser, so the gateway and LAN entries share it.
    pub risk_acknowledged: bool,
}

/// Replaces the parts that are present.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SettingsUpdate {
    pub global: Option<GlobalSettings>,
    pub server: Option<ServerSettings>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct MonitorInput {
    #[schema(value_type = String)]
    pub path: PathBuf,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// The server's default profile when absent.
    pub profile_id: Option<String>,
    #[serde(default = "yes")]
    pub recursive: bool,
    #[serde(default)]
    pub excludes: Vec<String>,
}

fn yes() -> bool {
    true
}

fn invalid(message: impl Into<String>) -> AppError {
    AppError::bad_request("invalid_settings", message)
}

/// Profile ids end up in the processed marker `FNCOMPRESS=1;v=..;profile=..`.
fn valid_profile_id(id: &str) -> bool {
    (1..=40).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn check_profile(p: &SavedProfile) -> AppResult<()> {
    if !valid_profile_id(&p.id) {
        return Err(invalid("方案 id 只能用 1–40 个字母、数字、- 和 _"));
    }
    if p.name.trim().is_empty() {
        return Err(invalid("方案名称不能为空"));
    }
    if p.profile.quality > 63 {
        return Err(invalid("质量参数超出范围（0–63）"));
    }
    Ok(())
}

fn check_global(g: &GlobalSettings) -> AppResult<()> {
    if !(0.0..100.0).contains(&g.min_savings_pct) {
        return Err(invalid("最低节省比例应在 0–100% 之间"));
    }
    for (what, days) in [
        ("回收目录保留天数", g.recycle_retention_days),
        ("待确认成品保留天数", g.pending_retention_days),
    ] {
        if !(1..=3650).contains(&days) {
            return Err(invalid(format!("{what}应在 1–3650 之间")));
        }
    }
    if g.encode_timeout_factor <= 0.0 {
        return Err(invalid("编码超时系数必须大于 0"));
    }
    if g.ffmpeg.as_os_str().is_empty() || g.ffprobe.as_os_str().is_empty() {
        return Err(invalid("ffmpeg / ffprobe 路径不能为空"));
    }
    let v = &g.verify;
    if v.av_offset_soft_ms > v.av_offset_hard_ms || v.end_soft_ms > v.end_hard_ms {
        return Err(invalid("软性阈值不能大于硬性阈值"));
    }
    if v.ssim_hard > v.ssim_soft {
        return Err(invalid("SSIM 硬性阈值不能高于软性阈值"));
    }
    Ok(())
}

impl App {
    /// Tells listeners (SSE clients, the scheduler, the monitor) that the
    /// configuration changed.
    pub fn config_changed(&self, section: &str) {
        self.events.config_changed(section);
        self.config_version.send_modify(|v| *v += 1);
        self.scheduler.wake();
    }

    pub fn settings(&self) -> AppResult<Settings> {
        Ok(Settings {
            global: self.db.global_settings()?,
            server: self.db.server_settings()?,
            observed: self.db.observed_count()?,
            risk_acknowledged: self.db.risk_acknowledged()?.is_some(),
        })
    }

    pub fn update_settings(&self, u: &SettingsUpdate) -> AppResult<Settings> {
        if let Some(g) = &u.global {
            check_global(g)?;
        }
        if let Some(s) = &u.server {
            if s.gpu_slots > 8 || s.cpu_slots > 8 {
                return Err(invalid("每类配额最多 8 个"));
            }
            if s.scan_interval_mins < 5 {
                return Err(invalid("全量扫描间隔至少 5 分钟"));
            }
            if s.min_free_gib > 10_240 {
                return Err(invalid("保留空间最多 10240 GiB"));
            }
            if self.db.profile(&s.default_profile)?.is_none() {
                return Err(invalid(format!("默认方案 {} 不存在", s.default_profile)));
            }
        }
        if let Some(g) = &u.global {
            self.db.set_global_settings(g)?;
        }
        if let Some(s) = &u.server {
            self.db.set_server_settings(s)?;
        }
        self.config_changed("settings");
        self.settings()
    }

    /// Starts the observation mode over.
    pub fn reset_observation(&self) -> AppResult<Settings> {
        self.db.set_observed_count(0)?;
        self.config_changed("settings");
        self.settings()
    }

    /// Records that the user confirmed the risk notice, with this version.
    pub fn acknowledge_risk(&self) -> AppResult<Settings> {
        self.db.set_risk_acknowledged(env!("CARGO_PKG_VERSION"))?;
        self.config_changed("settings");
        self.settings()
    }

    pub fn profile(&self, id: &str) -> AppResult<SavedProfile> {
        self.db
            .profile(id)?
            .ok_or_else(|| AppError::not_found(&format!("配置方案 {id}")))
    }

    pub fn create_profile(&self, p: &SavedProfile) -> AppResult<SavedProfile> {
        check_profile(p)?;
        if self.db.profile(&p.id)?.is_some() {
            return Err(AppError::conflict(
                "profile_exists",
                format!("配置方案 {} 已存在", p.id),
            ));
        }
        let saved = self.db.put_profile(p)?;
        self.config_changed("profiles");
        Ok(saved)
    }

    pub fn update_profile(
        &self,
        id: &str,
        name: &str,
        profile: &Profile,
    ) -> AppResult<SavedProfile> {
        self.profile(id)?;
        let p = SavedProfile {
            id: id.into(),
            name: name.into(),
            profile: profile.clone(),
        };
        check_profile(&p)?;
        let saved = self.db.put_profile(&p)?;
        self.config_changed("profiles");
        Ok(saved)
    }

    pub fn delete_profile(&self, id: &str) -> AppResult<()> {
        self.profile(id)?;
        if self.db.server_settings()?.default_profile == id {
            return Err(AppError::conflict(
                "profile_in_use",
                "这是默认方案，不能删除",
            ));
        }
        let monitors = self.db.monitors_using(id)?;
        if !monitors.is_empty() {
            return Err(AppError::conflict(
                "profile_in_use",
                format!("有 {} 个监控目录在用这个方案", monitors.len()),
            )
            .with_details(serde_json::json!({ "monitors": monitors })));
        }
        self.db.delete_profile(id)?;
        self.config_changed("profiles");
        Ok(())
    }

    pub fn monitor(&self, id: i64) -> AppResult<MonitorDir> {
        self.db
            .monitor(id)?
            .ok_or_else(|| AppError::not_found(&format!("监控目录 #{id}")))
    }

    fn checked_monitor(&self, id: i64, m: &MonitorInput) -> AppResult<MonitorDir> {
        let path = scan::canonical(&m.path).map_err(|e| {
            AppError::bad_request(
                "bad_path",
                format!("目录 {} 无法访问：{e}", m.path.display()),
            )
        })?;
        if !path.is_dir() {
            return Err(AppError::bad_request(
                "bad_path",
                format!("{} 不是目录", path.display()),
            ));
        }
        Excludes::new(&m.excludes).map_err(|e| invalid(format!("排除规则无效：{e}")))?;
        let profile_id = match &m.profile_id {
            Some(p) => p.clone(),
            None => self.db.server_settings()?.default_profile,
        };
        self.profile(&profile_id)?;
        if let Some(other) = self
            .db
            .monitors()?
            .into_iter()
            .find(|o| o.path == path && o.id != id)
        {
            return Err(AppError::conflict(
                "monitor_exists",
                format!("这个目录已在监控中（#{}）", other.id),
            ));
        }
        Ok(MonitorDir {
            id,
            path,
            enabled: m.enabled,
            profile_id,
            recursive: m.recursive,
            excludes: m.excludes.clone(),
        })
    }

    pub fn create_monitor(&self, m: &MonitorInput) -> AppResult<MonitorDir> {
        let dir = self.checked_monitor(0, m)?;
        let saved = self.db.put_monitor(&dir)?;
        self.config_changed("monitors");
        Ok(saved)
    }

    pub fn update_monitor(&self, id: i64, m: &MonitorInput) -> AppResult<MonitorDir> {
        self.monitor(id)?;
        let dir = self.checked_monitor(id, m)?;
        let saved = self.db.put_monitor(&dir)?;
        self.config_changed("monitors");
        Ok(saved)
    }

    pub fn delete_monitor(&self, id: i64) -> AppResult<()> {
        self.monitor(id)?;
        self.db.delete_monitor(id)?;
        self.config_changed("monitors");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_ids_fit_in_the_marker() {
        assert!(valid_profile_id("movies-1080p_hevc"));
        assert!(!valid_profile_id(""));
        assert!(!valid_profile_id("a;b"));
        assert!(!valid_profile_id("a=b"));
        assert!(!valid_profile_id("电影"));
        assert!(!valid_profile_id(&"x".repeat(41)));
    }

    #[test]
    fn global_settings_are_checked() {
        assert!(check_global(&GlobalSettings::default()).is_ok());
        let mut g = GlobalSettings {
            min_savings_pct: 100.0,
            ..Default::default()
        };
        assert!(check_global(&g).is_err());
        g.min_savings_pct = 5.0;
        g.verify.ssim_hard = 0.95;
        assert!(check_global(&g).is_err());
        g.verify.ssim_hard = 0.8;
        g.pending_retention_days = 0;
        assert!(check_global(&g).is_err());
    }
}
