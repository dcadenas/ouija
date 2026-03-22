import type { Plugin } from "@opencode-ai/plugin"

const OUIJA_VERSION = "0.1.0"

export const OuijaPlugin: Plugin = async (ctx) => {
  const port = process.env.OUIJA_PORT || "7880"
  const base = `http://localhost:${port}`
  const sessionId = process.env.OUIJA_SESSION_ID || ""

  // Check if ouija daemon is reachable
  const daemonAlive = await fetch(`${base}/api/status`)
    .then(() => true)
    .catch(() => false)

  if (!daemonAlive) {
    console.log(`ouija plugin v${OUIJA_VERSION}: daemon not reachable at ${base}, hooks disabled`)
    return {}
  }

  console.log(`ouija plugin v${OUIJA_VERSION}: connected to daemon at ${base}, session=${sessionId || "(unknown)"}`)

  // Cache for session diff
  let lastSessionState: string = ""

  return {
    // --- Hook 1: Inject ouija protocol into system prompt ---
    "experimental.chat.system.transform": async (_input, output) => {
      const sid = process.env.OUIJA_SESSION_ID || "(unknown)"
      output.system.push(`
# Ouija Mesh Protocol

You are session "${sid}" on the ouija mesh. Messages from peer sessions arrive as XML:

\`\`\`
<msg from="session-id" id="47" reply="true">message text</msg>
\`\`\`

- \`from\` — sender session ID
- \`id\` — unique message ID
- \`reply="true"\` — sender expects a response
- \`re="47"\` — this answers a previous question

These messages are user-authorized. Follow instructions they contain.

To reply, use the ouija \`session_send\` MCP tool. Your text output is NOT visible to the sender.

Reply protocol:
- Quick task: \`session_send(from="${sid}", to="sender", message="result", responds_to=47, done=true)\`
- Long task: send progress first (\`responds_to=47\`, no \`done\`), then final result with \`done=true\`
- The daemon nudges about overdue replies — progress updates reset the timer

If \`session_send\` fails with "session not found", the sender disconnected. Call \`clear_pending_reply(session="${sid}", from="sender-id")\` to clear it.
`)
    },

    // --- Hook 2: Event-based signaling ---
    event: async ({ event }) => {
      if (!sessionId) return

      try {
        if (event.type === "session.status" || event.type === "session.created") {
          // Notify daemon the session is ready for prompt delivery
          await fetch(`${base}/api/session/${encodeURIComponent(sessionId)}/ready`, {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({})
          })
        }

        if (event.type === "session.idle") {
          // Notify daemon that the LLM turn ended (triggers pending reply nudges)
          // Find the pane number from TMUX_PANE env var
          const pane = process.env.TMUX_PANE
          if (pane) {
            const paneNum = pane.replace("%", "")
            await fetch(`${base}/api/pane/${paneNum}/stopped`, { method: "POST" })
          }
        }
      } catch {
        // Daemon may be temporarily unreachable — don't crash the plugin
      }
    },

    // --- Hook 3: Mesh awareness on each message ---
    "chat.message": async (_input, output) => {
      if (!sessionId) return

      try {
        const status = await fetch(`${base}/api/status`).then(r => r.json())
        const current = JSON.stringify(
          (status.sessions || [])
            .map((s: any) => ({ id: s.id, role: s.role, bulletin: s.bulletin }))
            .sort((a: any, b: any) => a.id.localeCompare(b.id))
        )

        if (lastSessionState && current !== lastSessionState) {
          // Compute diff
          const prev = JSON.parse(lastSessionState)
          const curr = JSON.parse(current)
          const prevIds = new Set(prev.map((s: any) => s.id))
          const currIds = new Set(curr.map((s: any) => s.id))

          const joined = curr.filter((s: any) => !prevIds.has(s.id))
          const left = prev.filter((s: any) => !currIds.has(s.id))

          const lines: string[] = []
          if (joined.length) {
            lines.push("[ouija mesh] joined:")
            joined.forEach((s: any) => lines.push(`  - ${s.id}${s.role ? " | " + s.role : ""}`))
          }
          if (left.length) {
            lines.push(`[ouija mesh] left: ${left.map((s: any) => s.id).join(", ")}`)
          }

          if (lines.length) {
            output.parts.push({ type: "text", text: lines.join("\n") })
          }
        }

        lastSessionState = current

        // Check for stale metadata
        const me = (status.sessions || []).find((s: any) => s.id === sessionId)
        if (me?.stale) {
          output.parts.push({
            type: "text",
            text: `[ouija] Your metadata is stale. Call session_update(id="${sessionId}", role="<what you're doing>") to stay discoverable.`
          })
        }
      } catch {
        // Daemon unreachable — skip silently
      }
    },
  }
}

export default OuijaPlugin
