#!/usr/bin/env pwsh
# Build the DM-MC02 firmware and report one unambiguous result.
#
# Usage:
#   powershell -NoProfile -ExecutionPolicy Bypass -File .\build-check.ps1
#   pwsh -NoProfile -File .\build-check.ps1
#
# Exit code:
#   0  = cargo build --locked succeeded and the ELF exists
#   *  = failure (cargo's exit code, or 1 if the ELF is missing)

$ErrorActionPreference = 'Stop'

$repo = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $repo

$log = Join-Path $repo 'build.log'
$elf = Join-Path $repo 'target\thumbv7em-none-eabihf\debug\dm-mc02-cmsis-dap'

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Host 'BUILD FAILED: cargo is not on PATH'
    exit 127
}

Write-Host '==> cargo build --locked'
# cmd.exe performs the redirection so PowerShell does not turn cargo's stderr
# progress lines into terminating error records.
$command = 'cargo build --locked > "{0}" 2>&1' -f $log
& cmd.exe /d /s /c $command
$code = $LASTEXITCODE

if (Test-Path $log) {
    Get-Content $log | ForEach-Object { Write-Host $_ }
}

if ($code -ne 0) {
    Write-Host ''
    Write-Host "BUILD FAILED (cargo exit code $code)"
    exit $code
}

if (-not (Test-Path $elf)) {
    Write-Host ''
    Write-Host "BUILD FAILED (firmware ELF not found: $elf)"
    exit 1
}

$size = (Get-Item $elf).Length
Write-Host ''
Write-Host 'BUILD SUCCESS'
Write-Host "ELF:  $elf"
Write-Host "Size: $size bytes"
exit 0
