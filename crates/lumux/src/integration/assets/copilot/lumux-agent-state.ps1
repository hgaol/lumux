# installed by lumux
# managed by lumux; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# LUMUX_INTEGRATION_ID=copilot
# LUMUX_INTEGRATION_VERSION=5

param(
    [string]$Action = "",
    [int]$NativePid = 0
)

$nativePidSupplied = $PSBoundParameters.ContainsKey("NativePid")
# Kernel process creation time predates hook input parsing and has a shared
# FILETIME clock, so concurrent wrapper processes remain comparable.
$sequence = ([System.Diagnostics.Process]::GetCurrentProcess().StartTime.ToUniversalTime().Ticks - 621355968000000000L) * 100L
$inputText = [Console]::In.ReadToEnd()
if ($Action -notin @("session-start", "working", "blocked", "pre-tool", "post-tool", "stop", "notification", "session-end", "idle", "clear")) { exit 0 }
if ([string]::IsNullOrWhiteSpace($env:LUMUX)) { exit 0 }
if ([string]::IsNullOrWhiteSpace($env:LUMUX_PANE)) { exit 0 }
if ($null -eq (Get-Command lumux -ErrorAction SilentlyContinue)) { exit 0 }

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
        } elseif ($notification -eq "agent_idle") {
            $state = "idle"
        }
    }
}

if ($Action -in @("stop", "idle")) {
    $stopReason = First-Text @("stop_reason", "stopReason")
    if ([string]::IsNullOrWhiteSpace($stopReason) -or $stopReason -eq "end_turn") {
        $state = "idle"
    }
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
    & lumux report-state $state --agent copilot *> $null
} catch {
}
exit 0
