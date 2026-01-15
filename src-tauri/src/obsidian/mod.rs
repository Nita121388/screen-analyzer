// Obsidian 导出模块 - 生成 Markdown 文件

use anyhow::{anyhow, Result};
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;

use crate::actors::LLMHandle;
use crate::domains::summary::SummaryGenerator;
use crate::llm::plugin::ActivityCategory;
use crate::models::{ActivityTag, ObsidianExportConfig, ObsidianExportMode};
use crate::storage::{Database, Frame, Session, TimelineCardRecord};

/// Obsidian 导出器
pub struct ObsidianExporter {
    config: ObsidianExportConfig,
}

/// 导出结果摘要
pub struct ExportOutcome {
    pub daily_note_path: PathBuf,
    pub session_paths: Vec<PathBuf>,
    pub index_note_path: Option<PathBuf>,
    pub warnings: Vec<String>,
}

impl ExportOutcome {
    /// 渲染提示信息
    pub fn render_message(&self) -> String {
        let mut message = format!(
            "已导出每日总结: {}\n会话数量: {}",
            self.daily_note_path.to_string_lossy(),
            self.session_paths.len()
        );
        if let Some(path) = &self.index_note_path {
            message.push_str("\n索引文件: ");
            message.push_str(&path.to_string_lossy());
        }
        if !self.warnings.is_empty() {
            message.push_str("\n\n警告:\n");
            for warning in &self.warnings {
                message.push_str("- ");
                message.push_str(warning);
                message.push('\n');
            }
        }
        message
    }
}

impl ObsidianExporter {
    /// 创建新的导出器
    pub fn new(config: ObsidianExportConfig) -> Self {
        Self { config }
    }

    /// 导出指定日期的数据
    pub async fn export_day(
        &self,
        db: Arc<Database>,
        llm_handle: LLMHandle,
        date: &str,
        force_refresh: bool,
    ) -> Result<ExportOutcome> {
        let vault_root = PathBuf::from(self.config.vault_path.trim());
        if vault_root.as_os_str().is_empty() {
            return Err(anyhow!("未配置 Obsidian Vault 路径"));
        }
        if !vault_root.exists() {
            return Err(anyhow!("Obsidian Vault 路径不存在"));
        }

        let root = if self.config.root_folder.trim().is_empty() {
            vault_root
        } else {
            vault_root.join(self.config.root_folder.trim())
        };

        let daily_dir = root.join("Daily");
        let sessions_dir = root.join("Sessions").join(date);
        let assets_dir = root.join("Assets").join(date);

        fs::create_dir_all(&daily_dir).await?;
        fs::create_dir_all(&sessions_dir).await?;
        if self.config.include_screenshots {
            fs::create_dir_all(&assets_dir).await?;
        }

        let summary_generator = SummaryGenerator::with_llm(db.clone(), llm_handle);
        let day_summary = summary_generator
            .generate_day_summary(date, force_refresh)
            .await
            .map_err(|e| anyhow!(e))?;

        let sessions = db
            .get_sessions_by_date(date)
            .await
            .map_err(|e| anyhow!(e))?;

        let mut warnings = Vec::new();
        let mut session_paths = Vec::new();
        let mut session_links = Vec::new();

        for session in sessions {
            match self
                .export_session(&db, &session, &sessions_dir, &assets_dir)
                .await
            {
                Ok((session_path, link)) => {
                    session_paths.push(session_path);
                    session_links.push(link);
                }
                Err(e) => {
                    warnings.push(format!("会话 {} 导出失败: {}", session.id.unwrap_or(0), e));
                }
            }
        }

        let daily_note_path = daily_dir.join(format!("{}.md", sanitize_filename(date)));
        let daily_content = self.render_daily_note(&day_summary, &session_links);
        fs::write(&daily_note_path, daily_content).await?;

        let index_note_path = match self.export_month_index(db.as_ref(), date, &root).await {
            Ok(path) => Some(path),
            Err(err) => {
                warnings.push(format!("索引生成失败: {}", err));
                None
            }
        };

        Ok(ExportOutcome {
            daily_note_path,
            session_paths,
            index_note_path,
            warnings,
        })
    }

    async fn export_session(
        &self,
        db: &Arc<Database>,
        session: &Session,
        sessions_dir: &Path,
        assets_dir: &Path,
    ) -> Result<(PathBuf, String)> {
        let session_id = session.id.unwrap_or(0);
        let start_time = format_time(session.start_time);
        let end_time = format_time(session.end_time);
        let duration_minutes = (session.end_time - session.start_time).num_minutes().max(0);

        let filename = format!(
            "{}_{}-{}_session-{}.md",
            sanitize_filename(&session.start_time.format("%Y-%m-%d").to_string()),
            sanitize_filename(&session.start_time.format("%H%M").to_string()),
            sanitize_filename(&session.end_time.format("%H%M").to_string()),
            session_id
        );

        let session_path = sessions_dir.join(filename);
        let link = format!(
            "Sessions/{}/{}",
            session.start_time.format("%Y-%m-%d"),
            session_path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("session.md")
        );

        let tags = parse_tags(&session.tags);
        let tags_text = format_tags(&tags);

        let timeline_cards = db
            .get_timeline_cards_by_session(session_id)
            .await
            .unwrap_or_default();

        let timeline_text = render_timeline(&timeline_cards);

        let video_link = if self.config.include_video_link {
            session
                .video_path
                .as_ref()
                .map(|path| format_markdown_link("回放视频", &to_file_url(path)))
                .unwrap_or_else(|| "暂无视频".to_string())
        } else {
            String::new()
        };

        let screenshots_section = if self.config.include_screenshots {
            self.render_screenshots(db, session_id, assets_dir).await
        } else {
            String::new()
        };

        let content = self.render_session_note(
            session,
            &start_time,
            &end_time,
            duration_minutes,
            &tags_text,
            &timeline_text,
            &video_link,
            &screenshots_section,
        );

        fs::write(&session_path, content).await?;

        Ok((session_path, format!("[[{}]]", link)))
    }

    fn render_daily_note(
        &self,
        summary: &crate::domains::summary::DaySummary,
        session_links: &[String],
    ) -> String {
        let session_list = if session_links.is_empty() {
            "- 当天没有会话记录".to_string()
        } else {
            session_links
                .iter()
                .map(|link| format!("- {}", link))
                .collect::<Vec<_>>()
                .join("\n")
        };

        let usage_patterns = if summary.usage_patterns.is_empty() {
            "暂无统计".to_string()
        } else {
            summary
                .usage_patterns
                .iter()
                .map(|p| format!("- {}: {}", p.label, p.value))
                .collect::<Vec<_>>()
                .join("\n")
        };

        let device_stats = if summary.device_stats.is_empty() {
            "暂无设备统计".to_string()
        } else {
            summary
                .device_stats
                .iter()
                .map(|stat| {
                    format!(
                        "- {} ({})：{}，截图 {} 张",
                        stat.name, stat.device_type, stat.total_time, stat.screenshots
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        let default_template = format!(
            "---\n\
type: screen-analyzer-daily\n\
date: {date}\n\
session_count: {session_count}\n\
active_device_count: {device_count}\n\
source: screen-analyzer\n\
---\n\
\n\
# {date} 屏幕活动总结\n\
\n\
{summary}\n\
\n\
## 会话索引\n\
{session_list}\n\
\n\
## 使用模式\n\
{usage_patterns}\n\
\n\
## 设备统计\n\
{device_stats}\n",
            date = summary.date,
            session_count = session_links.len(),
            device_count = summary.active_device_count,
            summary = summary.summary_text,
            session_list = session_list,
            usage_patterns = usage_patterns,
            device_stats = device_stats
        );

        render_template(
            self.config
                .daily_template
                .as_deref()
                .filter(|t| !t.trim().is_empty()),
            &default_template,
            &[
                ("date", summary.date.clone()),
                ("summary", summary.summary_text.clone()),
                ("session_list", session_list),
                ("usage_patterns", usage_patterns),
                ("device_stats", device_stats),
                (
                    "active_device_count",
                    summary.active_device_count.to_string(),
                ),
            ],
        )
    }

    fn render_session_note(
        &self,
        session: &Session,
        start: &str,
        end: &str,
        duration_minutes: i64,
        tags: &str,
        timeline: &str,
        video_link: &str,
        screenshots_section: &str,
    ) -> String {
        let title = if session.title.trim().is_empty() {
            "未命名会话".to_string()
        } else {
            session.title.clone()
        };

        let summary_text = if session.summary.trim().is_empty() {
            "暂无总结".to_string()
        } else {
            session.summary.clone()
        };

        let video_block = if video_link.trim().is_empty() {
            String::new()
        } else {
            format!("\n## 视频\n{}\n", video_link)
        };

        let screenshots_block = if screenshots_section.trim().is_empty() {
            String::new()
        } else {
            format!("\n## 截图\n{}\n", screenshots_section)
        };

        let default_template = format!(
            "---\n\
type: screen-analyzer-session\n\
date: {date}\n\
session_id: {session_id}\n\
start: {start}\n\
end: {end}\n\
duration_minutes: {duration}\n\
tags: {tags}\n\
source: screen-analyzer\n\
---\n\
\n\
# {title}\n\
\n\
{summary}\n\
\n\
## 时间线\n\
{timeline}\n\
{video_block}\
{screenshots_block}",
            date = session.start_time.format("%Y-%m-%d"),
            session_id = session.id.unwrap_or(0),
            start = start,
            end = end,
            duration = duration_minutes,
            tags = tags,
            title = title,
            summary = summary_text,
            timeline = timeline,
            video_block = video_block,
            screenshots_block = screenshots_block
        );

        render_template(
            self.config
                .session_template
                .as_deref()
                .filter(|t| !t.trim().is_empty()),
            &default_template,
            &[
                ("date", session.start_time.format("%Y-%m-%d").to_string()),
                ("session_id", session.id.unwrap_or(0).to_string()),
                ("start", start.to_string()),
                ("end", end.to_string()),
                ("duration_minutes", duration_minutes.to_string()),
                ("title", title),
                ("summary", summary_text),
                ("tags", tags.to_string()),
                ("timeline", timeline.to_string()),
                ("video_link", video_link.to_string()),
                ("screenshots", screenshots_section.to_string()),
            ],
        )
    }

    async fn render_screenshots(
        &self,
        db: &Arc<Database>,
        session_id: i64,
        assets_dir: &Path,
    ) -> String {
        let frames = db.get_frames_by_session(session_id).await.unwrap_or_default();
        let targets = pick_screenshots(&frames);

        if targets.is_empty() {
            return "暂无可用截图".to_string();
        }

        let mut links = Vec::new();
        for (index, frame) in targets.iter().enumerate() {
            match self.prepare_screenshot(frame, assets_dir, session_id, index).await {
                Ok(link) => links.push(link),
                Err(err) => links.push(format!("截图处理失败: {}", err)),
            }
        }

        links.join("\n")
    }

    async fn prepare_screenshot(
        &self,
        frame: &Frame,
        assets_dir: &Path,
        session_id: i64,
        index: usize,
    ) -> Result<String> {
        let frame_path = PathBuf::from(&frame.file_path);
        if !frame_path.exists() {
            return Err(anyhow!("截图文件不存在"));
        }

        match self.config.export_mode {
            ObsidianExportMode::Copy => {
                let target_name = format!("session-{}-{}.jpg", session_id, index);
                let target_path = assets_dir.join(target_name);
                fs::copy(&frame_path, &target_path).await?;
                let relative = format!(
                    "Assets/{}/{}",
                    assets_dir
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or(""),
                    target_path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                );
                Ok(format!("![]({})", relative))
            }
            ObsidianExportMode::Link => {
                let file_url = to_file_url(&frame.file_path);
                Ok(format!("![]({})", file_url))
            }
        }
    }

    async fn export_month_index(
        &self,
        db: &Database,
        date: &str,
        root: &Path,
    ) -> Result<PathBuf> {
        let day = NaiveDate::parse_from_str(date, "%Y-%m-%d")
            .map_err(|_| anyhow!("日期格式错误: {}", date))?;
        let (year, month) = (day.year(), day.month());

        let month_start =
            NaiveDate::from_ymd_opt(year, month, 1).ok_or_else(|| anyhow!("月份无效"))?;
        let next_month = if month == 12 {
            NaiveDate::from_ymd_opt(year + 1, 1, 1)
        } else {
            NaiveDate::from_ymd_opt(year, month + 1, 1)
        }
        .ok_or_else(|| anyhow!("月份无效"))?;
        let month_end = next_month - chrono::Duration::days(1);

        let start_date = month_start.format("%Y-%m-%d").to_string();
        let end_date = month_end.format("%Y-%m-%d").to_string();

        let mut activities = db
            .get_activities(&start_date, &end_date)
            .await
            .map_err(|e| anyhow!(e))?;

        activities.sort_by(|a, b| a.date.cmp(&b.date));

        let total_sessions: i32 = activities.iter().map(|a| a.session_count).sum();
        let total_minutes: i32 = activities.iter().map(|a| a.total_duration_minutes).sum();

        let mut category_counts: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for activity in &activities {
            for category in &activity.main_categories {
                *category_counts.entry(category.clone()).or_insert(0) += 1;
            }
        }

        let mut categories: Vec<(String, usize)> = category_counts.into_iter().collect();
        categories.sort_by(|a, b| b.1.cmp(&a.1));
        let top_categories = if categories.is_empty() {
            "暂无".to_string()
        } else {
            categories
                .iter()
                .take(5)
                .map(|(name, count)| format!("{}({})", name, count))
                .collect::<Vec<_>>()
                .join("、")
        };

        let mut table_lines = Vec::new();
        table_lines.push("| 日期 | 会话数 | 总时长(分钟) | 主要类别 |".to_string());
        table_lines.push("| --- | --- | --- | --- |".to_string());

        if activities.is_empty() {
            table_lines.push("| - | 0 | 0 | - |".to_string());
        } else {
            for activity in &activities {
                let date_link = format!("[[Daily/{}]]", activity.date);
                let categories = if activity.main_categories.is_empty() {
                    "-".to_string()
                } else {
                    activity.main_categories.join(", ")
                };
                table_lines.push(format!(
                    "| {} | {} | {} | {} |",
                    date_link, activity.session_count, activity.total_duration_minutes, categories
                ));
            }
        }

        let content = format!(
            "---\n\
type: screen-analyzer-index\n\
month: {month}\n\
total_sessions: {sessions}\n\
total_minutes: {minutes}\n\
source: screen-analyzer\n\
---\n\
\n\
# {month} 月度索引\n\
\n\
## 概览\n\
- 会话总数：{sessions}\n\
- 总时长：{minutes} 分钟\n\
- 主要类别：{top_categories}\n\
\n\
## 每日明细\n\
{table}\n",
            month = format!("{:04}-{:02}", year, month),
            sessions = total_sessions,
            minutes = total_minutes,
            top_categories = top_categories,
            table = table_lines.join("\n")
        );

        let index_path = root.join("Index").join(format!(
            "sessions-{:04}-{:02}.md",
            year, month
        ));
        export_index_file(&index_path, content).await
    }
}

async fn export_index_file(path: &Path, content: String) -> Result<PathBuf> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(path, content).await?;
    Ok(path.to_path_buf())
}

fn format_time(dt: DateTime<Utc>) -> String {
    dt.format("%H:%M").to_string()
}

fn render_timeline(cards: &[TimelineCardRecord]) -> String {
    if cards.is_empty() {
        return "- 无可用时间线".to_string();
    }

    let mut lines = Vec::new();
    for card in cards {
        let (start, end) = format_time_range(&card.start_time, &card.end_time);
        let line = format!(
            "- {}-{} [{} / {}] {}：{}",
            start, end, card.category, card.subcategory, card.title, card.summary
        );
        lines.push(line);
    }
    lines.join("\n")
}

fn format_time_range(start: &str, end: &str) -> (String, String) {
    let format = |value: &str| -> String {
        chrono::DateTime::parse_from_rfc3339(value)
            .map(|dt| dt.format("%H:%M").to_string())
            .unwrap_or_else(|_| value.to_string())
    };
    (format(start), format(end))
}

fn parse_tags(raw: &str) -> Vec<ActivityTag> {
    serde_json::from_str::<Vec<ActivityTag>>(raw).unwrap_or_default()
}

fn format_tags(tags: &[ActivityTag]) -> String {
    if tags.is_empty() {
        return "[]".to_string();
    }

    let values = tags
        .iter()
        .map(|tag| category_to_string(&tag.category))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{}]", values)
}

fn category_to_string(category: &ActivityCategory) -> &'static str {
    match category {
        ActivityCategory::Work => "work",
        ActivityCategory::Communication => "communication",
        ActivityCategory::Learning => "learning",
        ActivityCategory::Personal => "personal",
        ActivityCategory::Idle => "idle",
        ActivityCategory::Other => "other",
    }
}

fn pick_screenshots(frames: &[Frame]) -> Vec<Frame> {
    if frames.is_empty() {
        return Vec::new();
    }
    if frames.len() == 1 {
        return vec![frames[0].clone()];
    }

    let first = frames.first().cloned();
    let last = frames.last().cloned();

    let mut selected = Vec::new();
    if let Some(frame) = first {
        selected.push(frame);
    }
    if let Some(frame) = last {
        if selected
            .last()
            .map(|f| f.file_path != frame.file_path)
            .unwrap_or(true)
        {
            selected.push(frame);
        }
    }
    selected
}

fn sanitize_filename(raw: &str) -> String {
    raw.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => c,
        })
        .collect()
}

fn to_file_url(path: &str) -> String {
    let normalized = path.replace('\\', "/").replace(' ', "%20");
    if normalized.contains(":/") {
        format!("file:///{}", normalized)
    } else {
        format!("file://{}", normalized)
    }
}

fn format_markdown_link(label: &str, url: &str) -> String {
    format!("[{}]({})", label, url)
}

fn render_template(template: Option<&str>, fallback: &str, values: &[(&str, String)]) -> String {
    let mut content = template.unwrap_or(fallback).to_string();
    for (key, value) in values {
        let placeholder = format!("{{{{{}}}}}", key);
        content = content.replace(&placeholder, value);
    }
    content
}
