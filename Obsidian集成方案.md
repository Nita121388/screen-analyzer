# Obsidian 集成方案（混合式）

## 目标
- 将屏幕活动沉淀为 Obsidian 可检索、可链接的知识记录
- 维持低心智负担与低存储压力，避免大文件拖慢 Vault
- 为后续插件化扩展预留协议

## 方案选择
采用混合式路径：先做 Markdown 导出（MVP），再为插件协议预留元数据与索引。

## 导出结构（建议）
```
<Vault>/
  ScreenAnalyzer/
    Daily/
      2025-01-15.md
    Sessions/
      2025-01-15/
        2025-01-15_0900-1015_session-123.md
    Assets/
      2025-01-15/
        session-123-preview.jpg (可选)
    Index/
      sessions-2025-01.md (可选)
      weeks-2025-W03.md (可选)
    Weekly/
      2025-W03.md (可选)
```

## Markdown 模板（建议）
### Daily
```
---
type: screen-analyzer-daily
date: 2025-01-15
session_count: 6
focus_score: 78
productivity_score: 72
source: screen-analyzer
---

# 2025-01-15 屏幕活动总结
<自动生成的总结内容>
```

### Session
```
---
type: screen-analyzer-session
date: 2025-01-15
session_id: 123
start: 09:00
end: 10:15
duration_minutes: 75
tags: [work, communication]
video_path: "file:///..."
source: screen-analyzer
---

# 09:00-10:15 会话总结
<会话摘要>

## 时间线
- 09:00-09:30 ...
- 09:30-10:15 ...
```

## 链接策略
- 默认仅写入视频/截图的文件链接，不复制大文件
- 可配置为复制缩略图或关键截图（可选）
- 写入路径使用相对路径，便于 Vault 迁移

## 配置项（建议）
- `obsidian_export.enabled`: 是否启用导出
- `obsidian_export.vault_path`: Vault 路径
- `obsidian_export.export_mode`: `link` / `copy`
- `obsidian_export.daily_template` / `session_template`: 自定义模板
- `obsidian_export.include_screenshots`: 是否包含截图链接

## MVP 范围
- 导出每日总结 Markdown
- 导出会话详情 Markdown（含时间线与标签）
- 支持手动导出与定时导出两种入口

## 周度索引补充
- 生成周度索引文件并汇总专注度（基于时间线卡片时长）
- 默认按 ISO 周统计（周一到周日）

## 风险与控制
- 仅导出必要信息，避免包含 API Key 等敏感内容
- 对大文件采用链接策略，避免 Vault 膨胀
- 统一命名与目录结构，减少冲突与重复
