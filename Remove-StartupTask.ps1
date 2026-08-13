[CmdletBinding()]
param([string]$TaskName = 'Codex Process Guardian Rust')

Write-Host '⚠️ 危险操作检测！' -ForegroundColor Yellow
Write-Host '操作类型：停止并删除 Windows 计划任务'
Write-Host "影响范围：$TaskName"
Write-Host '风险评估：删除后不再登录自启，日志和状态不会删除。'
$confirmation = Read-Host '请确认是否继续？请输入“是”、“确认”或“继续”'
if ($confirmation -notin @('是', '确认', '继续')) {
    Write-Host '已取消。'
    exit 1
}

& (Join-Path $PSScriptRoot 'guardian-cli.exe') --stop-watch 2>$null
$task = Get-ScheduledTask -TaskName $TaskName -ErrorAction Stop
if ($task.State -eq 'Running') {
    Stop-ScheduledTask -TaskName $TaskName -ErrorAction Stop
}
Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
Write-Host "已停止并删除：$TaskName" -ForegroundColor Green

