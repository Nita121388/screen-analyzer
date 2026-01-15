// 配置迁移模块 - 负责配置导出/导入

use serde::{Deserialize, Serialize};

use crate::models::{
    AppConfig, CaptureSettings, DatabaseConfig, LoggerSettings, NotionConfig, ObsidianExportConfig,
    PersistedAppConfig, UISettings,
};

/// 配置导出包
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigExportPackage {
    pub version: u32,
    pub exported_at: String,
    pub include_secrets: bool,
    pub app_config: PersistedAppConfig,
}

/// 移除敏感信息
pub fn strip_secrets(config: &mut PersistedAppConfig) {
    if let Some(llm_config) = config.llm_config.as_mut() {
        llm_config.api_key.clear();
        llm_config.auth_token.clear();
    }

    if let Some(notion_config) = config.notion_config.as_mut() {
        notion_config.api_token.clear();
    }

    if let Some(database_config) = config.database_config.as_mut() {
        if let DatabaseConfig::MariaDB { password, .. } = database_config {
            password.clear();
        }
    }
}

/// 规范化导入配置（补齐可选字段的默认值）
pub fn normalize_imported_config(mut config: PersistedAppConfig) -> PersistedAppConfig {
    if config.ui_settings.is_none() {
        config.ui_settings = Some(UISettings::default());
    }

    if config.capture_settings.is_none() {
        config.capture_settings = Some(CaptureSettings::default());
    }

    if config.logger_settings.is_none() {
        config.logger_settings = Some(LoggerSettings::default());
    }

    if config.notion_config.is_none() {
        config.notion_config = Some(NotionConfig::default());
    }

    if config.obsidian_config.is_none() {
        config.obsidian_config = Some(ObsidianExportConfig::default());
    }

    config
}

/// 将持久化配置转换为 AppConfig（用于应用更新逻辑）
pub fn persisted_to_app_config(config: PersistedAppConfig) -> AppConfig {
    AppConfig {
        retention_days: Some(config.retention_days),
        llm_provider: Some(config.llm_provider),
        capture_interval: Some(config.capture_interval),
        summary_interval: Some(config.summary_interval),
        video_config: Some(config.video_config),
        ui_settings: config.ui_settings,
        llm_config: config.llm_config,
        capture_settings: config.capture_settings,
        logger_settings: config.logger_settings,
        database_config: config.database_config,
        notion_config: config.notion_config,
        obsidian_config: config.obsidian_config,
    }
}
