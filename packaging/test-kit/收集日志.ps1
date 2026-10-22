<#
收集本机的 Farwing 日志，打包到桌面（Farwing-logs-<计算机名>-<时间>.zip），并摘出与连接、线路、恢复有关的关键行（key-lines.txt）。

用法：右键 → 使用 PowerShell 运行；或在 PowerShell 中运行 .\收集日志.ps1
  -Days    收集最近几天的日志，默认 2。
  -OutDir  zip 放在哪里，默认桌面。
装了后台服务的电脑，以管理员身份运行才能收集服务的日志。
#>
param(
    [int]$Days = 2,
    [string]$OutDir = [Environment]::GetFolderPath('Desktop')
)
$ErrorActionPreference = 'Continue'
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$name = "Farwing-logs-$env:COMPUTERNAME-$stamp"
$work = Join-Path $env:TEMP $name
New-Item -ItemType Directory -Path $work -Force | Out-Null
$since = (Get-Date).AddDays(-$Days)
$notes = New-Object System.Collections.Generic.List[string]

$sources = New-Object System.Collections.Generic.List[object]
$sources.Add(@{ Name = 'user'; Path = Join-Path $env:LOCALAPPDATA 'Farwing\logs' })
$sources.Add(@{ Name = 'service'; Path = Join-Path $env:ProgramData 'Farwing\host\logs' })
Get-ChildItem -LiteralPath (Join-Path $env:LOCALAPPDATA 'Farwing\profiles') -Directory -ErrorAction SilentlyContinue |
    ForEach-Object { $sources.Add(@{ Name = "profile-$($_.Name)"; Path = Join-Path $_.FullName 'logs' }) }

foreach ($source in $sources) {
    if (-not (Test-Path -LiteralPath $source.Path)) { continue }
    try {
        $files = Get-ChildItem -LiteralPath $source.Path -File -ErrorAction Stop |
            Where-Object { $_.LastWriteTime -ge $since }
    } catch {
        $notes.Add("$($source.Name)：没有权限读取 $($source.Path)，请以管理员身份运行本脚本")
        continue
    }
    if (-not $files) { continue }
    $target = Join-Path $work $source.Name
    New-Item -ItemType Directory -Path $target -Force | Out-Null
    foreach ($file in $files) {
        try {
            # Logs that are still open are read through a shared handle.
            $in = [System.IO.File]::Open($file.FullName, 'Open', 'Read', 'ReadWrite')
            $out = [System.IO.File]::Create((Join-Path $target $file.Name))
            $in.CopyTo($out)
            $out.Dispose(); $in.Dispose()
        } catch {
            $notes.Add("无法复制 $($file.FullName)：$($_.Exception.Message)")
        }
    }
}

# Key lines: connection outcome, path choice, hole punching, recovery, errors.
$patterns = @(
    'connection path summary', 'path switch', 'path load test', 'peer NAT behaviour',
    'NAT groups', 'punch round', 'punch promoted', 'punch stopped', 'probe punching skipped',
    'host selected transport path', 'selected ICE candidate pair changed', 'first picture presented',
    'local network path changed', 'system resumed', 'signaling is back', 'ICE restart',
    'did not connect in time', ' ERROR ', 'panicked'
)
$logs = Get-ChildItem -LiteralPath $work -Recurse -File -Filter '*.log' -ErrorAction SilentlyContinue
$key = Join-Path $work 'key-lines.txt'
if ($logs) {
    Select-String -LiteralPath $logs.FullName -SimpleMatch -Pattern $patterns -Encoding UTF8 |
        ForEach-Object { "{0}: {1}" -f $_.Filename, $_.Line } |
        Set-Content -LiteralPath $key -Encoding UTF8
} else {
    $notes.Add("最近 $Days 天没有找到日志")
}

# Environment: Windows, graphics, displays, network adapters.
$environment = New-Object System.Collections.Generic.List[string]
$os = Get-CimInstance Win32_OperatingSystem
$environment.Add("计算机：$env:COMPUTERNAME")
$environment.Add("Windows：$($os.Caption) $($os.Version)（$($os.OSArchitecture)）")
Get-CimInstance Win32_VideoController | ForEach-Object {
    $environment.Add("显卡：$($_.Name)，驱动 $($_.DriverVersion)，$($_.CurrentHorizontalResolution)×$($_.CurrentVerticalResolution) @ $($_.CurrentRefreshRate) Hz")
}
Get-NetAdapter -ErrorAction SilentlyContinue | Where-Object { $_.Status -eq 'Up' } | ForEach-Object {
    $environment.Add("网卡：$($_.Name)（$($_.InterfaceDescription)），$($_.LinkSpeed)")
}
$installed = Join-Path $env:ProgramFiles 'Farwing\Farwing.exe'
if (Test-Path -LiteralPath $installed) {
    $stream = [System.IO.File]::OpenRead($installed)
    $digest = [System.Security.Cryptography.SHA256]::Create().ComputeHash($stream)
    $stream.Dispose()
    $hash = (($digest | ForEach-Object { $_.ToString('x2') }) -join '').Substring(0, 16)
    $environment.Add("已安装的 Farwing：$installed，SHA256 前 16 位 $hash")
}
$service = Get-Service -Name 'FarwingHostService' -ErrorAction SilentlyContinue
if ($service) { $environment.Add("后台服务：$($service.Status)") }
$environment | Set-Content -LiteralPath (Join-Path $work 'environment.txt') -Encoding UTF8
if ($notes.Count) { $notes | Set-Content -LiteralPath (Join-Path $work 'notes.txt') -Encoding UTF8 }

$zip = Join-Path $OutDir "$name.zip"
Compress-Archive -Path (Join-Path $work '*') -DestinationPath $zip -Force
Remove-Item -LiteralPath $work -Recurse -Force
Write-Host "已生成：$zip"
foreach ($note in $notes) { Write-Host $note }
if ($Host.Name -eq 'ConsoleHost' -and -not $env:FARWING_NO_PAUSE) { Read-Host '按回车键关闭' | Out-Null }
