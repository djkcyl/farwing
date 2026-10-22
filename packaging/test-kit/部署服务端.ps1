<#
部署 Farwing 测试服务端：生成配置、改用内置中继并开启 NAT 探测端口、创建测试账号。

用法（在放有 farwing-server.exe 的目录中运行）：
  .\部署服务端.ps1 -Address <测试者访问服务器用的 IP 或域名> -PublicIp <服务器的公网 IPv4>

  -Address   客户端连接服务器时填写的地址（不含端口）。
  -PublicIp  中继与 NAT 探测对外公布的 IPv4。服务器在路由器后面时填路由器的公网 IP，
             并在路由器上转发下面列出的端口；只在同一局域网测试时填服务器的局域网 IP。
  -Port      网页与登录的 HTTPS 端口，默认 8443。
  -AddFirewallRules  同时在本机 Windows 防火墙放行这些端口（需要以管理员身份运行）。
  -NoAccounts        不创建账号（已经有账号时）。
#>
param(
    [Parameter(Mandatory = $true)][string]$Address,
    [Parameter(Mandatory = $true)][string]$PublicIp,
    [int]$Port = 8443,
    [switch]$AddFirewallRules,
    [switch]$NoAccounts
)
$ErrorActionPreference = 'Stop'
# Passwords go to farwing-server.exe through stdin as UTF-8.
$OutputEncoding = New-Object System.Text.UTF8Encoding $false
Set-Location -LiteralPath $PSScriptRoot
$server = Join-Path $PSScriptRoot 'farwing-server.exe'
$config = Join-Path $PSScriptRoot 'farwing-server.toml'
if (-not (Test-Path -LiteralPath $server)) { throw "没有找到 farwing-server.exe（应与本脚本在同一目录）" }
if (Test-Path -LiteralPath $config) { throw "已经有 farwing-server.toml。要重新部署，请先把它和 data 目录移走" }
$ip = $null
if (-not [System.Net.IPAddress]::TryParse($PublicIp, [ref]$ip) -or $ip.AddressFamily -ne 'InterNetwork') {
    throw "-PublicIp 需要一个 IPv4 地址，例如 203.0.113.10"
}

& $server init --config $config --public-url "https://${Address}:$Port" --data-dir .\data
if ($LASTEXITCODE -ne 0) { throw "生成配置失败" }

# 改用内置中继，并加两个 NAT 行为探测端口（打洞按探测结果规划）。
$text = [System.IO.File]::ReadAllText($config)
$text = $text -replace '(?m)^listen = "0\.0\.0\.0:8443"', "listen = `"0.0.0.0:$Port`""
$text = $text -replace '(?m)^mode = "external"', 'mode = "builtin"'
$text = [regex]::Replace($text, '(?ms)^urls = \[.*?^\]\r?\n', "urls = []`n")
$text = $text -replace '(?m)^# listen_udp = ', 'listen_udp = '
$text = $text -replace '(?m)^# listen_tcp = ', 'listen_tcp = '
$text = $text -replace '(?m)^# public_ip = "[^"]*"', "public_ip = `"$PublicIp`""
$text = $text -replace '(?m)^# relay_port_min = ', 'relay_port_min = '
$text = $text -replace '(?m)^# relay_port_max = ', 'relay_port_max = '
$stun = @"
[[turn.stun]]
listen = "0.0.0.0:3479"
public = "${PublicIp}:3479"

[[turn.stun]]
listen = "0.0.0.0:3480"
public = "${PublicIp}:3480"

"@
$limits = $text.IndexOf("`n[limits]")
if ($limits -lt 0) { throw "配置格式与预期不同（没有 [limits] 段）" }
$text = $text.Insert($limits + 1, $stun)
[System.IO.File]::WriteAllText($config, $text, (New-Object System.Text.UTF8Encoding $false))

# 检查配置能被读取。
& $server user list --config $config | Out-Null
if ($LASTEXITCODE -ne 0) { throw "配置检查失败" }

if (-not $NoAccounts) {
    foreach ($account in @(
            @{ Name = 'admin'; Admin = $true },
            @{ Name = 'alice'; Admin = $false },
            @{ Name = 'bob'; Admin = $false })) {
        $secure = Read-Host -AsSecureString "设置 $($account.Name) 的密码"
        $plain = [System.Runtime.InteropServices.Marshal]::PtrToStringBSTR(
            [System.Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure))
        $arguments = @('user', 'create', $account.Name, '--password-stdin', '--config', $config)
        if ($account.Admin) { $arguments += '--admin' }
        $plain | & $server @arguments
        $plain = $null
        if ($LASTEXITCODE -ne 0) { throw "创建账号 $($account.Name) 失败" }
    }
}

$ports = @(
    @{ Name = 'Farwing HTTPS'; Protocol = 'TCP'; Port = "$Port"; Use = '网页、登录与实时信令' },
    @{ Name = 'Farwing STUN/TURN'; Protocol = 'UDP'; Port = '3478'; Use = 'STUN / TURN' },
    @{ Name = 'Farwing TURN TCP'; Protocol = 'TCP'; Port = '3478'; Use = 'TURN（UDP 不通时）' },
    @{ Name = 'Farwing NAT discovery'; Protocol = 'UDP'; Port = '3479-3480'; Use = 'NAT 行为探测' },
    @{ Name = 'Farwing relay'; Protocol = 'UDP'; Port = '49160-49200'; Use = '中继' }
)
if ($AddFirewallRules) {
    foreach ($p in $ports) {
        New-NetFirewallRule -DisplayName $p.Name -Direction Inbound -Action Allow `
            -Protocol $p.Protocol -LocalPort $p.Port | Out-Null
    }
    Write-Host '已在本机 Windows 防火墙放行以下端口。'
} else {
    Write-Host '请在本机防火墙（以及服务器在路由器后面时的路由器端口转发）放行以下端口：'
}
foreach ($p in $ports) { Write-Host ("  {0,-4} {1,-12} {2}" -f $p.Protocol, $p.Port, $p.Use) }
Write-Host ''
Write-Host '启动服务端：'
Write-Host "  .\farwing-server.exe serve --config farwing-server.toml"
Write-Host "启动日志中的证书指纹，测试者第一次连接 ${Address}:$Port 时核对。"
Write-Host "网页管理：https://${Address}:$Port/（admin 登录）"
