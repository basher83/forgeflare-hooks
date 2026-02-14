#!/bin/bash
# ralph-activity.sh — PostToolUse + PostToolUseFailure hook for Bash, Write, Edit
# Logs a one-line activity entry to ralph-activity.log for monitoring.
# Tail this file in a tmux pane: tail -f .claude/ralph-activity.log
#
# PostToolUse fires on success only. PostToolUseFailure fires on failure.
# Bash has no numeric exit code in the hook input — success/failure is
# determined by which event fires.
set -euo pipefail

input=$(cat)
tool_name=$(echo "$input" | jq -r '.tool_name // "?"')
event=$(echo "$input" | jq -r '.hook_event_name // "PostToolUse"')
timestamp=$(date '+%H:%M:%S')

LOG_FILE="${CLAUDE_PROJECT_DIR:-.}/.claude/ralph-activity.log"

case "$tool_name" in
  Bash)
    cmd=$(echo "$input" | jq -r '.tool_input.command // ""' | head -1 | cut -c1-100)
    if [ "$event" = "PostToolUseFailure" ]; then
      icon="✗"
    else
      icon="✓"
    fi
    echo "$timestamp │ BASH │ $cmd │ $icon" >> "$LOG_FILE"
    ;;
  Write)
    path=$(echo "$input" | jq -r '.tool_input.file_path // ""' | sed "s|$CLAUDE_PROJECT_DIR/||")
    echo "$timestamp │ WRITE │ $path │ ✓" >> "$LOG_FILE"
    ;;
  Edit)
    path=$(echo "$input" | jq -r '.tool_input.file_path // ""' | sed "s|$CLAUDE_PROJECT_DIR/||")
    echo "$timestamp │ EDIT  │ $path │ ✓" >> "$LOG_FILE"
    ;;
esac

exit 0
