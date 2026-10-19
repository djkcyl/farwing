# Checks the staged driver packages: INF matches source, signer is the staged
# Farwing Drivers certificate, and the INF and binary are catalog members.
# -AllowUntrustedRoot: on a build machine that does not trust the self-signed
# certificate, accept signatures whose only chain problem is that untrusted
# root (integrity and signer are still checked; catalog membership is checked
# through the catalog API instead of SignTool's trust policy).
param([switch]$AllowUntrustedRoot)
$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$assets = Join-Path $repoRoot 'assets/drivers'
. (Join-Path $PSScriptRoot 'kits.ps1')

Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Runtime.InteropServices;
public static class FarwingCatalog {
    [DllImport("wintrust.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool CryptCATAdminAcquireContext2(out IntPtr admin, IntPtr subsystem, string algorithm, IntPtr policy, uint flags);
    [DllImport("wintrust.dll", SetLastError = true)]
    static extern bool CryptCATAdminCalcHashFromFileHandle2(IntPtr admin, IntPtr file, ref uint size, byte[] hash, uint flags);
    [DllImport("wintrust.dll")]
    static extern bool CryptCATAdminReleaseContext(IntPtr admin, uint flags);
    [DllImport("wintrust.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern IntPtr CryptCATOpen(string path, uint flags, IntPtr provider, uint version, uint encoding);
    [DllImport("wintrust.dll", CharSet = CharSet.Unicode)]
    static extern IntPtr CryptCATGetMemberInfo(IntPtr catalog, string tag);
    [DllImport("wintrust.dll")]
    static extern bool CryptCATClose(IntPtr catalog);

    static string Tag(string algorithm, string file) {
        IntPtr admin;
        if (!CryptCATAdminAcquireContext2(out admin, IntPtr.Zero, algorithm, IntPtr.Zero, 0)) return null;
        try {
            using (var stream = File.OpenRead(file)) {
                uint size = 64;
                var hash = new byte[size];
                if (!CryptCATAdminCalcHashFromFileHandle2(admin, stream.SafeFileHandle.DangerousGetHandle(), ref size, hash, 0)) return null;
                return BitConverter.ToString(hash, 0, (int)size).Replace("-", "");
            }
        } finally {
            CryptCATAdminReleaseContext(admin, 0);
        }
    }

    // Whether `file` (by its catalog hash, SHA-256 or SHA-1) is a member of `catalog`.
    public static bool Contains(string catalog, string file) {
        IntPtr handle = CryptCATOpen(catalog, 2 /* CRYPTCAT_OPEN_EXISTING */, IntPtr.Zero, 0, 0);
        if (handle == new IntPtr(-1)) throw new IOException("cannot open catalog " + catalog);
        try {
            foreach (var algorithm in new[] { "SHA256", "SHA1" }) {
                var tag = Tag(algorithm, file);
                if (tag != null && CryptCATGetMemberInfo(handle, tag) != IntPtr.Zero) return true;
            }
            return false;
        } finally {
            CryptCATClose(handle);
        }
    }
}
'@

function Test-Signer($file, $certificate) {
    $signature = Get-AuthenticodeSignature -LiteralPath $file
    if ($signature.SignerCertificate.Thumbprint -ne $certificate.Thumbprint) { return "unexpected signer ($($signature.Status))" }
    if ($signature.Status -eq 'Valid') { return $null }
    if ($AllowUntrustedRoot -and $signature.Status -eq 'UnknownError') {
        $chain = [System.Security.Cryptography.X509Certificates.X509Chain]::new()
        try {
            $chain.ChainPolicy.RevocationMode = 'NoCheck'
            [void]$chain.Build($signature.SignerCertificate)
            $problems = @($chain.ChainStatus | ForEach-Object { "$($_.Status)" } | Where-Object { $_ -ne 'UntrustedRoot' })
            if ($problems.Count -eq 0) { return $null }
            return "chain: $($problems -join ', ')"
        } finally {
            $chain.Dispose()
        }
    }
    return "$($signature.Status)"
}

$certificate = [System.Security.Cryptography.X509Certificates.X509Certificate2]::new((Join-Path $assets 'FarwingDrivers.cer'))
try {
    if ($certificate.HasPrivateKey -or $certificate.Subject -ne 'CN=Farwing Drivers') {
        throw 'The driver certificate must contain only the Farwing Drivers public identity.'
    }
    # build.rs derives the installer's certificate pin from this file.
    if ((Get-Date) -lt $certificate.NotBefore -or (Get-Date) -gt $certificate.NotAfter) {
        throw 'The driver certificate is outside its validity period.'
    }
    foreach ($kind in @('input', 'display', 'audio')) {
        $name = switch ($kind) { 'input' { 'FarwingInput' } 'display' { 'FarwingDisplay' } 'audio' { 'FarwingAudio' } }
        $package = Join-Path $assets $kind
        $inf = Join-Path $package "$name.inf"
        $extension = if ($kind -eq 'audio') { 'sys' } else { 'dll' }
        $dll = Join-Path $package "$name.$extension"
        $catalog = Join-Path $package "$name.cat"
        $sourceInf = Join-Path $PSScriptRoot "$kind/$name.inf"
        if (!(Test-Path -LiteralPath $inf)) {
            Write-Output "$name is not staged; this build will not embed it."
            continue
        }
        $infMatches = if ($kind -eq 'audio') {
            [IO.File]::ReadAllText($sourceInf, [Text.Encoding]::UTF8) -ceq [IO.File]::ReadAllText($inf, [Text.Encoding]::Unicode)
        } else {
            (Get-FileHash -LiteralPath $sourceInf).Hash -eq (Get-FileHash -LiteralPath $inf).Hash
        }
        if (!$infMatches) {
            throw "$kind driver source INF differs from the staged package."
        }
        foreach ($file in @($dll, $catalog)) {
            $problem = Test-Signer $file $certificate
            if ($problem) {
                throw "Invalid driver signature or unexpected signer: $file ($problem). Run this check on the configured signing machine, or pass -AllowUntrustedRoot."
            }
            $signature = Get-AuthenticodeSignature -LiteralPath $file
            if (!$signature.TimeStamperCertificate) { Write-Warning "$name has no signature timestamp: $([IO.Path]::GetFileName($file))" }
        }
        foreach ($file in @($inf, $dll)) {
            if ($AllowUntrustedRoot) {
                if (![FarwingCatalog]::Contains($catalog, $file)) { throw "Not a member of $([IO.Path]::GetFileName($catalog)): $file" }
            } else {
                & $signTool verify /pa /c $catalog $file
                if ($LASTEXITCODE) { throw "Catalog membership/signature verification failed: $file" }
            }
        }
        Write-Output "$name verified."
    }
    Write-Output "Driver publisher: $($certificate.Subject), $($certificate.Thumbprint)"
} finally {
    $certificate.Dispose()
}
