param([ValidatePattern('^[0-9A-Fa-f]{40}$')][string]$TestSigningThumbprint, [switch]$Analyze, [switch]$Stage)
$ErrorActionPreference = 'Stop'
if ($Stage -and !$TestSigningThumbprint) { throw 'Staging requires a signed driver package.' }
$repo = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$out = Join-Path $repo 'target/audio-driver'
New-Item -ItemType Directory -Path $out -Force | Out-Null
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
$vs = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (!$vs) { throw 'Visual C++ build tools are required.' }
$version = (Get-Content (Join-Path $vs 'VC/Auxiliary/Build/Microsoft.VCToolsVersion.default.txt')).Trim()
$vc = Join-Path $vs "VC/Tools/MSVC/$version"
. (Join-Path $PSScriptRoot '../kits.ps1')
$kit = $kitRoot
$includes = @("/external:I$vc/include", "/external:I$kit/Include/$wdkVersion/km", "/external:I$kit/Include/$wdkVersion/shared", "/external:I$kit/Include/$sdkVersion/shared", "/external:I$kit/Include/$sdkVersion/ucrt", "/external:I$kit/Include/wdf/kmdf/1.31", "/external:I$kit/Include/$wdkVersion/km/acx/km/1.1")
$objects = @()
$analysisFlags = if ($Analyze) { @('/analyze', '/analyze:external-', '/wd28301') } else { @() } # WDK POOL_FLAGS SAL redeclaration.
foreach ($unit in @('device','circuit','stream','bridge')) {
    $object = Join-Path $out "$unit.obj"
    & (Join-Path $vc 'bin/Hostx64/x64/cl.exe') /nologo /c /external:W0 /kernel /std:c++17 /GR- /EHs-c- /Zl /W4 /WX /wd4201 /wd4324 /O2 /GS /Zi /guard:cf /D_AMD64_ /D_WIN64 /D_WIN32_WINNT=0x0A00 /DNTDDI_VERSION=0x0A000008 /DUNICODE /D_UNICODE /DACX_VERSION_MAJOR=1 /DACX_VERSION_MINOR=1 /DACX_MINIMUM_VERSION_REQUIRED=0 /DKMDF_VERSION_MAJOR=1 /DKMDF_VERSION_MINOR=31 $includes $analysisFlags "/Fo$object" "/Fd$out/compiler.pdb" (Join-Path $PSScriptRoot "$unit.cpp")
    if ($LASTEXITCODE) { throw "Audio driver compilation failed: $unit" }
    $objects += $object
}
$binary = Join-Path $out 'FarwingAudio.sys'
& (Join-Path $vc 'bin/Hostx64/x64/link.exe') /nologo /driver /subsystem:native,10.00 /entry:FxDriverEntry /machine:x64 /dynamicbase /nxcompat /integritycheck /guard:cf /release /debug:full /opt:ref /opt:icf "/out:$binary" "/pdb:$out/FarwingAudio.pdb" $objects "/libpath:$kit/Lib/$wdkVersion/km/x64" "/libpath:$kit/Lib/wdf/kmdf/x64/1.31" ntoskrnl.lib hal.lib wmilib.lib BufferOverflowFastFailK.lib WdfDriverEntry.lib WdfLdr.lib acx/km/1.1/acxstub.lib
if ($LASTEXITCODE) { throw 'Audio driver linking failed.' }
# Keep source UTF-8 for review; Windows INF localization requires UTF-16LE.
[IO.File]::WriteAllText((Join-Path $out 'FarwingAudio.inf'),
    [IO.File]::ReadAllText((Join-Path $PSScriptRoot 'FarwingAudio.inf'), [Text.Encoding]::UTF8),
    [Text.Encoding]::Unicode)
if ($TestSigningThumbprint) {
    & $signTool sign /fd SHA256 /s My /sha1 $TestSigningThumbprint $binary
    if ($LASTEXITCODE) { throw 'Audio driver test signing failed.' }
}
$catalogStage = Join-Path $out ('catalog-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $catalogStage | Out-Null
try {
    # Inf2Cat scans its directory; compiler/debugger files are not driver payload.
    Copy-Item -LiteralPath $binary, (Join-Path $out 'FarwingAudio.inf') -Destination $catalogStage
    & $inf2Cat "/driver:$catalogStage" /os:10_X64 /uselocaltime
    if ($LASTEXITCODE) { throw 'Audio driver catalog generation failed.' }
    Copy-Item -LiteralPath (Join-Path $catalogStage 'FarwingAudio.cat') -Destination $out -Force
} finally {
    foreach ($name in @('FarwingAudio.sys','FarwingAudio.inf','FarwingAudio.cat')) {
        $file = Join-Path $catalogStage $name
        if (Test-Path -LiteralPath $file) { Remove-Item -LiteralPath $file }
    }
    Remove-Item -LiteralPath $catalogStage
}
if ($TestSigningThumbprint) {
    & $signTool sign /fd SHA256 /s My /sha1 $TestSigningThumbprint (Join-Path $out 'FarwingAudio.cat')
    if ($LASTEXITCODE) { throw 'Audio catalog test signing failed.' }
}
if ($Stage) {
    $stageDir = Join-Path $repo 'assets/drivers/audio'
    New-Item -ItemType Directory -Path $stageDir -Force | Out-Null
    foreach ($name in @('FarwingAudio.inf','FarwingAudio.sys','FarwingAudio.cat')) {
        Copy-Item -LiteralPath (Join-Path $out $name) -Destination $stageDir -Force
    }
}
Write-Output "Audio driver build: $out (test-signed when requested; no system settings changed)"
