# install.ps1 — cship Windows installer
# IMPORTANT: Update $Repo below to match your GitHub fork before distributing this script.

$ErrorActionPreference = 'Stop'

# ── Config ────────────────────────────────────────────────────────────────────
# UPDATE THIS: set to your GitHub username/repo (e.g. "jonhut/cship")
$Repo = "sixtymage/cship-windows"

$InstallDir = "$HOME\.local\bin"

# ── 1. Arch Detection ─────────────────────────────────────────────────────────
$arch = $env:PROCESSOR_ARCHITECTURE
switch ($arch) {
    'AMD64' { $Target = 'x86_64-pc-windows-msvc' }
    'ARM64' { $Target = 'aarch64-pc-windows-msvc' }
    default {
        Write-Error "Unsupported architecture: $arch"
        exit 1
    }
}
Write-Host "Detected: Windows/$arch -> target: $Target"

# ── 2. Download Binary ────────────────────────────────────────────────────────
$BinaryUrl = "https://github.com/$Repo/releases/latest/download/cship-${Target}.exe"
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
$BinaryPath = "$InstallDir\cship.exe"

Write-Host "Downloading cship from $BinaryUrl ..."
# Enforce TLS 1.2 for PowerShell 5.1 compatibility
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
Invoke-WebRequest -Uri $BinaryUrl -OutFile $BinaryPath -UseBasicParsing

if (-not (Test-Path $BinaryPath) -or (Get-Item $BinaryPath).Length -eq 0) {
    Write-Error "Error: downloaded binary is empty -- check network or release URL"
    Remove-Item -Force $BinaryPath -ErrorAction SilentlyContinue
    exit 1
}
Write-Host "Installed cship to $BinaryPath"

# ── 3. Add InstallDir to User PATH (idempotent) ───────────────────────────────
$userPath = [System.Environment]::GetEnvironmentVariable('PATH', 'User')
if (($userPath -split ';') -notcontains $InstallDir) {
    [System.Environment]::SetEnvironmentVariable('PATH', "$userPath;$InstallDir", 'User')
    Write-Host "Added $InstallDir to user PATH."
    Write-Host "Restart your terminal (or open a new session) for PATH changes to take effect."
} else {
    Write-Host "$InstallDir is already in user PATH."
}

# ── 4. cship.toml — download default config (idempotent) ─────────────────────
$CshipConfig = "$HOME\.config\cship.toml"
New-Item -ItemType Directory -Force -Path (Split-Path $CshipConfig) | Out-Null

if (Test-Path $CshipConfig) {
    Write-Host "cship.toml already exists at $CshipConfig, skipping."
} else {
    $ConfigUrl = "https://raw.githubusercontent.com/$Repo/main/cship.toml"
    Write-Host "Downloading default config from $ConfigUrl ..."
    Invoke-WebRequest -Uri $ConfigUrl -OutFile $CshipConfig -UseBasicParsing
    Write-Host "Created default cship config at $CshipConfig"
}

# ── 5. ~/.claude/settings.json — wire statusLine ─────────────────────────────
$Settings = "$HOME\.claude\settings.json"
if (-not (Test-Path $Settings)) {
    Write-Host "settings.json not found at $Settings -- skipping (Claude Code may not be installed yet)."
} else {
    try {
        $json = Get-Content $Settings -Raw | ConvertFrom-Json
        if ($null -eq $json.statusLine) {
            $statusLine = [PSCustomObject]@{ type = 'command'; command = 'cship' }
            $json | Add-Member -NotePropertyName 'statusLine' -NotePropertyValue $statusLine
            $json | ConvertTo-Json -Depth 10 | Set-Content $Settings -Encoding UTF8
            Write-Host "Added statusLine config to $Settings"
        } else {
            Write-Host """statusLine"" already set in $Settings, skipping."
        }
    } catch {
        Write-Warning "Failed to update settings.json: $_"
        Write-Host "To wire cship manually, add ""statusLine"": {""type"": ""command"", ""command"": ""cship""} to $Settings"
    }
}

# ── 6. First-run preview ──────────────────────────────────────────────────────
Write-Host ""
Write-Host "Running cship explain..."
& "$BinaryPath" explain

Write-Host ""
Write-Host "cship installation complete!"
Write-Host "If $InstallDir is not active yet, restart your terminal or run:"
Write-Host "  `$env:PATH += ';$InstallDir'"
