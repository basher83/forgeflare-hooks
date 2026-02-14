#!/bin/bash
# ralph-guard.sh — PreToolUse hook for Bash
# Blocks destructive commands that could damage the system or repo.
# Returns deny with reason; otherwise allows silently.
set -euo pipefail

input=$(cat)
command=$(echo "$input" | jq -r '.tool_input.command // ""')

# Normalize: collapse whitespace, lowercase for matching
normalized=$(echo "$command" | tr '[:upper:]' '[:lower:]' | tr -s ' ')

deny() {
  echo "{\"hookSpecificOutput\":{\"permissionDecision\":\"deny\"},\"systemMessage\":\"BLOCKED by ralph-guard: $1\"}" >&2
  exit 2
}

# --- Filesystem destruction ---
# rm -rf with dangerous targets (/, ~, $HOME, .)
if echo "$normalized" | grep -qE 'rm\s+(-[a-z]*r[a-z]*f|--recursive)\s+(/($|\s|\*)|~/|/root|\$home|\.\s*$|\./)'; then
  deny "Recursive force delete on dangerous path"
fi

# rm -r (without -f) on root
if echo "$normalized" | grep -qE 'rm\s+-[a-z]*r[a-z]*\s+/($|\s)'; then
  deny "Recursive delete on root"
fi

# --- Git destruction ---
if echo "$normalized" | grep -qE 'git\s+push\s+.*(-f|--force)'; then
  deny "Force push"
fi

if echo "$normalized" | grep -qE 'git\s+reset\s+--hard\s+(origin|upstream)/'; then
  deny "Hard reset to remote (destroys local work)"
fi

if echo "$normalized" | grep -qE 'git\s+clean\s+-[a-z]*f'; then
  deny "git clean -f (removes untracked files)"
fi

if echo "$normalized" | grep -qE 'git\s+checkout\s+\.\s*$'; then
  deny "git checkout . (discards all changes)"
fi

# --- Kubernetes destruction ---
if echo "$normalized" | grep -qE 'kubectl\s+delete\s+(namespace|ns)\s'; then
  deny "Deleting Kubernetes namespace"
fi

if echo "$normalized" | grep -qE 'kubectl\s+delete\s+.*--all\s'; then
  deny "kubectl delete --all"
fi

# --- System destruction ---
if echo "$normalized" | grep -qE 'mkfs|dd\s+.*of=/dev|>\s*/dev/sd'; then
  deny "Device/filesystem write"
fi

if echo "$normalized" | grep -qE 'chmod\s+-[rR]\s+777\s+/'; then
  deny "Recursive 777 on root paths"
fi

# --- Scope boundary (cross-repo) ---
# Customize: add patterns for repos that should never be modified from this project.
# Example:
#   if echo "$command" | grep -qE 'other-repo-name'; then
#     deny "Cross-repo boundary violation: Do NOT touch other-repo-name"
#   fi

# Allow everything else
exit 0
