param([ValidatePattern('^[0-9A-Fa-f]{40}$')][string]$SigningThumbprint, [switch]$Stage)
$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$outputDir = Join-Path $repoRoot 'target/display-driver'
New-Item -ItemType Directory -Path $outputDir -Force | Out-Null
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
$vsRoot = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (!$vsRoot) { throw 'Visual C++ build tools are required.' }
$vcVersion = (Get-Content (Join-Path $vsRoot 'VC/Auxiliary/Build/Microsoft.VCToolsVersion.default.txt')).Trim()
$vcRoot = Join-Path $vsRoot "VC/Tools/MSVC/$vcVersion"
. (Join-Path $PSScriptRoot '../kits.ps1')
$includes = @("/I$PSScriptRoot", "/I$vcRoot/include", "/I$kitRoot/Include/$sdkVersion/um", "/I$kitRoot/Include/$sdkVersion/shared", "/I$kitRoot/Include/$sdkVersion/ucrt", "/I$kitRoot/Include/$sdkVersion/winrt", "/I$kitRoot/Include/wdf/umdf/2.25", "/I$kitRoot/Include/$wdkVersion/um/iddcx/1.10", "/I$kitRoot/Include/$wdkVersion/um")
$object = Join-Path $outputDir 'driver.obj'
& (Join-Path $vcRoot 'bin/Hostx64/x64/cl.exe') /nologo /c /std:c++17 /EHsc /MT /W4 /wd4324 /O2 /GS /D_AMD64_ /D_WIN64 /D_WIN32_WINNT=0x0A00 /DNTDDI_VERSION=0x0A000000 /DUNICODE /D_UNICODE /DUMDF_DRIVER /DUMDF_VERSION_MAJOR=2 /DUMDF_VERSION_MINOR=25 /DIDDCX_VERSION_MAJOR=1 /DIDDCX_VERSION_MINOR=10 /DIDDCX_MINIMUM_VERSION_REQUIRED=4 $includes "/Fo$object" (Join-Path $PSScriptRoot 'Driver.cpp')
if ($LASTEXITCODE) { throw 'Display driver compilation failed.' }
$binary = Join-Path $outputDir 'FarwingDisplay.dll'
& (Join-Path $vcRoot 'bin/Hostx64/x64/link.exe') /nologo /dll /subsystem:windows /include:FxDriverEntryUm /machine:x64 /dynamicbase /nxcompat /release "/out:$binary" $object "/libpath:$vcRoot/lib/x64" "/libpath:$kitRoot/Lib/$sdkVersion/um/x64" "/libpath:$kitRoot/Lib/$sdkVersion/ucrt/x64" "/libpath:$kitRoot/Lib/wdf/umdf/x64/2.25" "/libpath:$kitRoot/Lib/$wdkVersion/um/x64/iddcx/1.10" WdfDriverStubUm.lib IddCxStub.lib ntdll.lib OneCoreUAP.lib d3d11.lib dxgi.lib avrt.lib
if ($LASTEXITCODE) { throw 'Display driver linking failed.' }
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'FarwingDisplay.inf') -Destination $outputDir -Force
& (Join-Path $PSScriptRoot '../package.ps1') -Directory $outputDir -Name FarwingDisplay -Kind display -SigningThumbprint $SigningThumbprint -Stage:$Stage
