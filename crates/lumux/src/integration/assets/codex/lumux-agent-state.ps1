# installed by lumux
# managed by lumux; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# LUMUX_INTEGRATION_ID=codex
# LUMUX_INTEGRATION_VERSION=4

param(
    [string]$State = "",
    [int]$NativePid = 0
)

$nativePidSupplied = $PSBoundParameters.ContainsKey("NativePid")
$ErrorActionPreference = "SilentlyContinue"

# Internal detached watcher mode. The initial SessionStart hook records the
# native Codex process creation time, so pid reuse cannot keep this watcher on a
# replacement process or let it clear a replacement provider lifecycle.
if ($State -eq "watch") {
    if ([string]::IsNullOrWhiteSpace($env:LUMUX) -or
        [string]::IsNullOrWhiteSpace($env:LUMUX_PANE) -or
        [string]::IsNullOrWhiteSpace($env:LUMUX_AGENT_OWNER) -or
        [string]::IsNullOrWhiteSpace($env:LUMUX_CODEX_WATCH_PID) -or
        [string]::IsNullOrWhiteSpace($env:LUMUX_CODEX_WATCH_IDENTITY)) {
        exit 0
    }
    try {
        $watchPid = [int]$env:LUMUX_CODEX_WATCH_PID
        $watchIdentity = [long]$env:LUMUX_CODEX_WATCH_IDENTITY
    } catch {
        exit 0
    }
    while ($true) {
        $sameProcess = $false
        try {
            $candidate = Get-Process -Id $watchPid -ErrorAction Stop
            $sameProcess = $candidate.StartTime.ToUniversalTime().Ticks -eq $watchIdentity
        } catch {
            $sameProcess = $false
        }
        if (-not $sameProcess) { break }
        Start-Sleep -Milliseconds 250
    }
    if ($null -eq (Get-Command lumux -ErrorAction SilentlyContinue)) { exit 0 }
    $env:LUMUX_AGENT_SEQUENCE = [string](([DateTime]::UtcNow.Ticks - 621355968000000000L) * 100L)
    $env:LUMUX_AGENT_CLAIM = "0"
    Remove-Item -Path Env:LUMUX_CODEX_WATCH_PID -ErrorAction SilentlyContinue
    Remove-Item -Path Env:LUMUX_CODEX_WATCH_IDENTITY -ErrorAction SilentlyContinue
    try {
        & lumux report-state clear --agent codex *> $null
    } catch {
    }
    exit 0
}

# Kernel process creation time predates hook input parsing and has a shared
# FILETIME clock, so concurrent wrapper processes remain comparable.
$sequence = ([System.Diagnostics.Process]::GetCurrentProcess().StartTime.ToUniversalTime().Ticks - 621355968000000000L) * 100L
$inputText = [Console]::In.ReadToEnd()
if ($State -notin @("idle", "working", "blocked")) { exit 0 }
if ([string]::IsNullOrWhiteSpace($env:LUMUX)) { exit 0 }
if ([string]::IsNullOrWhiteSpace($env:LUMUX_PANE)) { exit 0 }
if ($null -eq (Get-Command lumux -ErrorAction SilentlyContinue)) { exit 0 }

try {
    $payload = if ([string]::IsNullOrWhiteSpace($inputText)) { $null } else { $inputText | ConvertFrom-Json -ErrorAction Stop }
} catch {
    exit 0
}
if ($null -eq $payload) { exit 0 }
if ($payload.session_id -isnot [string] -or [string]::IsNullOrWhiteSpace($payload.session_id)) { exit 0 }

$nativeIdentity = $null
if ($NativePid -gt 0) {
    try {
        $native = Get-Process -Id $NativePid -ErrorAction Stop
        $nativeIdentity = $native.StartTime.ToUniversalTime().Ticks
    } catch {
        $nativeIdentity = $null
    }
}
# Installed commands always supply the native pid. Never downgrade a generated
# owner to its bare session id when that process generation cannot be proven.
if ($nativePidSupplied -and $null -eq $nativeIdentity) { exit 0 }
$owner = [string]$payload.session_id
if ($null -ne $nativeIdentity) { $owner = "${owner}@win:${nativeIdentity}" }
Remove-Item -Path Env:LUMUX_AGENT_OWNER -ErrorAction SilentlyContinue
$env:LUMUX_AGENT_OWNER = $owner
$env:LUMUX_AGENT_SEQUENCE = [string]$sequence
$env:LUMUX_AGENT_CLAIM = if ($payload.hook_event_name -in @("SessionStart", "UserPromptSubmit")) { "1" } else { "0" }

try {
    & lumux report-state $State --agent codex *> $null
} catch {
}

# Codex has no documented SessionEnd hook. SessionStart's command passes the pid
# of the long-lived native Codex process; start a handle-free child that outlives
# this hook and reports clear when that exact process exits.
if ($payload.hook_event_name -eq "SessionStart" -and $NativePid -gt 0 -and $null -ne $nativeIdentity) {
    try {
        $powershell = (Get-Process -Id $PID -ErrorAction Stop).Path
        $startInfo = New-Object System.Diagnostics.ProcessStartInfo
        $startInfo.FileName = $powershell
        $startInfo.Arguments = "-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File `"$PSCommandPath`" watch"
        $startInfo.UseShellExecute = $false
        $startInfo.CreateNoWindow = $true
        $startInfo.RedirectStandardInput = $true
        $startInfo.RedirectStandardOutput = $true
        $startInfo.RedirectStandardError = $true
        $startInfo.EnvironmentVariables["LUMUX_AGENT_OWNER"] = $owner
        $startInfo.EnvironmentVariables["LUMUX_AGENT_CLAIM"] = "0"
        $startInfo.EnvironmentVariables["LUMUX_CODEX_WATCH_PID"] = [string]$NativePid
        $startInfo.EnvironmentVariables["LUMUX_CODEX_WATCH_IDENTITY"] = [string]$nativeIdentity
        $watcher = [System.Diagnostics.Process]::Start($startInfo)
        if ($null -ne $watcher) { $watcher.Dispose() }
    } catch {
    }
}
exit 0
