# Codex Process Guardian Rust

Windows 原生轻量版，无 PowerShell 常驻、无 WMI 常驻、无 WebView、无 .NET、无异步运行时。

## 使用

1. 双击 `guardian.exe` 打开图形界面。
2. 双击 `guardian-cli.exe` 打开终端管理器。
3. GUI 中点击“启动监控”，或运行 `guardian.exe --watch`。
4. 后台安全停止：`guardian-cli.exe --stop-watch`。

## 图形界面

- 主表同时显示风险、状态、PID、进程类型、归属宿主、父 PID、子进程数、运行时长和闲置时长。
- 单击任意进程可查看完整的“宿主 → 启动器 → 当前进程”关系链和命令行。
- 搜索支持 PID、进程名、类型、状态、宿主、关系路径和命令行。
- 每行都有复选框，并提供“勾选需关注”“全选当前”“清空勾选”和“批量结束已勾选”。
- 批量结束会合并父子重叠选择，操作前展示影响范围并要求一次明确确认，结束后逐项报告结果。

## 默认安全策略

- 默认 `action=report`，只报告，不自动结束进程。
- 只有监控器实际观察到由 ChatGPT/Codex 宿主启动的进程，才允许自动或手动结束。
- 结束前复核 PID 与创建时间，防止 PID 被系统复用后误杀。
- 创建时间校验与终止操作绑定到同一个进程句柄，避免校验后 PID 被复用。
- 发出终止请求后等待对应进程句柄确认退出，不依赖固定延迟或重复全系统快照。
- GUI/CLI 手动结束均要求明确确认；未确认归属的进程拒绝操作。
- 自动清理只处理宿主退出且超过宽限期的候选项；宿主在线的闲置浏览器默认只告警。
- 后台扫描或自动清理失败会写入日志；相同连续错误只记录一次，恢复时记录恢复事件。
- GUI 扫描失败时会明确标注“上次成功快照”，避免把陈旧列表误认为当前结果。

## 配置

编辑同目录的 `guardian.conf`：

- `interval_seconds=30`：扫描间隔，最低 5 秒。
- `grace_minutes=5`：宿主退出后的宽限期。
- `owned_browser_idle_minutes=20`：宿主在线时的闲置浏览器告警阈值。
- `owned_tool_idle_minutes=30`：宿主在线时 MCP、CodeGraph、AgentMemory、CUA runtime 等工具服务的闲置告警阈值。
- `action=report`：改为 `terminate` 才启用自动清理。
- `terminate_owned_idle_browser=false`：是否允许自动结束宿主在线的闲置浏览器。
- `terminate_owned_idle_tool=false`：是否允许自动结束宿主在线的闲置工具服务。

## 数据

状态与日志位于 `%LOCALAPPDATA%\CodexProcessGuardian`：

- `state-rust.tsv`
- `events-rust.log`

状态文件只保存进程身份、分类和活动时间，不持久化完整命令行；完整命令行仅在实时扫描的界面中显示。

## 性能基线

在本机当前进程规模下：

- 完整扫描平均约 160 ms。
- 后台私有内存约 7.6 MB；新版原生表格 GUI 打开后约 9.4 MB。
- `guardian.exe` 约 292 KB，`guardian-cli.exe` 约 276 KB。
- 默认每 30 秒扫描一次，扫描间隔内由内核事件等待，不轮询。

## 登录启动

运行 `Install-StartupTask.ps1`，按中文提示确认。卸载运行 `Remove-StartupTask.ps1`。
