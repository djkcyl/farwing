# Build Farwing Server with its web console embedded: the front end first
# (server/web → server/web/dist), then the server binary that includes it.
#
#   pwsh server/build-web.ps1            # release build
#   pwsh server/build-web.ps1 -SkipCargo # front end only
param(
    [switch]$SkipCargo
)
$ErrorActionPreference = 'Stop'
$server = $PSScriptRoot
$web = Join-Path $server 'web'

Push-Location $web
try {
    npm ci
    if ($LASTEXITCODE) { throw "npm ci failed ($LASTEXITCODE)" }
    npm run check
    if ($LASTEXITCODE) { throw "front-end checks failed ($LASTEXITCODE)" }
    npm run build
    if ($LASTEXITCODE) { throw "front-end build failed ($LASTEXITCODE)" }
} finally {
    Pop-Location
}

if (-not $SkipCargo) {
    Push-Location $server
    try {
        cargo build --release --locked
        if ($LASTEXITCODE) { throw "cargo build failed ($LASTEXITCODE)" }
    } finally {
        Pop-Location
    }
    Write-Host "farwing-server with the web console: $server\target\release\farwing-server.exe"
}
