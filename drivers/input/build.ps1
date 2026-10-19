param([ValidatePattern('^[0-9A-Fa-f]{40}$')][string]$SigningThumbprint, [switch]$Stage)
$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$outputDir = Join-Path $repoRoot 'target/input-driver'
New-Item -ItemType Directory -Path $outputDir -Force | Out-Null
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
$vsRoot = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (!$vsRoot) { throw 'Visual C++ build tools are required.' }
$vcVersion = (Get-Content (Join-Path $vsRoot 'VC/Auxiliary/Build/Microsoft.VCToolsVersion.default.txt')).Trim()
$vcRoot = Join-Path $vsRoot "VC/Tools/MSVC/$vcVersion"
. (Join-Path $PSScriptRoot '../kits.ps1')
$includeArgs = @("/I$vcRoot/include", "/I$kitRoot/Include/$sdkVersion/um", "/I$kitRoot/Include/$sdkVersion/shared", "/I$kitRoot/Include/$sdkVersion/ucrt", "/I$kitRoot/Include/wdf/umdf/2.15")
$objectPath = Join-Path $outputDir 'driver.obj'
& (Join-Path $vcRoot 'bin/Hostx64/x64/cl.exe') /nologo /c /MT /W4 /WX /wd4324 /O2 /GS /D_AMD64_ /D_WIN64 /D_WIN32_WINNT=0x0A00 /DNTDDI_VERSION=0x0A000000 /DUNICODE /D_UNICODE $includeArgs "/Fo$objectPath" (Join-Path $PSScriptRoot 'driver.c')
if ($LASTEXITCODE) { throw 'Input driver compilation failed.' }
$driverPath = Join-Path $outputDir 'FarwingInput.dll'
& (Join-Path $vcRoot 'bin/Hostx64/x64/link.exe') /nologo /dll /subsystem:windows /include:FxDriverEntryUm /machine:x64 /dynamicbase /nxcompat /release "/out:$driverPath" $objectPath "/libpath:$vcRoot/lib/x64" "/libpath:$kitRoot/Lib/$sdkVersion/um/x64" "/libpath:$kitRoot/Lib/$sdkVersion/ucrt/x64" "/libpath:$kitRoot/Lib/wdf/umdf/x64/2.15" "/libpath:$kitRoot/Lib/$wdkVersion/um/x64" WdfDriverStubUm.lib VhfUm.lib ntdll.lib mincore.lib advapi32.lib bcrypt.lib
if ($LASTEXITCODE) { throw 'Input driver linking failed.' }
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'FarwingInput.inf') -Destination $outputDir -Force
& (Join-Path $PSScriptRoot '../package.ps1') -Directory $outputDir -Name FarwingInput -Kind input -SigningThumbprint $SigningThumbprint -Stage:$Stage
