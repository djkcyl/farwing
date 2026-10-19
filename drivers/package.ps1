param(
    [Parameter(Mandatory)][string]$Directory,
    [Parameter(Mandatory)][string]$Name,
    [ValidateSet('input','display')][string]$Kind,
    [ValidatePattern('^[0-9A-Fa-f]{40}$')][string]$SigningThumbprint,
    [switch]$Stage
)
$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
. (Join-Path $PSScriptRoot 'kits.ps1')
if ($SigningThumbprint) {
    $cert = Get-Item -LiteralPath "Cert:/CurrentUser/My/$SigningThumbprint"
    if ($cert.Subject -ne 'CN=Farwing Drivers' -or !$cert.HasPrivateKey) { throw 'Use the Farwing Drivers signing identity.' }
    & $signTool sign /fd SHA256 /s My /sha1 $SigningThumbprint (Join-Path $Directory "$Name.dll")
    if ($LASTEXITCODE) { throw 'Driver DLL signing failed.' }
}
& $inf2Cat "/driver:$Directory" /os:10_X64 /uselocaltime
if ($LASTEXITCODE) { throw 'Driver catalog generation failed.' }
if ($SigningThumbprint) {
    & $signTool sign /fd SHA256 /s My /sha1 $SigningThumbprint (Join-Path $Directory "$Name.cat")
    if ($LASTEXITCODE) { throw 'Driver catalog signing failed.' }
    if ($Stage) {
        $assets = Join-Path $repoRoot "assets/drivers/$Kind"
        New-Item -ItemType Directory -Path $assets -Force | Out-Null
        foreach ($extension in @('inf','dll','cat')) {
            Copy-Item -LiteralPath (Join-Path $Directory "$Name.$extension") -Destination $assets -Force
        }
        Export-Certificate -Cert $cert -FilePath (Join-Path $repoRoot 'assets/drivers/FarwingDrivers.cer') -Force | Out-Null
    }
} elseif ($Stage) { throw 'Only a signed package can be staged.' }
Write-Output "Driver package: $Directory"
