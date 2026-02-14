#!/bin/bash
# Ralph Wiggum Loop
# Usage: ./loop.sh [--json] [plan|plan-work "description"|task "description"] [max_iterations]
# Examples:
#   ./loop.sh              # Build mode, human output, unlimited
#   ./loop.sh --json       # Build mode, JSON output
#   ./loop.sh 20           # Build mode, max 20 iterations
#   ./loop.sh plan         # Plan mode, unlimited
#   ./loop.sh plan 5       # Plan mode, max 5 iterations
#   ./loop.sh --json plan  # Plan mode, JSON output
#   ./loop.sh plan-work "user auth with OAuth"  # Scoped plan for work branch
#   ./loop.sh task "migrate rand per specs/rand-0.10-migration.md"  # Task mode, scoped to one task
#   ./loop.sh task "fix PKCE verifier length" 3  # Task mode, max 3 iterations

# Parse --json flag
OUTPUT_FORMAT=""
if [ "$1" = "--json" ]; then
    OUTPUT_FORMAT="--output-format=stream-json"
    shift
fi

# Parse mode and iterations
if [ "$1" = "task" ]; then
    if [ -z "$2" ]; then
        echo "Error: task requires a description or spec path"
        echo "Usage: ./loop.sh task \"description of the task\" [max_iterations]"
        exit 1
    fi
    MODE="task"
    PROMPT_FILE="PROMPT_build.md"
    TASK_DESC="$2"
    MAX_ITERATIONS=${3:-0}
elif [ "$1" = "plan-work" ]; then
    if [ -z "$2" ]; then
        echo "Error: plan-work requires a work description"
        echo "Usage: ./loop.sh plan-work \"description of the work\""
        exit 1
    fi
    MODE="plan-work"
    PROMPT_FILE="PROMPT_plan_work.md"
    export WORK_SCOPE="$2"
    MAX_ITERATIONS=${3:-5}
elif [ "$1" = "plan" ]; then
    MODE="plan"
    PROMPT_FILE="PROMPT_plan.md"
    MAX_ITERATIONS=${2:-0}
elif [[ "$1" =~ ^[0-9]+$ ]]; then
    MODE="build"
    PROMPT_FILE="PROMPT_build.md"
    MAX_ITERATIONS=$1
else
    MODE="build"
    PROMPT_FILE="PROMPT_build.md"
    MAX_ITERATIONS=0
fi

ITERATION=0
CURRENT_BRANCH=$(git branch --show-current)
CLAUDE_PID=""

# Validate branch for plan-work mode
if [ "$MODE" = "plan-work" ]; then
    if [ "$CURRENT_BRANCH" = "main" ] || [ "$CURRENT_BRANCH" = "master" ]; then
        echo "Error: plan-work should be run on a work branch, not main/master"
        echo "Create a work branch first: git checkout -b ralph/your-work"
        exit 1
    fi
fi

# --- Sandbox pre-flight check ---
# Warn if no sandbox boundary is detected. Does not block — the operator
# may have a sandbox mechanism this check cannot detect.
check_sandbox() {
    # Container indicators
    [ -f /.dockerenv ] && return 0
    [ "${CONTAINER:-}" = "true" ] && return 0
    grep -qE '/docker/|/lxc/' /proc/1/cgroup 2>/dev/null && return 0
    # Claude Code native sandbox (bubblewrap/seatbelt)
    command -v bwrap >/dev/null 2>&1 && return 0
    [ "$(uname)" = "Darwin" ] && return 0  # Seatbelt available on macOS
    return 1
}
if ! check_sandbox; then
    echo "⚠  WARNING: No sandbox boundary detected."
    echo "   The loop uses --dangerously-skip-permissions (all tool calls auto-approved)."
    echo "   Recommended: enable Claude Code native sandbox (/sandbox command)"
    echo "   or run inside a container (docker sandbox run claude)."
    echo "   See forge-preflight references/sandbox-guide.md for details."
    echo ""
    echo "   Continuing in 5 seconds... (Ctrl+C to abort)"
    sleep 5
fi

# Signal handler — kill claude process and exit cleanly
cleanup() {
    echo -e "\n\nCaught signal, stopping..."
    if [ -n "$CLAUDE_PID" ] && kill -0 "$CLAUDE_PID" 2>/dev/null; then
        kill -TERM "$CLAUDE_PID" 2>/dev/null
        sleep 0.5
        kill -9 "$CLAUDE_PID" 2>/dev/null
    fi
    exit 130
}
trap cleanup SIGINT SIGTERM SIGQUIT

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "Ralph Wiggum Loop"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "Mode:   $MODE"
echo "Prompt: $PROMPT_FILE"
echo "Output: ${OUTPUT_FORMAT:-human}"
echo "Branch: $CURRENT_BRANCH"
[ "$MODE" = "task" ] && echo "Task:   $TASK_DESC"
[ "$MODE" = "plan-work" ] && echo "Scope:  $WORK_SCOPE"
[ $MAX_ITERATIONS -gt 0 ] && echo "Max:    $MAX_ITERATIONS iterations"
echo "Stop:   Ctrl+C"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

# Verify prompt file exists
if [ ! -f "$PROMPT_FILE" ]; then
    echo "Error: $PROMPT_FILE not found"
    exit 1
fi

while true; do
    # Task mode: exit when TASK.md has been deleted (agent signals completion)
    if [ "$MODE" = "task" ] && [ $ITERATION -gt 0 ] && [ ! -f TASK.md ]; then
        echo "Task complete — TASK.md deleted by agent."
        break
    fi

    # Create TASK.md at start of each iteration if it doesn't exist yet (first iteration only)
    if [ "$MODE" = "task" ] && [ $ITERATION -eq 0 ]; then
        echo "$TASK_DESC" > TASK.md
        echo "Created TASK.md"
    fi

    if [ $MAX_ITERATIONS -gt 0 ] && [ $ITERATION -ge $MAX_ITERATIONS ]; then
        echo "Reached max iterations: $MAX_ITERATIONS"
        if [ "$MODE" = "plan-work" ]; then
            echo ""
            echo "Scoped plan created for: $WORK_SCOPE"
            echo "To build: ./loop.sh"
        fi
        if [ "$MODE" = "task" ] && [ -f TASK.md ]; then
            echo ""
            echo "⚠  TASK.md still exists — task may be incomplete."
        fi
        break
    fi

    # Run Ralph iteration
    # Background + wait pattern enables signal handling during execution
    # -p: Headless mode (non-interactive, reads from stdin)
    # --dangerously-skip-permissions: Auto-approve all tool calls
    # --model opus: Opus for task selection/prioritization
    # plan-work mode: envsubst substitutes ${WORK_SCOPE} in the prompt template
    if [ "$MODE" = "plan-work" ]; then
        envsubst < "$PROMPT_FILE" | claude -p \
            --dangerously-skip-permissions \
            ${OUTPUT_FORMAT:+"$OUTPUT_FORMAT"} \
            --model opus \
            --verbose &
    else
        claude -p \
            --dangerously-skip-permissions \
            ${OUTPUT_FORMAT:+"$OUTPUT_FORMAT"} \
            --model opus \
            --verbose \
            < "$PROMPT_FILE" &
    fi
    CLAUDE_PID=$!
    wait $CLAUDE_PID
    EXIT_CODE=$?
    CLAUDE_PID=""

    if [ $EXIT_CODE -ne 0 ]; then
        echo "⚠  Claude exited with code $EXIT_CODE (iteration $((ITERATION + 1)))"
    fi

    # Push changes after each iteration
    if ! git push origin "$CURRENT_BRANCH" 2>/dev/null; then
        if ! git push -u origin "$CURRENT_BRANCH" 2>/dev/null; then
            echo "⚠  git push failed (iteration $((ITERATION + 1)))"
            echo "   Local commits accumulating — check auth/network."
        fi
    fi

    ITERATION=$((ITERATION + 1))
    echo -e "\n\n════════════════════ LOOP $ITERATION ════════════════════\n"
done
