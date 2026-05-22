# RantaiClaw — native Windows installer
#
# One-liner (recommended):
#   iwr https://raw.githubusercontent.com/RantAI-dev/RantAIClaw/main/scripts/install.ps1 -UseBasicParsing | iex
#
# With options (download then run):
#   iwr https://raw.githubusercontent.com/RantAI-dev/RantAIClaw/main/scripts/install.ps1 -OutFile install.ps1
#   .\install.ps1 -InstallDir "$HOME\bin" -Onboard
#
# Mirrors scripts/bootstrap.sh for native Windows (no WSL, no Git Bash needed).
# Detects architecture, downloads the published prebuilt binary, verifies its
# SHA-256 checksum against SHA256SUMS, extracts to a stable location, and
# amends the *user* PATH via the registry (NOT `setx`, which truncates at
# 1024 chars). Broadcasts WM_SETTINGCHANGE so Explorer-spawned tools notice
# the new PATH without a logoff.
#
# Environment variables (mirror bootstrap.sh names):
#   RANTAICLAW_INSTALL_DIR          Override install directory
#   RANTAICLAW_RELEASE_BASE_URL     Override release-asset base URL
#   RANTAICLAW_AUTO_MODIFY_PATH=1   Always amend user PATH (no prompt)
#   RANTAICLAW_NO_MODIFY_PATH=1     Never amend user PATH
#   VERIFY_CHECKSUM=false           Skip SHA256 verification (offline / mirror)
#   RANTAICLAW_API_KEY              Used with -Onboard
#   RANTAICLAW_PROVIDER             Used with -Onboard (default: openrouter)
#   RANTAICLAW_MODEL                Used with -Onboard

[CmdletBinding()]
param(
    [string]$InstallDir,
    [string]$ReleaseBaseUrl,
    [string]$Version = 'latest',
    [switch]$NoVerifyChecksum,
    [switch]$NoModifyPath,
    [switch]$ForceModifyPath,
    # As of v0.6.52-alpha the installer ALWAYS runs `rantaiclaw setup --force`
    # at the end (the "full" wizard: provider, approvals, channels, persona,
    # skills, MCP). Use -SkipSetup or $env:RANTAICLAW_SKIP_SETUP=1 to disable.
    [switch]$SkipSetup,
    # Legacy: switches the post-install step from `setup --force` to the
    # older `onboard` flow.
    [switch]$Onboard,
    [switch]$Interactive,
    [string]$ApiKey,
    [string]$Provider,
    [string]$Model
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# --- TLS: GitHub release downloads require TLS 1.2 on older Windows hosts ---
try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
} catch {
    # Best-effort: newer .NET defaults already include TLS 1.2.
}

# --- env-var fallbacks (mirror bootstrap.sh) ---
if (-not $InstallDir       -and $env:RANTAICLAW_INSTALL_DIR)        { $InstallDir       = $env:RANTAICLAW_INSTALL_DIR }
if (-not $ReleaseBaseUrl   -and $env:RANTAICLAW_RELEASE_BASE_URL)   { $ReleaseBaseUrl   = $env:RANTAICLAW_RELEASE_BASE_URL }
if (-not $ApiKey           -and $env:RANTAICLAW_API_KEY)            { $ApiKey           = $env:RANTAICLAW_API_KEY }
if (-not $Provider         -and $env:RANTAICLAW_PROVIDER)           { $Provider         = $env:RANTAICLAW_PROVIDER }
if (-not $Model            -and $env:RANTAICLAW_MODEL)              { $Model            = $env:RANTAICLAW_MODEL }
if (-not $NoModifyPath     -and $env:RANTAICLAW_NO_MODIFY_PATH    -eq '1') { $NoModifyPath     = $true }
if (-not $ForceModifyPath  -and $env:RANTAICLAW_AUTO_MODIFY_PATH  -eq '1') { $ForceModifyPath  = $true }
if (-not $NoVerifyChecksum -and $env:VERIFY_CHECKSUM -eq 'false')          { $NoVerifyChecksum = $true }
if (-not $SkipSetup        -and $env:RANTAICLAW_SKIP_SETUP        -eq '1') { $SkipSetup        = $true }

if (-not $ReleaseBaseUrl) {
    $ReleaseBaseUrl = if ($Version -eq 'latest') {
        'https://github.com/RantAI-dev/RantAIClaw/releases/latest/download'
    } else {
        "https://github.com/RantAI-dev/RantAIClaw/releases/download/$Version"
    }
}

# --- UX helpers (color-aware; respects $env:NO_COLOR) ---
$script:UseColor = -not $env:NO_COLOR -and $Host.UI.RawUI -ne $null

function Write-Info    ([string]$m) { if ($script:UseColor) { Write-Host "-> $m" -ForegroundColor Cyan }   else { Write-Host "-> $m" } }
function Write-Ok      ([string]$m) { if ($script:UseColor) { Write-Host "OK  $m" -ForegroundColor Green } else { Write-Host "OK  $m" } }
function Write-Warn    ([string]$m) { if ($script:UseColor) { Write-Host "!!  $m" -ForegroundColor Yellow } else { Write-Host "!!  $m" } }
function Write-Err     ([string]$m) { if ($script:UseColor) { Write-Host "XX  $m" -ForegroundColor Red }    else { Write-Host "XX  $m" } }

function Write-Banner {
    $bar = '-' * 57
    Write-Host ''
    if ($script:UseColor) {
        Write-Host "+$bar+" -ForegroundColor Magenta
        Write-Host '|            RantaiClaw Installer (Windows)              |' -ForegroundColor Magenta
        Write-Host "+$bar+" -ForegroundColor Magenta
    } else {
        Write-Host "+$bar+"
        Write-Host '|            RantaiClaw Installer (Windows)              |'
        Write-Host "+$bar+"
    }
    Write-Host ''
}

function Write-SuccessBanner ([string[]]$NextSteps) {
    $bar = '-' * 57
    Write-Host ''
    if ($script:UseColor) {
        Write-Host "+$bar+" -ForegroundColor Green
        Write-Host '|            Installation Complete!                      |' -ForegroundColor Green
        Write-Host "+$bar+" -ForegroundColor Green
    } else {
        Write-Host "+$bar+"
        Write-Host '|            Installation Complete!                      |'
        Write-Host "+$bar+"
    }
    if ($NextSteps.Count -gt 0) {
        Write-Host ''
        Write-Host '-> Next steps:'
        foreach ($line in $NextSteps) { Write-Host "   * $line" }
    }
    Write-Host ''
}

function Write-ActionRequired ([string]$Title, [string[]]$Lines) {
    $bar = '-' * 57
    Write-Host ''
    if ($script:UseColor) {
        Write-Host "+$bar+" -ForegroundColor Yellow
        Write-Host ("|  ACTION REQUIRED: {0,-37}|" -f $Title) -ForegroundColor Yellow
        Write-Host "+$bar+" -ForegroundColor Yellow
    } else {
        Write-Host "+$bar+"
        Write-Host ("|  ACTION REQUIRED: {0,-37}|" -f $Title)
        Write-Host "+$bar+"
    }
    if ($Lines.Count -gt 0) {
        Write-Host ''
        foreach ($line in $Lines) { Write-Host "  $line" }
    }
    Write-Host ''
}

# --- Architecture detection ---
function Get-ReleaseTarget {
    $arch = [Environment]::Is64BitOperatingSystem
    $procArch = $env:PROCESSOR_ARCHITECTURE
    if ($procArch -eq 'ARM64') {
        throw "Windows ARM64 is not yet a published target. Build from source: https://github.com/RantAI-dev/RantAIClaw/blob/main/docs/install.md#option-3---build-from-source-msvc"
    }
    if (-not $arch) {
        throw "32-bit Windows is not supported. Use a 64-bit Windows 10/11."
    }
    return 'x86_64-pc-windows-msvc'
}

# --- Resolve install dir ---
# Priority: -InstallDir / $env:RANTAICLAW_INSTALL_DIR -> %LOCALAPPDATA%\Programs\rantaiclaw
# %LOCALAPPDATA%\Programs is the conventional Windows location for user-local
# programs (used by VS Code, Slack, Discord). Doesn't require admin.
function Resolve-InstallDir {
    if ($InstallDir) { return $InstallDir }
    $local = $env:LOCALAPPDATA
    if (-not $local) { $local = Join-Path $env:USERPROFILE 'AppData\Local' }
    return (Join-Path $local 'Programs\rantaiclaw')
}

# --- Download a URL to a path; surface a clean error on 404 (mid-publish race) ---
function Invoke-Download ([string]$Url, [string]$OutFile) {
    Write-Info "Downloading $Url"
    try {
        Invoke-WebRequest -Uri $Url -OutFile $OutFile -UseBasicParsing -ErrorAction Stop
    } catch {
        throw "Download failed: $Url  ($($_.Exception.Message))"
    }
}

# --- SHA256 verify against SHA256SUMS (same format as bootstrap.sh) ---
function Test-Checksum ([string]$ArchivePath, [string]$ChecksumsPath, [string]$ArchiveBasename) {
    if (-not (Test-Path $ChecksumsPath)) {
        throw "SHA256SUMS not found at $ChecksumsPath"
    }
    $expected = $null
    foreach ($line in Get-Content $ChecksumsPath) {
        $parts = $line -split '\s+', 2
        if ($parts.Count -lt 2) { continue }
        $hash = $parts[0]
        $name = ($parts[1] -replace '^\./', '').Trim()
        if ($name -eq $ArchiveBasename) { $expected = $hash; break }
    }
    if (-not $expected) {
        throw "SHA256SUMS has no entry for $ArchiveBasename (release artifacts may be mid-publish; retry in a minute)"
    }
    $actual = (Get-FileHash -Path $ArchivePath -Algorithm SHA256).Hash.ToLowerInvariant()
    $expectedLc = $expected.ToLowerInvariant()
    if ($actual -ne $expectedLc) {
        throw "Checksum mismatch for $ArchiveBasename.`n  expected: $expectedLc`n  actual:   $actual`nRefusing to install a tampered or corrupt archive."
    }
    Write-Ok "Checksum verified ($expectedLc)"
}

# --- PATH amendment via registry (not setx — setx truncates >1024 chars) ---
function Test-PathContains ([string]$Dir) {
    $sep = ';'
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not $userPath) { $userPath = '' }
    foreach ($p in $userPath -split $sep) {
        if ($p -and ($p.TrimEnd('\') -ieq $Dir.TrimEnd('\'))) { return $true }
    }
    foreach ($p in $env:Path -split $sep) {
        if ($p -and ($p.TrimEnd('\') -ieq $Dir.TrimEnd('\'))) { return $true }
    }
    return $false
}

function Add-ToUserPath ([string]$Dir) {
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not $userPath) { $userPath = '' }
    $new = if ($userPath) { "$Dir;$userPath" } else { $Dir }
    [Environment]::SetEnvironmentVariable('Path', $new, 'User')
    # Also update the *current* process so the user can use rantaiclaw
    # immediately in this same session (no new window required for the
    # PowerShell that ran the installer).
    $env:Path = "$Dir;$env:Path"
    # Broadcast WM_SETTINGCHANGE so Explorer-spawned tools (new cmd, Run dialog)
    # pick up the new PATH without a logoff.
    try {
        $sig = @"
[DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Auto)]
public static extern IntPtr SendMessageTimeout(IntPtr hWnd, uint Msg, UIntPtr wParam, string lParam, uint fuFlags, uint uTimeout, out UIntPtr lpdwResult);
"@
        $type = Add-Type -MemberDefinition $sig -Name 'Win32SendMessageTimeout' -Namespace RantaiClaw -PassThru -ErrorAction Stop
        $HWND_BROADCAST = [IntPtr]0xFFFF
        $WM_SETTINGCHANGE = 0x1A
        $SMTO_ABORTIFHUNG = 0x2
        [UIntPtr]$result = [UIntPtr]::Zero
        [void]$type::SendMessageTimeout($HWND_BROADCAST, $WM_SETTINGCHANGE, [UIntPtr]::Zero, 'Environment', $SMTO_ABORTIFHUNG, 5000, [ref]$result)
    } catch {
        # Non-fatal: existing apps will still pick up the new PATH on next launch.
    }
}

# =============================================================================
# Main
# =============================================================================
Write-Banner

$target = Get-ReleaseTarget
$archiveBase = "rantaiclaw-$target.zip"
$archiveUrl = "$ReleaseBaseUrl/$archiveBase"
$checksumsUrl = "$ReleaseBaseUrl/SHA256SUMS"

$installDirResolved = Resolve-InstallDir
$tempDir = Join-Path $env:TEMP "rantaiclaw-install-$(Get-Random)"
New-Item -ItemType Directory -Force -Path $tempDir | Out-Null

try {
    $archivePath   = Join-Path $tempDir $archiveBase
    $checksumsPath = Join-Path $tempDir 'SHA256SUMS'

    Invoke-Download -Url $archiveUrl -OutFile $archivePath

    if ($NoVerifyChecksum) {
        Write-Warn 'Skipping SHA256 verification (NoVerifyChecksum / VERIFY_CHECKSUM=false)'
    } else {
        Invoke-Download -Url $checksumsUrl -OutFile $checksumsPath
        Test-Checksum -ArchivePath $archivePath -ChecksumsPath $checksumsPath -ArchiveBasename $archiveBase
    }

    # --- Extract ---
    Write-Info "Extracting to $installDirResolved"
    if (-not (Test-Path $installDirResolved)) {
        New-Item -ItemType Directory -Force -Path $installDirResolved | Out-Null
    }
    Expand-Archive -Path $archivePath -DestinationPath $tempDir -Force
    $exe = Get-ChildItem -Path $tempDir -Recurse -Filter 'rantaiclaw.exe' | Select-Object -First 1
    if (-not $exe) { throw "Archive did not contain rantaiclaw.exe" }
    $finalExe = Join-Path $installDirResolved 'rantaiclaw.exe'
    Copy-Item -Path $exe.FullName -Destination $finalExe -Force
    Write-Ok "Installed rantaiclaw.exe to $finalExe"

    $versionLine = ''
    try { $versionLine = & $finalExe --version 2>$null | Select-Object -First 1 } catch {}
    if ($versionLine) { Write-Info "Version: $versionLine" }

    # --- PATH ---
    # Decision tree:
    #   NoModifyPath           -> never amend
    #   ForceModifyPath        -> always amend, no prompt
    #   Piped (iwr | iex)      -> always amend (no console for Read-Host;
    #                             matches what users expect from a one-liner
    #                             installer like rustup-init / oh-my-posh)
    #   Interactive .ps1 run   -> prompt [Y/n], default Y
    $pathAlreadyOk = Test-PathContains -Dir $installDirResolved
    $modifiedPath = $false
    if (-not $pathAlreadyOk) {
        if ($NoModifyPath) {
            Write-Warn "$installDirResolved is not on PATH (skipped: NoModifyPath)"
        } else {
            $shouldModify = $true
            $isPiped = [Console]::IsInputRedirected
            if (-not $ForceModifyPath -and -not $isPiped) {
                $resp = Read-Host "Add $installDirResolved to your User PATH now? [Y/n]"
                $shouldModify = ($resp -eq '' -or $resp -match '^[Yy]')
            }
            if ($shouldModify) {
                Add-ToUserPath -Dir $installDirResolved
                $modifiedPath = $true
                Write-Ok "Added $installDirResolved to User PATH"
            } else {
                Write-Warn "Skipped PATH amendment"
            }
        }
    }

    # --- Post-install configuration ---
    # Default: `rantaiclaw setup --force` (the "full" wizard) unless the user
    # passed -SkipSetup / $env:RANTAICLAW_SKIP_SETUP=1, or opted into the
    # legacy `onboard` path via -Onboard.
    $ranPostInstall = $false
    if ($Onboard) {
        $onboardArgs = @('onboard')
        if ($Interactive) {
            $onboardArgs += '--interactive'
        } else {
            if ($ApiKey)   { $onboardArgs += '--api-key';  $onboardArgs += $ApiKey }
            if ($Provider) { $onboardArgs += '--provider'; $onboardArgs += $Provider }
            if ($Model)    { $onboardArgs += '--model';    $onboardArgs += $Model }
        }
        Write-Info "Running: rantaiclaw.exe $($onboardArgs -join ' ')"
        & $finalExe @onboardArgs
        $ranPostInstall = $true
    } elseif (-not $SkipSetup) {
        # Always run the full setup wizard at install end. When piped via
        # `iwr | iex`, stdin is redirected but the wizard expects interactive
        # input — Windows doesn't have a clean /dev/tty fallback, so when
        # input is redirected we print a clear reminder instead of crashing.
        if ([Console]::IsInputRedirected) {
            Write-Warn 'No interactive console (piped install).'
            Write-Warn 'Run the full setup wizard manually in a new PowerShell window:'
            Write-Warn ''
            Write-Warn '    rantaiclaw.exe setup --force'
        } else {
            Write-Info 'Running guided setup (rantaiclaw setup --force)'
            Write-Info 'Pass -SkipSetup or $env:RANTAICLAW_SKIP_SETUP=1 to disable this.'
            try {
                & $finalExe setup --force
                $ranPostInstall = $true
            } catch {
                Write-Warn "Setup exited non-zero: $($_.Exception.Message)"
                Write-Warn 'Re-run anytime with: rantaiclaw.exe setup --force'
            }
        }
    }

    # --- Success banner ---
    $nextSteps = @(
        'rantaiclaw chat       — start an interactive session'
        'rantaiclaw agent      — run the autonomous agent loop'
        'rantaiclaw status     — verify installation'
    )
    if (-not $ranPostInstall) {
        $nextSteps += 'rantaiclaw setup --force   — guided wizard (provider, approvals, channels, persona, skills, MCP)'
        $nextSteps += 'rantaiclaw doctor          — verify the install once configured'
    } else {
        $nextSteps += 'rantaiclaw doctor          — verify the install at any time'
    }
    Write-SuccessBanner -NextSteps $nextSteps

    # --- Bold post-install reminder ---
    # Even when we amended PATH, *other* shells / cmd.exe / VS Code terminals
    # opened before the install will not see the new PATH until restarted.
    if (-not $pathAlreadyOk) {
        if ($modifiedPath) {
            Write-ActionRequired -Title 'Open a NEW terminal' -Lines @(
                "rantaiclaw is on your PATH in THIS PowerShell session,",
                "but OTHER terminals (cmd, VS Code, Git Bash, new tabs)",
                "still cache the old PATH until you close and reopen them.",
                "",
                "Verify in a new PowerShell window:",
                "",
                "    rantaiclaw.exe --version"
            )
        } else {
            Write-ActionRequired -Title 'rantaiclaw not on PATH' -Lines @(
                "The install directory was not added to your PATH.",
                "",
                "Add it now (run in PowerShell, then open a new terminal):",
                "",
                "    [Environment]::SetEnvironmentVariable('Path',",
                "      '$installDirResolved;' + [Environment]::GetEnvironmentVariable('Path','User'),",
                "      'User')",
                "",
                "Then verify:",
                "",
                "    rantaiclaw.exe --version"
            )
        }
    } else {
        Write-Info 'PATH was already configured; rantaiclaw is ready to use.'
    }
}
finally {
    if (Test-Path $tempDir) { Remove-Item -Recurse -Force $tempDir -ErrorAction SilentlyContinue }
}
