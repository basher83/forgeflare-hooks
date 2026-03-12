#!/bin/bash
# cmux Supervisor for Ralph Loop
# Usage: ./supervisor.sh [loop.sh args...]
# Examples:
#   ./supervisor.sh              # Build mode (default)
#   ./supervisor.sh plan         # Plan mode
#   ./supervisor.sh plan 5       # Plan mode, max 5 iterations
#   ./supervisor.sh task "fix glob injection" 3

set -euo pipefail

# --- Configuration ---
POLL_INTERVAL=15          # seconds between screen polls
INACTIVITY_THRESHOLD=40   # consecutive unchanged polls before hung (40 * 15s = 10min)
MAX_RESTARTS=5            # give up after this many restarts
SCREEN_LINES=200          # lines to capture from loop pane (more scrollback = better change detection)

# --- Fatal patterns ---
FATAL_PATTERNS=(
    "prompt is too long"
    "after property name in JSON at"
    "exceed context limit"
    "Shutting down\.\.\."
    "Error: ENOENT"
    "Error: EPERM"
    "Connection refused"
    "error sending request"
)

# --- Clean exit patterns ---
EXIT_PATTERNS=(
    "Reached max iterations"
    "Task complete"
)

# --- State ---
LOOP_SURFACE=""
LOOP_ARGS=("$@")
LOOP_CMD=""
RESTART_COUNT=0
ITERATION=0
PREV_HASH=""
STALE_COUNT=0
STATE="starting"
START_TIME=$(date +%s)

# --- Helpers ---
log_info()    { echo "[supervisor] $1"; cmux log --source ralph --level info    -- "$1" 2>/dev/null || true; }
log_success() { echo "[supervisor] $1"; cmux log --source ralph --level success -- "$1" 2>/dev/null || true; }
log_warning() { echo "[supervisor] ⚠  $1"; cmux log --source ralph --level warning -- "$1" 2>/dev/null || true; }
log_error()   { echo "[supervisor] ✗  $1"; cmux log --source ralph --level error   -- "$1" 2>/dev/null || true; }

set_state() {
    STATE="$1"
    local icon="hammer" color=""
    case "$STATE" in
        starting)   icon="hammer";  color="#007aff" ;;
        running)    icon="hammer";  color="#34c759" ;;
        hung)       icon="hammer";  color="#ff9500" ;;
        restarting) icon="hammer";  color="#ff9500" ;;
        converged)  icon="sparkle"; color="#34c759" ;;
        stopped)    icon="sparkle"; color="#8e8e93" ;;
        failed)     icon="sparkle"; color="#ff3b30" ;;
    esac
    cmux set-status ralph "$STATE" --icon "$icon" --color "$color" 2>/dev/null || true
}

update_progress() {
    local iter="$1" max="$2"
    if [ "$max" -gt 0 ] 2>/dev/null; then
        local frac
        frac=$(echo "scale=2; $iter / $max" | bc 2>/dev/null || echo "0")
        cmux set-progress "$frac" --label "iteration $iter/$max" 2>/dev/null || true
    else
        cmux set-progress 0 --label "iteration $iter" 2>/dev/null || true
    fi
}

format_duration() {
    local secs=$1
    local mins=$((secs / 60))
    local hours=$((mins / 60))
    mins=$((mins % 60))
    secs=$((secs % 60))
    if [ "$hours" -gt 0 ]; then
        printf "%dh%02dm%02ds" "$hours" "$mins" "$secs"
    elif [ "$mins" -gt 0 ]; then
        printf "%dm%02ds" "$mins" "$secs"
    else
        printf "%ds" "$secs"
    fi
}

print_summary() {
    local reason="$1"
    local elapsed=$(($(date +%s) - START_TIME))
    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo "Supervisor Summary"
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo "Runtime:    $(format_duration $elapsed)"
    echo "Iterations: $ITERATION"
    echo "Restarts:   $RESTART_COUNT"
    echo "Exit:       $reason"
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    cmux clear-progress 2>/dev/null || true
}

read_screen() {
    cmux read-screen --surface "$LOOP_SURFACE" --scrollback --lines "$SCREEN_LINES" 2>/dev/null || echo ""
}

hash_screen() {
    echo "$1" | shasum -a 256 | cut -d' ' -f1
}

parse_iteration() {
    local screen="$1"
    echo "$screen" | grep -oE 'LOOP [0-9]+' | tail -1 | grep -oE '[0-9]+' || echo "0"
}

parse_max_iterations() {
    local screen="$1"
    echo "$screen" | grep -oE 'Max: +[0-9]+ iterations' | grep -oE '[0-9]+' || echo "0"
}

check_loop_exited() {
    local screen="$1"
    local last_line
    last_line=$(echo "$screen" | grep -v '^$' | tail -1)
    echo "$last_line" | grep -qE '^\$ *$|^% *$|^❯ *$'
}

# Returns 0 if restart succeeded, 1 if max restarts exceeded
try_restart() {
    local reason="$1"
    if [ "$RESTART_COUNT" -ge "$MAX_RESTARTS" ]; then
        log_error "Max restarts ($MAX_RESTARTS) exceeded. Giving up."
        set_state "failed"
        cmux trigger-flash --surface "$LOOP_SURFACE" 2>/dev/null || true
        print_summary "failed (max restarts)"
        return 1
    fi

    RESTART_COUNT=$((RESTART_COUNT + 1))
    set_state "restarting"
    log_warning "Restarting loop (attempt $RESTART_COUNT/$MAX_RESTARTS): $reason"
    cmux trigger-flash --surface "$LOOP_SURFACE" 2>/dev/null || true

    cmux send-key --surface "$LOOP_SURFACE" ctrl+c 2>/dev/null || true
    sleep 2
    cmux send --surface "$LOOP_SURFACE" -- "${LOOP_CMD}\n"
    set_state "running"
    STALE_COUNT=0
    PREV_HASH=""
    return 0
}

# --- Preflight ---
if ! command -v cmux >/dev/null 2>&1; then
    echo "Error: cmux not found in PATH"
    exit 1
fi

if ! cmux ping >/dev/null 2>&1; then
    echo "Error: cmux socket not responding (is cmux running?)"
    exit 1
fi

if [ ! -x "./loop.sh" ]; then
    echo "Error: ./loop.sh not found or not executable"
    exit 1
fi

# --- Phase 1: Setup ---
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "cmux Supervisor"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "Loop args: ${LOOP_ARGS[*]+${LOOP_ARGS[*]}}"
echo "Poll:      ${POLL_INTERVAL}s"
echo "Hang:      $((INACTIVITY_THRESHOLD * POLL_INTERVAL))s inactivity"
echo "Restarts:  max $MAX_RESTARTS"
echo "Stop:      Ctrl+C"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

set_state "starting"

# Create split pane for the loop
SPLIT_OUTPUT=$(cmux new-split right 2>&1)
LOOP_SURFACE=$(echo "$SPLIT_OUTPUT" | grep -oE 'surface:[0-9]+' | head -1)

if [ -z "$LOOP_SURFACE" ]; then
    echo "Error: failed to create split pane"
    echo "cmux new-split output: $SPLIT_OUTPUT"
    exit 1
fi

echo "Loop pane: $LOOP_SURFACE"
log_info "Supervisor started, loop pane: $LOOP_SURFACE"

# Build the loop command
LOOP_CMD="./loop.sh"
if [ $# -gt 0 ]; then
    for arg in "${LOOP_ARGS[@]}"; do
        if [[ "$arg" == *" "* ]]; then
            LOOP_CMD="$LOOP_CMD \"$arg\""
        else
            LOOP_CMD="$LOOP_CMD $arg"
        fi
    done
fi

# Launch the loop
cmux send --surface "$LOOP_SURFACE" -- "${LOOP_CMD}\n"
log_info "Launched: $LOOP_CMD"
set_state "running"
sleep 3

# --- Signal handler ---
cleanup() {
    echo ""
    log_warning "Supervisor interrupted"
    set_state "stopped"
    cmux send-key --surface "$LOOP_SURFACE" ctrl+c 2>/dev/null || true
    sleep 1
    local elapsed=$(($(date +%s) - START_TIME))
    print_summary "interrupted"
    log_success "Stopped after $(format_duration $elapsed), $ITERATION iterations, $RESTART_COUNT restarts"
    exit 130
}
trap cleanup SIGINT SIGTERM SIGQUIT

# --- Phase 2: Monitor Loop ---
while true; do
    sleep "$POLL_INTERVAL"

    SCREEN=$(read_screen)
    CURRENT_HASH=$(hash_screen "$SCREEN")

    # Track iteration progress
    NEW_ITER=$(parse_iteration "$SCREEN")
    if [ "$NEW_ITER" -gt "$ITERATION" ] 2>/dev/null; then
        ITERATION=$NEW_ITER
        MAX_ITER=$(parse_max_iterations "$SCREEN")
        update_progress "$ITERATION" "$MAX_ITER"
        log_info "Iteration $ITERATION"
    fi

    # --- Clean exit patterns ---
    MATCHED_EXIT=""
    for pattern in "${EXIT_PATTERNS[@]}"; do
        if echo "$SCREEN" | grep -qi "$pattern"; then
            MATCHED_EXIT="$pattern"
            break
        fi
    done
    if [ -n "$MATCHED_EXIT" ]; then
        log_success "Loop finished: $MATCHED_EXIT"
        set_state "converged"
        cmux trigger-flash --surface "$LOOP_SURFACE" 2>/dev/null || true
        print_summary "converged ($MATCHED_EXIT)"
        exit 0
    fi

    # --- Loop process exited (shell prompt visible) ---
    if check_loop_exited "$SCREEN"; then
        if echo "$SCREEN" | grep -qE 'Reached max iterations|Task complete'; then
            log_success "Loop exited cleanly"
            set_state "converged"
            print_summary "clean"
            exit 0
        fi

        log_warning "Loop process exited unexpectedly"
        if ! try_restart "unexpected exit"; then
            exit 1
        fi
        continue
    fi

    # --- Fatal error patterns ---
    MATCHED_FATAL=""
    for pattern in "${FATAL_PATTERNS[@]}"; do
        if echo "$SCREEN" | grep -qi "$pattern"; then
            MATCHED_FATAL="$pattern"
            break
        fi
    done
    if [ -n "$MATCHED_FATAL" ]; then
        log_warning "Fatal pattern: $MATCHED_FATAL"
        if ! try_restart "$MATCHED_FATAL"; then
            exit 1
        fi
        continue
    fi

    # --- Inactivity detection ---
    # Only trigger if screen is truly frozen AND we don't see active process indicators.
    # claude -p can go quiet for minutes during thinking — that's normal.
    if [ -n "$PREV_HASH" ] && [ "$CURRENT_HASH" = "$PREV_HASH" ]; then
        STALE_COUNT=$((STALE_COUNT + 1))
        if [ "$STALE_COUNT" -ge "$INACTIVITY_THRESHOLD" ]; then
            # Double-check: if loop exited (prompt visible), try_restart handles it above.
            # If we get here, screen is frozen but no prompt — likely a true hang.
            set_state "hung"
            log_warning "Inactivity: ${STALE_COUNT} polls ($((STALE_COUNT * POLL_INTERVAL))s)"
            if ! try_restart "inactivity ($((STALE_COUNT * POLL_INTERVAL))s)"; then
                exit 1
            fi
            continue
        elif [ "$((STALE_COUNT % 10))" -eq 0 ] && [ "$STALE_COUNT" -gt 0 ]; then
            # Periodic heartbeat so operator knows supervisor is alive
            log_info "Waiting... ($((STALE_COUNT * POLL_INTERVAL))s no output change)"
        fi
    else
        STALE_COUNT=0
    fi

    PREV_HASH="$CURRENT_HASH"
done
