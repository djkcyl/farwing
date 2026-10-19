$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
$OutputEncoding = [Console]::OutputEncoding
Import-Module "$PSHOME/Modules/NetAdapter/NetAdapter.psd1" -ErrorAction Stop
Import-Module "$PSHOME/Modules/CimCmdlets/CimCmdlets.psd1" -ErrorAction Stop
$adapters = @(Get-NetAdapter -Physical | Where-Object { $_.PhysicalMediaType -eq '802.3' })
if ($Target) {
    $adapters = @($adapters | Where-Object { ([guid]$_.InterfaceGuid).ToString() -eq $Target -and ($_.MacAddress -replace '-', '') -eq $ExpectedMac })
    if ($adapters.Count -ne 1) { throw 'Selected physical Ethernet adapter changed or is unavailable' }
}
$rows = @()
foreach ($nic in $adapters) {
    $items = @()
    $failures = @()
    $power = $null
    $advanced = @()
    try { $power = $nic | Get-NetAdapterPowerManagement } catch { $failures += 'Power-management properties could not be read' }
    try { $advanced = @($nic | Get-NetAdapterAdvancedProperty -AllProperties) } catch { $failures += 'Advanced properties could not be read' }
    $wake = @()
    $magic = @()
    $pattern = '^' + [regex]::Escape($nic.PnPDeviceID) + '(_[0-9]+)?$'
    try { $wake = @(Get-CimInstance -Namespace root/wmi -ClassName MSPower_DeviceWakeEnable | Where-Object { $_.InstanceName -match $pattern }) } catch { }
    try { $magic = @(Get-CimInstance -Namespace root/wmi -ClassName MSNdis_DeviceWakeOnMagicPacketOnly | Where-Object { $_.InstanceName -match $pattern }) } catch { }
    if ($Apply) {
        foreach ($field in @('WakeOnMagicPacket', 'AllowComputerToTurnOffDevice')) {
            if ($power -and [string]$power.$field -eq 'Disabled') {
                try {
                    $powerArguments = @{InputObject=$power; NoRestart=$true; ErrorAction='Stop'}
                    $powerArguments[$field] = 'Enabled'
                    Set-NetAdapterPowerManagement @powerArguments
                } catch { $failures += "Could not enable $field" }
            }
        }
        foreach ($property in $advanced) {
            if ($property.RegistryKeyword -cin @('S5WakeOnLan', 'EnablePME', '*WakeOnMagicPacket')) {
                if ([string]($property.RegistryValue -join ',') -ne '1') {
                    if (@($property.ValidRegistryValues) -contains '1') {
                        try { $property | Set-NetAdapterAdvancedProperty -RegistryValue 1 -NoRestart -ErrorAction Stop } catch { $failures += "Could not enable $($property.RegistryKeyword)" }
                    } else { $failures += "No supported enable value for $($property.RegistryKeyword)" }
                }
            }
        }
        if ($wake.Count -eq 1 -and !$wake[0].Enable) {
            try { $wake[0] | Set-CimInstance -Property @{Enable=$true} -ErrorAction Stop | Out-Null } catch { $failures += 'Could not enable device wake' }
        }
        if ($magic.Count -eq 1 -and !$magic[0].Enable) {
            try { $magic[0] | Set-CimInstance -Property @{Enable=$true} -ErrorAction Stop | Out-Null } catch { $failures += 'Could not restrict wake to magic packets' }
        }
        try { $power = $nic | Get-NetAdapterPowerManagement } catch { $power = $null; $failures += 'Power-management readback failed' }
        try { $advanced = @($nic | Get-NetAdapterAdvancedProperty -AllProperties) } catch { $advanced = @(); $failures += 'Advanced-property readback failed' }
        try { $wake = @(Get-CimInstance -Namespace root/wmi -ClassName MSPower_DeviceWakeEnable | Where-Object { $_.InstanceName -match $pattern }) } catch { $wake = @() }
        try { $magic = @(Get-CimInstance -Namespace root/wmi -ClassName MSNdis_DeviceWakeOnMagicPacketOnly | Where-Object { $_.InstanceName -match $pattern }) } catch { $magic = @() }
    }
    foreach ($field in @('WakeOnMagicPacket', 'AllowComputerToTurnOffDevice')) {
        $value = if ($power) { [string]$power.$field } else { 'Unknown' }
        $items += @{key=$field; value=$value; writable=($value -in @('Enabled','Disabled'))}
    }
    foreach ($key in @('S5WakeOnLan', 'EnablePME', '*WakeOnMagicPacket')) {
        $p = @($advanced | Where-Object { $_.RegistryKeyword -ceq $key })
        if ($p.Count -eq 1) {
            $items += @{key=$key; value=[string]($p[0].RegistryValue -join ','); writable=(@($p[0].ValidRegistryValues) -contains '1')}
        }
    }
    $items += @{key='DeviceWake'; value=$(if ($wake.Count -eq 1) { if ($wake[0].Enable) {'Enabled'} else {'Disabled'} } else {'Unknown'}); writable=($wake.Count -eq 1)}
    $items += @{key='MagicPacketOnly'; value=$(if ($magic.Count -eq 1) { if ($magic[0].Enable) {'Enabled'} else {'Disabled'} } else {'Unknown'}); writable=($magic.Count -eq 1)}
    if ($Apply) {
        foreach ($item in $items) {
            if ($item.writable -and $item.value -in @('Disabled','0')) { $failures += "Readback did not confirm $($item.key)" }
        }
    }
    $rows += @{
        key=@{guid=([guid]$nic.InterfaceGuid).ToString(); mac=($nic.MacAddress -replace '-', '')}
        name=[string]$nic.Name; description=[string]$nic.InterfaceDescription; index=[uint32]$nic.ifIndex
        connected=([string]$nic.Status -eq 'Up'); items=@($items); errors=@($failures)
    }
}
ConvertTo-Json -InputObject @($rows) -Depth 6 -Compress
