#!/bin/bash
# convergence-check.sh — Post-iteration convergence detector
# Called by loop.sh after git push. Analyzes the last commit to determine
# if the iteration was productive or a no-op.
#
# Exit code 0 = continue looping (productive)
# Exit code 1 = converged, stop the loop
#
# Configurable: CONVERGENCE_THRESHOLD (default 3)
set -euo pipefail

THRESHOLD="${CONVERGENCE_THRESHOLD:-3}"
COUNTER_FILE="${CLAUDE_PROJECT_DIR:-.}/.claude/convergence-count"
LOG_FILE="${CLAUDE_PROJECT_DIR:-.}/.claude/ralph-activity.log"
TIMESTAMP=$(date '+%H:%M:%S')

# Initialize counter file if missing
[ -f "$COUNTER_FILE" ] || echo "0" > "$COUNTER_FILE"

# --- Gather signals from last commit ---

# 1. Commit message
COMMIT_MSG=$(git log -1 --format='%s' 2>/dev/null || echo "")

# 2. STOP signal — immediate exit, no counting
if echo "$COMMIT_MSG" | grep -qiE '^STOP:'; then
    echo "🛑 STOP signal detected in commit message — terminating loop"
    echo "$TIMESTAMP │ CONV │ STOP signal: $COMMIT_MSG │ 🛑" >> "$LOG_FILE"
    exit 1
fi

# 3. Files changed and insertions
STAT=$(git show --stat --format='' HEAD 2>/dev/null || echo "")
FILES_CHANGED=$(echo "$STAT" | tail -1 | grep -oE '[0-9]+ files? changed' | grep -oE '[0-9]+' || echo "0")
INSERTIONS=$(echo "$STAT" | tail -1 | grep -oE '[0-9]+ insertions?' | grep -oE '[0-9]+' || echo "0")

# 4. Which files were touched (excluding metadata-only files)
CHANGED_FILES=$(git show --name-only --format='' HEAD 2>/dev/null || echo "")
SOURCE_CHANGES=$(echo "$CHANGED_FILES" | grep -cvE '^(IMPLEMENTATION_PLAN\.md|AGENTS\.md|\.claude/|$)' || echo "0")

# --- Score the iteration ---

NOOP=false

# Pattern 1: Commit message matches known convergence patterns
if echo "$COMMIT_MSG" | grep -qE '^[0-9]+(st|nd|rd|th) audit:.*(clean|no.*(code )?change)'; then
    NOOP=true
fi

# Pattern 2: Very small change to metadata-only files
if [ "$FILES_CHANGED" -le 1 ] && [ "$INSERTIONS" -le 5 ] && [ "$SOURCE_CHANGES" -eq 0 ]; then
    NOOP=true
fi

# Pattern 3: No commit at all (nothing changed)
if [ -z "$COMMIT_MSG" ] || [ "$FILES_CHANGED" -eq 0 ]; then
    NOOP=true
fi

# --- Update counter and decide ---

CURRENT=$(cat "$COUNTER_FILE")

if [ "$NOOP" = true ]; then
    CURRENT=$((CURRENT + 1))
    echo "$CURRENT" > "$COUNTER_FILE"

    if [ "$CURRENT" -ge "$THRESHOLD" ]; then
        echo "🛑 CONVERGED ($CURRENT/$THRESHOLD) — stopping loop"
        echo "$TIMESTAMP │ CONV │ CONVERGED ($CURRENT/$THRESHOLD): $COMMIT_MSG │ 🛑" >> "$LOG_FILE"
        exit 1
    else
        echo "⏳ NO-OP ($CURRENT/$THRESHOLD): $COMMIT_MSG"
        echo "$TIMESTAMP │ CONV │ NO-OP ($CURRENT/$THRESHOLD): $COMMIT_MSG │ ⏳" >> "$LOG_FILE"
        exit 0
    fi
else
    echo "0" > "$COUNTER_FILE"
    echo "⚡ PRODUCTIVE (reset): $COMMIT_MSG"
    echo "$TIMESTAMP │ CONV │ PRODUCTIVE (reset): $COMMIT_MSG │ ⚡" >> "$LOG_FILE"
    exit 0
fi
