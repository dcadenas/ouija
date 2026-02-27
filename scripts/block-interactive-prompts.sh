#!/bin/bash
# Conditionally block interactive prompts (AskUserQuestion, EnterPlanMode)
# for ouija sessions. Only blocks when the session is handling an ouija
# message, not during direct user interaction.

INPUT=$(cat)
TOOL=$(echo "$INPUT" | jq -r '.tool_name // "unknown"')
PANE="${TMUX_PANE:-$(tmux display-message -p '#{pane_id}' 2>/dev/null)}"
if [ -z "$PANE" ]; then
  echo "ok" >&2
  exit 0
fi

PORT="${OUIJA_PORT:-7880}"
PANE_NUM="${PANE#%}"

# Ask the daemon if this pane should block interactive prompts
BLOCKED=$(curl -sf "http://localhost:${PORT}/api/pane/${PANE_NUM}/block-interactive" 2>/dev/null \
  | jq -r '.block_interactive // false')

if [ "$BLOCKED" != "true" ]; then
  echo "ok" >&2
  exit 0
fi

case "$TOOL" in
  AskUserQuestion)
    cat >&2 << 'EOF'
Interactive prompts are disabled while handling ouija messages.
Do NOT use AskUserQuestion. Instead, respond in prose with the available
options and let the user answer via message. If this question was triggered
by a message from another session, forward the question to them via
session_send and continue when they reply.
EOF
    ;;
  EnterPlanMode)
    cat >&2 << 'EOF'
Plan mode is disabled while handling ouija messages.
Do NOT use EnterPlanMode. Instead, write your plan as a prose message to
the user or to the session that requested the task via session_send.
Describe your approach, list the steps, and ask for approval in the
message. Proceed when they confirm.
EOF
    ;;
  *)
    cat >&2 << EOF
Interactive tool '$TOOL' is disabled while handling ouija messages.
Communicate in prose via session_send instead.
EOF
    ;;
esac
exit 2
