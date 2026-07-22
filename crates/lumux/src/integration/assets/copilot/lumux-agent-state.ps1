# installed by lumux
# managed by lumux; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# LUMUX_INTEGRATION_ID=copilot
# LUMUX_INTEGRATION_VERSION=7

param(
    [string]$Action = "",
    [int]$NativePid = 0
)

$nativePidSupplied = $PSBoundParameters.ContainsKey("NativePid")
function Resolve-LumuxBin {
    if (-not [string]::IsNullOrWhiteSpace($env:LUMUX_BIN)) {
        try {
            $candidate = Get-Item -LiteralPath $env:LUMUX_BIN -ErrorAction Stop
            if (-not $candidate.PSIsContainer) { return $candidate.FullName }
        } catch {
        }
        return $null
    }
    $command = Get-Command lumux -CommandType Application -ErrorAction SilentlyContinue
    if ($null -ne $command) { return $command.Source }
    return $null
}

$lumuxBin = Resolve-LumuxBin
if (-not [string]::IsNullOrWhiteSpace($lumuxBin)) { $env:LUMUX_BIN = $lumuxBin }
# Kernel process creation time predates hook input parsing and has a shared
# FILETIME clock, so concurrent wrapper processes remain comparable.
$sequence = ([System.Diagnostics.Process]::GetCurrentProcess().StartTime.ToUniversalTime().Ticks - 621355968000000000L) * 100L
$inputText = [Console]::In.ReadToEnd()
if ($Action -notin @("session-start", "working", "blocked", "pre-tool", "post-tool", "error", "stop", "notification", "session-end", "idle", "clear")) { exit 0 }
if ([string]::IsNullOrWhiteSpace($env:LUMUX)) { exit 0 }
if ([string]::IsNullOrWhiteSpace($env:LUMUX_PANE)) { exit 0 }
if ([string]::IsNullOrWhiteSpace($lumuxBin)) { exit 0 }

try {
    $payload = if ([string]::IsNullOrWhiteSpace($inputText)) { $null } else { $inputText | ConvertFrom-Json -ErrorAction Stop }
} catch {
    exit 0
}
if ($null -eq $payload) { exit 0 }

function First-Text {
    param([string[]]$Names)
    foreach ($name in $Names) {
        $value = $payload.$name
        if ($value -is [string] -and -not [string]::IsNullOrWhiteSpace($value)) {
            return $value
        }
    }
    return $null
}

$state = $null
switch ($Action) {
    "session-start" {
        $initial = First-Text @("initial_prompt", "initialPrompt")
        $state = if ([string]::IsNullOrWhiteSpace($initial)) { "idle" } else { "working" }
    }
    "working" { $state = "working" }
    "blocked" { $state = "blocked" }
    "pre-tool" {
        $tool = First-Text @("tool_name", "toolName")
        $state = if ($tool -in @("ask_user", "exit_plan_mode", "AskUserQuestion", "ExitPlanMode")) { "blocked" } else { "working" }
    }
    "post-tool" {
        if ((First-Text @("tool_name", "toolName")) -ne "report_intent") { $state = "working" }
    }
    "notification" {
        $notification = First-Text @("notification_type", "notificationType")
        if ($notification -in @("permission_prompt", "elicitation_dialog")) {
            $state = "blocked"
        }
    }
}

if ($Action -eq "error") {
    # Recoverable errors may be handled while the main agent keeps working.
    # Only a non-recoverable error leaves the interactive session blocked.
    $recoverable = $payload.recoverable
    if ($recoverable -is [bool] -and -not $recoverable) {
        $state = "blocked"
    }
} elseif ($Action -in @("stop", "idle")) {
    # Stop is the main-agent turn boundary. Settle even when a future provider
    # version introduces a new reason so stale busy state cannot survive it.
    $state = "idle"
} elseif ($Action -in @("session-end", "clear")) {
    # SessionEnd is emitted only when the session terminates. Its documented
    # reasons (including complete/error/timeout) all end this lifecycle.
    $state = "clear"
}

if ([string]::IsNullOrWhiteSpace($state)) { exit 0 }
Remove-Item -Path Env:LUMUX_AGENT_OWNER -ErrorAction SilentlyContinue
$owner = First-Text @("sessionId", "session_id")
if ([string]::IsNullOrWhiteSpace($owner)) { exit 0 }
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
if ($null -ne $nativeIdentity) { $owner = "${owner}@win:${nativeIdentity}" }
$env:LUMUX_AGENT_OWNER = [string]$owner
$env:LUMUX_AGENT_SEQUENCE = [string]$sequence
$env:LUMUX_AGENT_CLAIM = if ($Action -in @("session-start", "working")) { "1" } else { "0" }
try {
    & $lumuxBin report-state $state --agent copilot *> $null
} catch {
}
exit 0
