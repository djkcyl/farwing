# Windows SDK and WDK locations, dot-sourced by the driver build scripts.
# The SDK (um/shared/ucrt headers and libraries, SignTool) and the WDK (km,
# WDF, IddCx, VHF, ACX, Inf2Cat) are installed side by side and may be
# different versions: each is the newest version that has its files.
# FARWING_SDK_VERSION / FARWING_WDK_VERSION select a specific version.
$kitRoot = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits/10'

function Select-KitVersion([string]$Override, [string[]]$Required, [string]$What) {
    $candidates = if ($Override) {
        @($Override)
    } else {
        Get-ChildItem -LiteralPath (Join-Path $kitRoot 'Include') -Directory |
            Where-Object Name -match '^10\.0\.\d+\.\d+$' |
            Sort-Object { [version]$_.Name } -Descending |
            ForEach-Object Name
    }
    foreach ($version in $candidates) {
        $missing = @($Required | Where-Object { !(Test-Path -LiteralPath (Join-Path $kitRoot ($_ -f $version))) })
        if ($missing.Count -eq 0) { return $version }
    }
    throw "$What not found under $kitRoot (needs $($Required -join ', '))."
}

$sdkVersion = Select-KitVersion $env:FARWING_SDK_VERSION @(
    'Include/{0}/um/windows.h', 'Include/{0}/ucrt', 'Lib/{0}/um/x64/kernel32.lib', 'bin/{0}/x64/signtool.exe'
) 'Windows SDK'
$wdkVersion = Select-KitVersion $env:FARWING_WDK_VERSION @(
    'Include/{0}/km/wdm.h', 'Lib/{0}/km/x64/ntoskrnl.lib', 'bin/{0}/x86/Inf2Cat.exe'
) 'Windows Driver Kit'
$signTool = Join-Path $kitRoot "bin/$sdkVersion/x64/signtool.exe"
$inf2Cat = Join-Path $kitRoot "bin/$wdkVersion/x86/Inf2Cat.exe"
Write-Verbose "Windows SDK $sdkVersion, WDK $wdkVersion"
