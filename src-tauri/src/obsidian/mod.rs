// Obsidian 导出模块 - 生成 Markdown 文件

use anyhow::{anyhow, Result};
use chrono::{DateTime, Datelike, NaiveDate, Utc, Weekday};
use serde::Serialize;
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
    pub week_index_path: Option<PathBuf>,
    pub weekly_note_path: Option<PathBuf>,
    pub overview_path: Option<PathBuf>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct WeekSummaryPreview {
    pub week_label: String,
    pub week_start: String,
    pub week_end: String,
    pub total_sessions: i32,
    pub total_minutes: i32,
    pub avg_session_minutes: i32,
    pub top_categories: String,
    pub focus_minutes: i64,
    pub distraction_minutes: i64,
    pub focus_ratio: i64,
    pub distraction_ratio: i64,
    pub focus_score: i64,
    pub effort_score: i64,
    pub productivity_score: i64,
    pub focus_weight: i64,
    pub effort_weight: i64,
    pub target_minutes: i64,
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
        if let Some(path) = &self.week_index_path {
            message.push_str("\n周索引文件: ");
            message.push_str(&path.to_string_lossy());
        }
        if let Some(path) = &self.weekly_note_path {
            message.push_str("\n周报文件: ");
            message.push_str(&path.to_string_lossy());
        }
        if let Some(path) = &self.overview_path {
            message.push_str("\n总览文件: ");
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

        let mut week_summary: Option<WeekSummaryData> = None;
        let (week_index_path, weekly_note_path) =
            match self.build_week_summary(db.as_ref(), date, &self.config).await {
                Ok(summary) => {
                    week_summary = Some(summary);
                    let summary_ref = week_summary.as_ref().expect("周报摘要缺失");
                    let index_path =
                        match self.export_week_index_with_summary(summary_ref, &root).await {
                            Ok(path) => Some(path),
                            Err(err) => {
                                warnings.push(format!("周索引生成失败: {}", err));
                                None
                            }
                        };
                    let weekly_note_path =
                        match self.export_weekly_note_with_summary(summary_ref, &root).await {
                            Ok(path) => Some(path),
                            Err(err) => {
                                warnings.push(format!("周报生成失败: {}", err));
                                None
                            }
                        };
                    (index_path, weekly_note_path)
                }
                Err(err) => {
                    warnings.push(format!("周报数据生成失败: {}", err));
                    (None, None)
                }
            };

        let overview_path = match self
            .export_overview_index(date, week_summary.as_ref(), &root)
            .await
        {
            Ok(path) => Some(path),
            Err(err) => {
                warnings.push(format!("总览生成失败: {}", err));
                None
            }
        };

        Ok(ExportOutcome {
            daily_note_path,
            session_paths,
            index_note_path,
            week_index_path,
            weekly_note_path,
            overview_path,
            warnings,
        })
    }

    pub async fn preview_week_summary(
        &self,
        db: &Database,
        date: &str,
    ) -> Result<WeekSummaryPreview> {
        let summary = self.build_week_summary(db, date, &self.config).await?;
        let focus_minutes = summary.focus_metrics.focus_minutes();
        let distraction_minutes = summary.focus_metrics.distraction_minutes();
        let focus_ratio = summary.focus_metrics.focus_ratio();
        let distraction_ratio = summary.focus_metrics.distraction_ratio();
        let focus_score = summary.focus_metrics.focus_score();
        let effort_score = summary
            .focus_metrics
            .effort_score(summary.score_config.target_minutes);
        let productivity_score = summary.focus_metrics.productivity_score(
            summary.score_config.focus_weight,
            summary.score_config.effort_weight,
            summary.score_config.target_minutes,
        );

        Ok(WeekSummaryPreview {
            week_label: summary.week_label,
            week_start: summary.week_start,
            week_end: summary.week_end,
            total_sessions: summary.total_sessions,
            total_minutes: summary.total_minutes,
            avg_session_minutes: summary.avg_session_minutes,
            top_categories: summary.top_categories,
            focus_minutes,
            distraction_minutes,
            focus_ratio,
            distraction_ratio,
            focus_score,
            effort_score,
            productivity_score,
            focus_weight: summary.score_config.focus_weight,
            effort_weight: summary.score_config.effort_weight,
            target_minutes: summary.score_config.target_minutes,
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
        let metrics = build_session_metrics(&timeline_cards, duration_minutes);
        let metrics_text = render_metrics(&metrics);

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
            &metrics_text,
            &metrics,
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
        metrics_text: &str,
        metrics: &SessionMetrics,
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
timeline_cards: {timeline_cards}\n\
context_switches: {context_switches}\n\
fragmentation_level: {fragmentation_level}\n\
tags: {tags}\n\
source: screen-analyzer\n\
---\n\
\n\
# {title}\n\
\n\
{summary}\n\
\n\
## 指标\n\
{metrics}\n\
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
            timeline_cards = metrics.timeline_cards,
            context_switches = metrics.context_switches,
            fragmentation_level = metrics.fragmentation_level,
            tags = tags,
            title = title,
            summary = summary_text,
            metrics = metrics_text,
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
                ("metrics", metrics_text.to_string()),
                ("context_switches", metrics.context_switches.to_string()),
                ("fragmentation_level", metrics.fragmentation_level.to_string()),
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
        let avg_session_minutes = if total_sessions > 0 {
            total_minutes / total_sessions
        } else {
            0
        };

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
avg_session_minutes: {avg_session}\n\
source: screen-analyzer\n\
---\n\
\n\
# {month} 月度索引\n\
\n\
## 概览\n\
- 会话总数：{sessions}\n\
- 总时长：{minutes} 分钟\n\
- 平均会话时长：{avg_session} 分钟\n\
- 主要类别：{top_categories}\n\
\n\
## 每日明细\n\
{table}\n",
            month = format!("{:04}-{:02}", year, month),
            sessions = total_sessions,
            minutes = total_minutes,
            avg_session = avg_session_minutes,
            top_categories = top_categories,
            table = table_lines.join("\n")
        );

        let index_path = root.join("Index").join(format!(
            "sessions-{:04}-{:02}.md",
            year, month
        ));
        export_index_file(&index_path, content).await
    }

    async fn export_week_index_with_summary(
        &self,
        summary: &WeekSummaryData,
        root: &Path,
    ) -> Result<PathBuf> {
        let focus_summary =
            render_week_focus_metrics(&summary.focus_metrics, &summary.score_config);
        let focus_minutes = summary.focus_metrics.focus_minutes();
        let distraction_minutes = summary.focus_metrics.distraction_minutes();
        let focus_ratio = summary.focus_metrics.focus_ratio();
        let distraction_ratio = summary.focus_metrics.distraction_ratio();
        let focus_score = summary.focus_metrics.focus_score();
        let effort_score =
            summary
                .focus_metrics
                .effort_score(summary.score_config.target_minutes);
        let productivity_score = summary.focus_metrics.productivity_score(
            summary.score_config.focus_weight,
            summary.score_config.effort_weight,
            summary.score_config.target_minutes,
        );

        let content = format!(
            "---\n\
type: screen-analyzer-week-index\n\
week: {week}\n\
week_start: {week_start}\n\
week_end: {week_end}\n\
total_sessions: {sessions}\n\
total_minutes: {minutes}\n\
avg_session_minutes: {avg_session}\n\
focus_minutes: {focus_minutes}\n\
focus_ratio: {focus_ratio}\n\
distraction_minutes: {distraction_minutes}\n\
distraction_ratio: {distraction_ratio}\n\
communication_minutes: {communication_minutes}\n\
focus_score: {focus_score}\n\
effort_score: {effort_score}\n\
productivity_score: {productivity_score}\n\
focus_weight: {focus_weight}\n\
effort_weight: {effort_weight}\n\
target_minutes: {target_minutes}\n\
source: screen-analyzer\n\
---\n\
\n\
# {week} 周度索引\n\
\n\
## 概览\n\
- 会话总数：{sessions}\n\
- 总时长：{minutes} 分钟\n\
- 平均会话时长：{avg_session} 分钟\n\
- 主要类别：{top_categories}\n\
\n\
## 专注度\n\
{focus_summary}\n\
\n\
## 每日明细\n\
{table}\n",
            week = summary.week_label,
            week_start = summary.week_start,
            week_end = summary.week_end,
            sessions = summary.total_sessions,
            minutes = summary.total_minutes,
            avg_session = summary.avg_session_minutes,
            focus_minutes = focus_minutes,
            focus_ratio = focus_ratio,
            distraction_minutes = distraction_minutes,
            distraction_ratio = distraction_ratio,
            communication_minutes = summary.focus_metrics.communication_minutes,
            focus_score = focus_score,
            effort_score = effort_score,
            productivity_score = productivity_score,
            focus_weight = summary.score_config.focus_weight,
            effort_weight = summary.score_config.effort_weight,
            target_minutes = summary.score_config.target_minutes,
            top_categories = summary.top_categories,
            focus_summary = focus_summary,
            table = summary.table_lines.join("\n")
        );

        let index_path = root
            .join("Index")
            .join(format!("weeks-{}.md", summary.week_label));
        export_index_file(&index_path, content).await
    }

    async fn export_weekly_note_with_summary(
        &self,
        summary: &WeekSummaryData,
        root: &Path,
    ) -> Result<PathBuf> {
        let weekly_dir = root.join("Weekly");
        fs::create_dir_all(&weekly_dir).await?;

        let weekly_path = weekly_dir.join(format!("{}.md", summary.week_label));
        let content = self.render_weekly_note(summary);
        fs::write(&weekly_path, content).await?;
        Ok(weekly_path)
    }

    fn render_weekly_note(&self, summary: &WeekSummaryData) -> String {
        let focus_summary =
            render_week_focus_metrics(&summary.focus_metrics, &summary.score_config);
        let focus_minutes = summary.focus_metrics.focus_minutes();
        let distraction_minutes = summary.focus_metrics.distraction_minutes();
        let focus_ratio = summary.focus_metrics.focus_ratio();
        let distraction_ratio = summary.focus_metrics.distraction_ratio();
        let focus_score = summary.focus_metrics.focus_score();
        let effort_score =
            summary
                .focus_metrics
                .effort_score(summary.score_config.target_minutes);
        let productivity_score = summary.focus_metrics.productivity_score(
            summary.score_config.focus_weight,
            summary.score_config.effort_weight,
            summary.score_config.target_minutes,
        );
        let highlights = if summary.daily_highlights.is_empty() {
            "- 暂无每日总结".to_string()
        } else {
            summary.daily_highlights.join("\n")
        };
        let insights = build_week_insights(summary);
        let insight_text = if insights.is_empty() {
            "- 暂无摘要".to_string()
        } else {
            insights
                .into_iter()
                .map(|item| format!("- {}", item))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let week_index_link = format!("[[Index/weeks-{}.md]]", summary.week_label);

        format!(
            "---\n\
type: screen-analyzer-weekly\n\
week: {week}\n\
week_start: {week_start}\n\
week_end: {week_end}\n\
total_sessions: {sessions}\n\
total_minutes: {minutes}\n\
avg_session_minutes: {avg_session}\n\
focus_minutes: {focus_minutes}\n\
focus_ratio: {focus_ratio}\n\
distraction_minutes: {distraction_minutes}\n\
distraction_ratio: {distraction_ratio}\n\
communication_minutes: {communication_minutes}\n\
focus_score: {focus_score}\n\
effort_score: {effort_score}\n\
productivity_score: {productivity_score}\n\
focus_weight: {focus_weight}\n\
effort_weight: {effort_weight}\n\
target_minutes: {target_minutes}\n\
source: screen-analyzer\n\
---\n\
\n\
# {week} 周报\n\
\n\
## 概览\n\
- 会话总数：{sessions}\n\
- 总时长：{minutes} 分钟\n\
- 平均会话时长：{avg_session} 分钟\n\
- 主要类别：{top_categories}\n\
\n\
## 专注度\n\
{focus_summary}\n\
\n\
## 周报摘要\n\
{insight_text}\n\
\n\
## 评分说明\n\
- 专注评分 = 专注占比\n\
- 投入时长评分：以 {target_minutes} 分钟为 100 分，上限封顶\n\
- 生产力评分 = 专注评分 {focus_weight}% + 投入时长评分 {effort_weight}%\n\
\n\
## 每日要点\n\
{highlights}\n\
\n\
## 周索引\n\
- {week_index_link}\n",
            week = summary.week_label,
            week_start = summary.week_start,
            week_end = summary.week_end,
            sessions = summary.total_sessions,
            minutes = summary.total_minutes,
            avg_session = summary.avg_session_minutes,
            focus_minutes = focus_minutes,
            focus_ratio = focus_ratio,
            distraction_minutes = distraction_minutes,
            distraction_ratio = distraction_ratio,
            communication_minutes = summary.focus_metrics.communication_minutes,
            focus_score = focus_score,
            effort_score = effort_score,
            productivity_score = productivity_score,
            focus_weight = summary.score_config.focus_weight,
            effort_weight = summary.score_config.effort_weight,
            target_minutes = summary.score_config.target_minutes,
            top_categories = summary.top_categories,
            focus_summary = focus_summary,
            insight_text = insight_text,
            highlights = highlights,
            week_index_link = week_index_link
        )
    }

    async fn build_week_summary(
        &self,
        db: &Database,
        date: &str,
        config: &ObsidianExportConfig,
    ) -> Result<WeekSummaryData> {
        let day = NaiveDate::parse_from_str(date, "%Y-%m-%d")
            .map_err(|_| anyhow!("日期格式错误: {}", date))?;
        let iso_week = day.iso_week();
        let week_year = iso_week.year();
        let week_number = iso_week.week();

        let week_start = NaiveDate::from_isoywd_opt(week_year, week_number, Weekday::Mon)
            .ok_or_else(|| anyhow!("周起始日期无效"))?;
        let week_end = NaiveDate::from_isoywd_opt(week_year, week_number, Weekday::Sun)
            .ok_or_else(|| anyhow!("周结束日期无效"))?;

        let start_date = week_start.format("%Y-%m-%d").to_string();
        let end_date = week_end.format("%Y-%m-%d").to_string();

        let mut activities = db
            .get_activities(&start_date, &end_date)
            .await
            .map_err(|e| anyhow!(e))?;

        activities.sort_by(|a, b| a.date.cmp(&b.date));

        let total_sessions: i32 = activities.iter().map(|a| a.session_count).sum();
        let total_minutes: i32 = activities.iter().map(|a| a.total_duration_minutes).sum();
        let avg_session_minutes = if total_sessions > 0 {
            total_minutes / total_sessions
        } else {
            0
        };

        let focus_metrics = self
            .compute_week_focus_metrics(db, week_start, week_end)
            .await;

        let focus_weight = i64::from(config.weekly_focus_weight.min(100));
        let effort_weight = 100 - focus_weight;
        let target_minutes = config.weekly_target_minutes.max(1);
        let score_config = WeekScoreConfig {
            focus_weight,
            effort_weight,
            target_minutes,
        };

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

        let mut daily_highlights = Vec::new();
        let mut cursor = week_start;
        while cursor <= week_end {
            let date_text = cursor.format("%Y-%m-%d").to_string();
            let link = format!("[[Daily/{}]]", date_text);
            let summary_text = match db.get_day_summary(&date_text).await {
                Ok(Some(summary)) => compact_summary_text(&summary.summary_text, 140),
                _ => "暂无总结".to_string(),
            };
            daily_highlights.push(format!("- {}: {}", link, summary_text));
            cursor += chrono::Duration::days(1);
        }

        Ok(WeekSummaryData {
            week_label: format!("{:04}-W{:02}", week_year, week_number),
            week_start: start_date,
            week_end: end_date,
            total_sessions,
            total_minutes,
            avg_session_minutes,
            top_categories,
            table_lines,
            focus_metrics,
            score_config,
            daily_highlights,
        })
    }

    async fn export_overview_index(
        &self,
        date: &str,
        week_summary: Option<&WeekSummaryData>,
        root: &Path,
    ) -> Result<PathBuf> {
        let day = NaiveDate::parse_from_str(date, "%Y-%m-%d")
            .map_err(|_| anyhow!("日期格式错误: {}", date))?;
        let month_label = format!("{:04}-{:02}", day.year(), day.month());
        let updated_at = crate::storage::local_now().format("%Y-%m-%d %H:%M").to_string();

        let daily_link = format!("[[Daily/{}]]", date);
        let week_link = week_summary
            .map(|summary| format!("[[Weekly/{}]]", summary.week_label))
            .unwrap_or_else(|| "暂无".to_string());
        let week_index_link = week_summary
            .map(|summary| format!("[[Index/weeks-{}.md]]", summary.week_label))
            .unwrap_or_else(|| "暂无".to_string());
        let month_index_link = format!("[[Index/sessions-{}.md]]", month_label);

        let content = format!(
            "---\n\
type: screen-analyzer-overview\n\
updated_at: {updated_at}\n\
source: screen-analyzer\n\
---\n\
\n\
# Screen Analyzer 总览\n\
\n\
- 今日：{daily_link}\n\
- 本周：{week_link}\n\
- 本周索引：{week_index_link}\n\
- 本月索引：{month_index_link}\n",
            updated_at = updated_at,
            daily_link = daily_link,
            week_link = week_link,
            week_index_link = week_index_link,
            month_index_link = month_index_link
        );

        let index_path = root.join("Index").join("overview.md");
        export_index_file(&index_path, content).await
    }

    async fn compute_week_focus_metrics(
        &self,
        db: &Database,
        week_start: NaiveDate,
        week_end: NaiveDate,
    ) -> WeekFocusMetrics {
        let mut metrics = WeekFocusMetrics::default();
        let mut cursor = week_start;

        while cursor <= week_end {
            let date = cursor.format("%Y-%m-%d").to_string();
            if let Ok(sessions) = db.get_sessions_by_date(&date).await {
                for session in sessions {
                    let session_id = match session.id {
                        Some(id) => id,
                        None => continue,
                    };
                    if let Ok(cards) = db.get_timeline_cards_by_session(session_id).await {
                        metrics.add_cards(&cards);
                    }
                }
            }
            cursor += chrono::Duration::days(1);
        }

        metrics
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

/// 会话指标
struct SessionMetrics {
    timeline_cards: usize,
    context_switches: usize,
    avg_segment_minutes: i64,
    fragmentation_level: String,
}

#[derive(Default)]
struct WeekFocusMetrics {
    total_minutes: i64,
    work_minutes: i64,
    learning_minutes: i64,
    communication_minutes: i64,
    personal_minutes: i64,
    idle_minutes: i64,
    other_minutes: i64,
}

struct WeekSummaryData {
    week_label: String,
    week_start: String,
    week_end: String,
    total_sessions: i32,
    total_minutes: i32,
    avg_session_minutes: i32,
    top_categories: String,
    table_lines: Vec<String>,
    focus_metrics: WeekFocusMetrics,
    score_config: WeekScoreConfig,
    daily_highlights: Vec<String>,
}

struct WeekScoreConfig {
    focus_weight: i64,
    effort_weight: i64,
    target_minutes: i64,
}

impl WeekFocusMetrics {
    fn add_cards(&mut self, cards: &[TimelineCardRecord]) {
        for card in cards {
            self.add_card(card);
        }
    }

    fn add_card(&mut self, card: &TimelineCardRecord) {
        let minutes = parse_card_minutes(card);
        if minutes <= 0 {
            return;
        }
        self.total_minutes += minutes;

        match normalize_timeline_category(&card.category) {
            ActivityCategory::Work => self.work_minutes += minutes,
            ActivityCategory::Learning => self.learning_minutes += minutes,
            ActivityCategory::Communication => self.communication_minutes += minutes,
            ActivityCategory::Personal => self.personal_minutes += minutes,
            ActivityCategory::Idle => self.idle_minutes += minutes,
            ActivityCategory::Other => self.other_minutes += minutes,
        }
    }

    fn focus_minutes(&self) -> i64 {
        self.work_minutes + self.learning_minutes
    }

    fn distraction_minutes(&self) -> i64 {
        self.personal_minutes + self.idle_minutes + self.other_minutes
    }

    fn focus_ratio(&self) -> i64 {
        if self.total_minutes == 0 {
            0
        } else {
            (self.focus_minutes() * 100 / self.total_minutes).max(0)
        }
    }

    fn distraction_ratio(&self) -> i64 {
        if self.total_minutes == 0 {
            0
        } else {
            (self.distraction_minutes() * 100 / self.total_minutes).max(0)
        }
    }

    fn effort_score(&self, target_minutes: i64) -> i64 {
        if self.total_minutes == 0 || target_minutes <= 0 {
            return 0;
        }
        let score = self.total_minutes * 100 / target_minutes;
        score.min(100).max(0)
    }

    fn focus_score(&self) -> i64 {
        self.focus_ratio()
    }

    fn productivity_score(
        &self,
        focus_weight: i64,
        effort_weight: i64,
        target_minutes: i64,
    ) -> i64 {
        let total_weight = (focus_weight + effort_weight).max(1);
        let focus_score = self.focus_score();
        let effort_score = self.effort_score(target_minutes);
        (focus_score * focus_weight + effort_score * effort_weight) / total_weight
    }
}

fn build_session_metrics(cards: &[TimelineCardRecord], duration_minutes: i64) -> SessionMetrics {
    let timeline_cards = cards.len();
    let context_switches = count_context_switches(cards);
    let avg_segment_minutes = if timeline_cards == 0 {
        0
    } else {
        (duration_minutes / timeline_cards as i64).max(0)
    };
    let fragmentation_level = match context_switches {
        0..=1 => "低",
        2..=3 => "中",
        _ => "高",
    }
    .to_string();

    SessionMetrics {
        timeline_cards,
        context_switches,
        avg_segment_minutes,
        fragmentation_level,
    }
}

fn render_metrics(metrics: &SessionMetrics) -> String {
    if metrics.timeline_cards == 0 {
        return "暂无指标".to_string();
    }

    format!(
        "- 片段数量: {}\n- 上下文切换: {}\n- 平均片段时长: {} 分钟\n- 碎片化等级: {}",
        metrics.timeline_cards,
        metrics.context_switches,
        metrics.avg_segment_minutes,
        metrics.fragmentation_level
    )
}

fn render_week_focus_metrics(metrics: &WeekFocusMetrics, score: &WeekScoreConfig) -> String {
    if metrics.total_minutes == 0 {
        return "暂无可用专注度数据".to_string();
    }

    let focus_score = metrics.focus_score();
    let effort_score = metrics.effort_score(score.target_minutes);
    let productivity_score =
        metrics.productivity_score(score.focus_weight, score.effort_weight, score.target_minutes);

    format!(
        "- 专注时长: {} 分钟 ({}%)\n- 沟通时长: {} 分钟\n- 分心时长: {} 分钟 ({}%)\n- 专注评分: {} / 100\n- 投入时长评分: {} / 100（目标 {} 分钟）\n- 生产力评分: {} / 100（权重 {}% / {}%）\n- 细分: 工作 {} / 学习 {} / 个人 {} / 空闲 {} / 其他 {}",
        metrics.focus_minutes(),
        metrics.focus_ratio(),
        metrics.communication_minutes,
        metrics.distraction_minutes(),
        metrics.distraction_ratio(),
        focus_score,
        effort_score,
        score.target_minutes,
        productivity_score,
        score.focus_weight,
        score.effort_weight,
        metrics.work_minutes,
        metrics.learning_minutes,
        metrics.personal_minutes,
        metrics.idle_minutes,
        metrics.other_minutes
    )
}

fn build_week_insights(summary: &WeekSummaryData) -> Vec<String> {
    let mut insights = Vec::new();
    let focus_ratio = summary.focus_metrics.focus_ratio();
    let productivity_score = summary.focus_metrics.productivity_score(
        summary.score_config.focus_weight,
        summary.score_config.effort_weight,
        summary.score_config.target_minutes,
    );
    let total_minutes = summary.total_minutes;
    let avg_session_minutes = summary.avg_session_minutes;

    if focus_ratio >= 70 {
        insights.push("本周专注度较高，建议保持当前节奏".to_string());
    } else if focus_ratio <= 40 {
        insights.push("本周专注度偏低，建议减少高干扰活动".to_string());
    } else {
        insights.push("本周专注度处于中等水平，可优化任务切换".to_string());
    }

    if productivity_score >= 70 {
        insights.push("生产力评分较高，投入与产出较为平衡".to_string());
    } else if productivity_score <= 40 {
        insights.push("生产力评分偏低，需关注投入时长与专注占比".to_string());
    }

    if total_minutes < 300 {
        insights.push("本周投入时长偏少，可能处于低负荷状态".to_string());
    } else if total_minutes >= 1200 {
        insights.push("本周投入时长较高，注意避免过度疲劳".to_string());
    }

    if avg_session_minutes < 20 {
        insights.push("平均会话较短，存在碎片化倾向".to_string());
    } else if avg_session_minutes >= 60 {
        insights.push("平均会话较长，体现深度工作趋势".to_string());
    }

    insights
}

fn count_context_switches(cards: &[TimelineCardRecord]) -> usize {
    let mut switches = 0usize;
    let mut last_category: Option<String> = None;

    for card in cards {
        let category = card.category.to_lowercase();
        if let Some(last) = &last_category {
            if last != &category {
                switches += 1;
            }
        }
        last_category = Some(category);
    }

    switches
}

fn compact_summary_text(text: &str, max_len: usize) -> String {
    let cleaned = text.replace('\n', " ").replace('\r', " ");
    if cleaned.chars().count() <= max_len {
        return cleaned;
    }
    let truncated: String = cleaned.chars().take(max_len).collect();
    format!("{}...", truncated)
}

fn normalize_timeline_category(raw: &str) -> ActivityCategory {
    match raw.to_lowercase().as_str() {
        "work" => ActivityCategory::Work,
        "communication" | "meeting" => ActivityCategory::Communication,
        "learning" | "research" => ActivityCategory::Learning,
        "personal" => ActivityCategory::Personal,
        "idle" | "break" => ActivityCategory::Idle,
        _ => ActivityCategory::Other,
    }
}

fn parse_card_minutes(card: &TimelineCardRecord) -> i64 {
    let start = chrono::DateTime::parse_from_rfc3339(&card.start_time).ok();
    let end = chrono::DateTime::parse_from_rfc3339(&card.end_time).ok();
    match (start, end) {
        (Some(s), Some(e)) => (e - s).num_minutes().max(0),
        _ => 0,
    }
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
