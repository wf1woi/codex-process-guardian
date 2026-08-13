[CmdletBinding()]
param([string]$TaskName = 'Codex Process Guardian Rust')

$exePath = Join-Path $PSScriptRoot 'guardian.exe'
if (-not (Test-Path -LiteralPath $exePath)) {
    throw "找不到程序：$exePath"
}

Write-Host '⚠️ 危险操作检测！' -ForegroundColor Yellow
Write-Host '操作类型：注册 Windows 登录启动计划任务'
Write-Host "影响范围：当前用户；任务名称 $TaskName；程序 $exePath --watch"
Write-Host '风险评估：会修改当前用户的计划任务；默认配置仅报告，不结束进程。'
$confirmation = Read-Host '请确认是否继续？请输入“是”、“确认”或“继续”'
if ($confirmation -notin @('是', '确认', '继续')) {
    Write-Host '已取消。'
    exit 1
}

$action = New-ScheduledTaskAction -Execute $exePath -Argument '--watch' -WorkingDirectory $PSScriptRoot
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1) -ExecutionTimeLimit ([TimeSpan]::Zero)
$principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Limited
Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger -Settings $settings -Principal $principal -Description '轻量监控 ChatGPT/Codex 遗留进程。' -Force | Out-Null
Start-ScheduledTask -TaskName $TaskName
Write-Host "已安装并启动：$TaskName" -ForegroundColor Green

