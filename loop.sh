#!/bin/bash
TASK_DESC=""
if [ "$1" = "task" ]; then
    TASK_DESC="$2"; PROMPT_FILE="PROMPT_build.md"; MAX=${3:-0}
elif [ "$1" = "plan" ]; then
    PROMPT_FILE="PROMPT_plan.md"; MAX=${2:-0}
elif [[ "$1" =~ ^[0-9]+$ ]]; then
    PROMPT_FILE="PROMPT_build.md"; MAX=$1
else
    PROMPT_FILE="PROMPT_build.md"; MAX=0
fi
ITERATION=0
[ -n "$TASK_DESC" ] && echo "$TASK_DESC" > TASK.md
while true; do
    [ -n "$TASK_DESC" ] && [ $ITERATION -gt 0 ] && [ ! -f TASK.md ] && echo "Task complete." && break
    [ $MAX -gt 0 ] && [ $ITERATION -ge $MAX ] && break
    claude -p --dangerously-skip-permissions --model opus --verbose < "$PROMPT_FILE"
    git push origin "$(git branch --show-current)" 2>/dev/null || git push -u origin "$(git branch --show-current)"
    ITERATION=$((ITERATION + 1))
    echo -e "\n════════════════════ LOOP $ITERATION ════════════════════\n"
done
