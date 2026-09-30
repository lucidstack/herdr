# installed by herdr
# managed by herdr; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# HERDR_INTEGRATION_ID=claude
# HERDR_INTEGRATION_VERSION=10

param([string]$Action = "")

if ($Action -ne "session" -and $Action -ne "notes") { exit 0 }
if ($env:HERDR_ENV -ne "1") { exit 0 }
if ([string]::IsNullOrWhiteSpace($env:HERDR_PANE_ID)) { exit 0 }

$inputText = [Console]::In.ReadToEnd()
try {
    $payload = if ([string]::IsNullOrWhiteSpace($inputText)) { $null } else { $inputText | ConvertFrom-Json }
} catch {
    exit 0
}

$propertyNames = @($payload.PSObject.Properties.Name)
if ((Test-Path Env:CURSOR_VERSION) -or $propertyNames -ccontains "cursor_version") { exit 0 }
if (-not ($propertyNames -ccontains "hook_event_name") -or $payload.hook_event_name -isnot [string]) { exit 0 }
if (-not [string]::IsNullOrWhiteSpace($payload.agent_id)) { exit 0 }
$herdr = if ([string]::IsNullOrWhiteSpace($env:HERDR_BIN_PATH)) { "herdr" } else { $env:HERDR_BIN_PATH }

if ($Action -eq "notes") {
    # Notes about what Herdr did outside this session. Before a prompt they join its
    # context; after a tool call they steer the running turn.
    if ($payload.hook_event_name -cne "UserPromptSubmit" -and $payload.hook_event_name -cne "PostToolUse") { exit 0 }
    try {
        $response = (& $herdr agent notes take $env:HERDR_PANE_ID 2>$null | Out-String) | ConvertFrom-Json
        $lines = @($response.result.notes | Where-Object { $_.text -is [string] -and $_.text } | ForEach-Object { $_.text })
        if ($lines.Count -gt 0) {
            @{
                hookSpecificOutput = @{
                    hookEventName = $payload.hook_event_name
                    additionalContext = ($lines -join "`n")
                }
            } | ConvertTo-Json -Compress -Depth 4
        }
    } catch {
    }
    exit 0
}

if ($payload.hook_event_name -cne "SessionStart") { exit 0 }

$sessionId = $payload.session_id
if ([string]::IsNullOrWhiteSpace($sessionId)) { exit 0 }

$seq = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()

try {
    $args = @(
        "pane",
        "report-agent-session",
        $env:HERDR_PANE_ID,
        "--source",
        "herdr:claude",
        "--agent",
        "claude",
        "--seq",
        "$seq",
        "--agent-session-id",
        "$sessionId"
    )
    if ($payload.transcript_path -is [string] -and -not [string]::IsNullOrWhiteSpace($payload.transcript_path)) {
        $args += @("--agent-session-path", "$($payload.transcript_path)")
    }
    if ($payload.hook_event_name -eq "SessionStart" -and $payload.source -is [string] -and -not [string]::IsNullOrWhiteSpace($payload.source)) {
        $args += @("--session-start-source", "$($payload.source)")
    }
    & $herdr @args 2>$null | Out-Null
} catch {
}
