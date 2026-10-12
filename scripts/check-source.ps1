#requires -Version 7.2
<#
.SYNOPSIS
Check formatting and run source tests without building or running an installer.
.DESCRIPTION
Requires an already installed Windows x64 MSVC Rust toolchain, rustfmt, both
x64/x86 MSVC targets, Visual C++ build tools and Windows SDK. Cargo may fetch
locked dependencies. This entry never installs tools, runs NSIS, opens a GUI,
or runs the ignored integration/performance tests. Existing output is preserved.
#>
[CmdletBinding()]
param(
    [string]$RustToolchain = 'stable',
    [string]$OutputDirectory
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (-not $IsWindows -or -not [Environment]::Is64BitProcess) {
    throw 'Use Windows x64 and PowerShell 7.2+.'
}
if ($RustToolchain -notmatch '^[A-Za-z0-9_.-]+$') { throw 'RustToolchain must name an installed rustup toolchain.' }
if ($env:CARGO_BUILD_TARGET) { throw 'Unset CARGO_BUILD_TARGET; this entry uses an x64 host and an explicit x86 bridge target.' }
$projectRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
if (-not $OutputDirectory) {
    $runName = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssfffZ') + '-' + [Guid]::NewGuid().ToString('N').Substring(0, 8)
    $OutputDirectory = "target/source-check/$runName"
}
$runRoot = [IO.Path]::GetFullPath($OutputDirectory, $projectRoot)
if (Test-Path -LiteralPath $runRoot) { throw "Choose a new OutputDirectory: $runRoot" }
$null = New-Item -ItemType Directory -Path $runRoot
$summary = [ordered]@{
    schema_version = 1; status = 'RUNNING'; started_at_utc = [DateTime]::UtcNow.ToString('o')
    rust_toolchain = $RustToolchain; output_directory = $runRoot
    installer_executed = $false; cloud_execution_claim = 'NOT_ASSERTED'
    skipped = @('50000-file boundary test', 'ignored tests including real NSIS and timing probes')
    commands = [Collections.Generic.List[object]]::new()
}
$oldToolchain = $env:RUSTUP_TOOLCHAIN
function Invoke-Check([string]$Name, [string]$Executable, [string[]]$Arguments) {
    $info = [Diagnostics.ProcessStartInfo]::new($Executable)
    $info.UseShellExecute = $false; $info.CreateNoWindow = $true
    $info.WorkingDirectory = $projectRoot
    $info.RedirectStandardOutput = $true; $info.RedirectStandardError = $true
    $info.StandardOutputEncoding = [Text.UTF8Encoding]::new($false)
    $info.StandardErrorEncoding = $info.StandardOutputEncoding
    foreach ($argument in $Arguments) { $info.ArgumentList.Add($argument) }
    $row = [ordered]@{ name = $Name; executable = $Executable; arguments = $Arguments; status = 'RUNNING' }
    $summary.commands.Add($row)
    $process = [Diagnostics.Process]::new(); $process.StartInfo = $info
    Write-Host "[$Name] starting"
    try {
        $null = $process.Start()
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()
        $process.WaitForExit()
        $stdout = $stdoutTask.GetAwaiter().GetResult(); $stderr = $stderrTask.GetAwaiter().GetResult()
        $row.exit_code = $process.ExitCode
        $row.stdout = Join-Path $runRoot "$Name.stdout.log"; $row.stderr = Join-Path $runRoot "$Name.stderr.log"
        [IO.File]::WriteAllText($row.stdout, $stdout, [Text.UTF8Encoding]::new($false))
        [IO.File]::WriteAllText($row.stderr, $stderr, [Text.UTF8Encoding]::new($false))
        $row.status = if ($process.ExitCode -eq 0) { 'PASSED' } else { 'FAILED' }
        if ($process.ExitCode -ne 0) {
            Write-Host $stdout; Write-Host $stderr
            throw "$Name exited $($process.ExitCode); inspect $runRoot"
        }
        Write-Host "[$Name] passed"
        return $stdout.Trim()
    } finally { $process.Dispose() }
}
try {
    $rustup = (Get-Command rustup.exe -CommandType Application -ErrorAction Stop | Select-Object -First 1).Source
    $installed = Invoke-Check 'installed-toolchains' $rustup @('toolchain', 'list')
    $names = @($installed -split '\r?\n' | ForEach-Object { ($_ -split '\s+')[0] })
    if ($names -notcontains $RustToolchain -and $names -notcontains "$RustToolchain-x86_64-pc-windows-msvc") {
        throw "Toolchain $RustToolchain is not installed. Provision it explicitly; no download was attempted."
    }
    $env:RUSTUP_TOOLCHAIN = $RustToolchain
    $version = Invoke-Check 'rustc-version' $rustup @('run', $RustToolchain, 'rustc', '--version', '--verbose')
    if ($version -notmatch '(?m)^host: x86_64-pc-windows-msvc\s*$') { throw 'Rust host must be x86_64-pc-windows-msvc.' }
    $summary.rustc_verbose = $version
    $targets = Invoke-Check 'installed-targets' $rustup @('target', 'list', '--installed', '--toolchain', $RustToolchain)
    foreach ($target in @('x86_64-pc-windows-msvc', 'i686-pc-windows-msvc')) {
        if (($targets -split '\r?\n') -notcontains $target) { throw "Install the $target target for $RustToolchain before retrying." }
    }
    $lockPath = Join-Path $projectRoot 'Cargo.lock'
    $summary.cargo_lock_sha256 = (Get-FileHash -LiteralPath $lockPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $null = Invoke-Check 'fmt' $rustup @('run', $RustToolchain, 'cargo', 'fmt', '--all', '--', '--check')
    $null = Invoke-Check 'source-tests' $rustup @('run', $RustToolchain, 'cargo', 'test', '--locked', '-p', 'avelin-core', '-p', 'avelin-ui', '-p', 'avelin-app', '-p', 'avelin-cli', '--features', 'avelin-app/gui', '--', '--skip', 'manifest_validates_fifty_thousand_files_and_rejects_the_next')
    $null = Invoke-Check 'bridge-tests' $rustup @('run', $RustToolchain, 'cargo', 'test', '--locked', '--target', 'i686-pc-windows-msvc', '-p', 'avelin-nsis-bridge')
    if ((Get-FileHash -LiteralPath $lockPath -Algorithm SHA256).Hash.ToLowerInvariant() -cne $summary.cargo_lock_sha256) {
        throw 'Cargo.lock changed during source checks.'
    }
    $summary.status = 'PASSED'
} catch {
    $summary.status = 'FAILED'; $summary.failure = $_.Exception.Message
    throw
} finally {
    $env:RUSTUP_TOOLCHAIN = $oldToolchain
    $summary.finished_at_utc = [DateTime]::UtcNow.ToString('o')
    [IO.File]::WriteAllText((Join-Path $runRoot 'summary.json'), ($summary | ConvertTo-Json -Depth 10), [Text.UTF8Encoding]::new($false))
}
Write-Output ($summary | ConvertTo-Json -Depth 10)
