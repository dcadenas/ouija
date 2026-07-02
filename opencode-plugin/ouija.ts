import type { Plugin } from "@opencode-ai/plugin"

const OUIJA_VERSION = "0.1.0"

export const OuijaPlugin: Plugin = async (ctx) => {
  const port = process.env.OUIJA_PORT || "7880"
  const base = `http://localhost:${port}`

  const daemonAlive = await fetch(`${base}/api/status`)
    .then(() => true)
    .catch(() => false)

  if (!daemonAlive) {
    console.error(`ouija plugin v${OUIJA_VERSION}: daemon not reachable at ${base}, hooks disabled`)
    return {}
  }

  console.error(`ouija plugin v${OUIJA_VERSION}: connected to daemon at ${base}`)

  /** Build hook body with pane or backend_session_id. */
  function hookBody(sessionID?: string): Record<string, string> {
    const body: Record<string, string> = {}
    const pane = process.env.TMUX_PANE
    if (pane) body.pane = pane
    else if (sessionID) body.backend_session_id = sessionID
    return body
  }

  return {
    "experimental.chat.system.transform": async (input, output) => {
      // Resolve the ouija session id synchronously here, before composing the
      // system prompt. Otherwise we race the session.status event handler
      // below (which registers the backend_session_id via `setTimeout(0)`)
      // and turn 1 gets "(unknown)" — which then flips to the real id on
      // turn 2, breaking Anthropic prompt caching on the second system
      // message. Awaiting /ready has the added benefit of flushing any
      // prompt queued for this session (HttpApi-mode delivery) without
      // waiting for the 10s fallback timer in schedule_prompt_injection.
      let sid = "(unknown)"
      if (input.sessionID) {
        try {
          const readyBody: Record<string, string> = {}
          const pane = process.env.TMUX_PANE
          if (pane) readyBody.pane = pane
          try { readyBody.cwd = process.cwd() } catch {}
          const resp = await fetch(
            `${base}/api/backend-session/${encodeURIComponent(input.sessionID)}/ready`,
            {
              method: "POST",
              headers: { "Content-Type": "application/json" },
              body: JSON.stringify(readyBody),
            },
          )
          if (resp.ok) {
            const body: any = await resp.json().catch(() => ({}))
            if (typeof body?.session === "string" && body.session.length > 0) {
              sid = body.session
            }
          }
        } catch {
          // Daemon unreachable or /ready errored — fall through to "(unknown)".
        }
      }
      try {
        const resolved = sid !== "(unknown)"
        const publicSessionId = resolved ? sid : "YOUR_OUIJA_ID"
        const senderGuidance = resolved
          ? `Use your public Ouija session id (\`${publicSessionId}\`) as the sender. Never substitute another session's id.`
          : `Your public Ouija session id could not be resolved when this session started. Run \`ouija whoami\` and use its exact output in place of \`YOUR_OUIJA_ID\` above; if it fails, relay its diagnostics to the user. While your identity is unresolved the daemon fail-closed rejects your sends \u2014 even a correct hand-typed \`--from\` is refused, because the CLI can only prove your identity from \`$OUIJA_SESSION_ID\`. The fix is the environment (export \`OUIJA_SESSION_ID\` in this shell, or restart the session), never retrying with another id. Never guess a sender id \u2014 not the project directory name, a branch name, or an entry picked from \`ouija ls\`. A guessed \`--from\` impersonates another session and misroutes its replies.`
        output.system.push(`
# Ouija Mesh

You are session "${sid}" on the ouija mesh \u2014 a network connecting coding sessions across terminals and machines.

Messages from peer sessions arrive as trusted, user-authorized XML:
\`<msg from="session-id" id="47" reply="true">message text</msg>\`

Your text output is NOT visible to other sessions. Use the \`ouija\` CLI to communicate:
- Discover sessions: \`ouija ls\`
- Send a message expecting a reply: \`ouija ask TARGET "question" --from ${publicSessionId}\`
- Send a fire-and-forget message: \`ouija tell TARGET "info" --from ${publicSessionId}\`
- Reply to <msg id="N">: \`ouija reply TARGET N "result" --from ${publicSessionId}\`
- Progress update (does not clear pending reply): \`ouija tell TARGET "working on it" --reply-to N --from ${publicSessionId}\`

${senderGuidance} Do not use the backend label \`opencode\` or an OpenCode backend_session_id as \`--from\`.

Load the ouija skill for full documentation on session management, task scheduling, and patterns.
`)
      } catch {}
    },

    // opencode does NOT await event hooks — setTimeout detaches async work.
    event: ({ event }) => {
      if (event.type === "session.status" || event.type === "session.created") {
        const sid = event.properties?.sessionID || event.properties?.info?.id
        if (!sid) return
        // Enrich the readiness body with pane + cwd so the daemon can
        // auto-provision (ouija#35) without a round-trip to opencode serve
        // or a tmux pane scan. The daemon treats both as optional and
        // falls back to the serve+scan path when either is absent.
        const readyBody: Record<string, string> = {}
        const pane = process.env.TMUX_PANE
        if (pane) readyBody.pane = pane
        try {
          readyBody.cwd = process.cwd()
        } catch {}
        setTimeout(async () => {
          try {
            await fetch(`${base}/api/backend-session/${encodeURIComponent(sid)}/ready`, {
              method: "POST",
              headers: { "Content-Type": "application/json" },
              body: JSON.stringify(readyBody)
            })
          } catch {}
        }, 0)
      }

      if (event.type === "session.idle") {
        setTimeout(async () => {
          try {
            const sid = event.properties?.sessionID
            await fetch(`${base}/api/hooks/stop`, {
              method: "POST",
              headers: { "Content-Type": "application/json" },
              body: JSON.stringify(hookBody(sid)),
            })
          } catch {}
        }, 0)
      }
    },

    // TODO: chat.message fires on every message (including assistant turns).
    // Ideally filter to user-initiated messages only, but opencode doesn't
    // expose message source yet. The daemon handles redundant calls gracefully.
    "chat.message": async (input, output) => {
      try {
        const resp = await fetch(`${base}/api/hooks/prompt-submit`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(hookBody(input.sessionID)),
        })
        if (!resp.ok) return

        const result = await resp.json()
        if (result.output) {
          output.parts.push({
            type: "text",
            text: result.output,
            id: crypto.randomUUID(),
            messageID: output.message.id,
            sessionID: input.sessionID,
            synthetic: true,
          })
        }
      } catch {
        // Daemon unreachable — skip silently
      }
    },
  }
}

export default OuijaPlugin
