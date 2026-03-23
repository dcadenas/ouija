import type { Plugin } from "@opencode-ai/plugin"

const OUIJA_VERSION = "0.1.0"

export const OuijaPlugin: Plugin = async (ctx) => {
  const port = process.env.OUIJA_PORT || "7880"
  const base = `http://localhost:${port}`

  const daemonAlive = await fetch(`${base}/api/status`)
    .then(() => true)
    .catch(() => false)

  if (!daemonAlive) {
    console.log(`ouija plugin v${OUIJA_VERSION}: daemon not reachable at ${base}, hooks disabled`)
    return {}
  }

  console.log(`ouija plugin v${OUIJA_VERSION}: connected to daemon at ${base}`)

  let lastSessionState: string = ""
  const nameCache = new Map<string, string>()

  function resolveOuijaSessionName(opencodeSid: string, sessions: any[]): string {
    const cached = nameCache.get(opencodeSid)
    if (cached) return cached
    const match = sessions.find((s: any) => s.backend_session_id === opencodeSid)
    const name = match?.id || "(unknown)"
    if (name !== "(unknown)") nameCache.set(opencodeSid, name)
    return name
  }

  async function fetchStatus(): Promise<any> {
    return fetch(`${base}/api/status`).then(r => r.json())
  }

  return {
    "experimental.chat.system.transform": async (input, output) => {
      try {
        const status = await fetchStatus()
        const sid = resolveOuijaSessionName(input.sessionID, status.sessions || [])
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
      } catch {}
    },

    // opencode does NOT await event hooks — setTimeout detaches async work.
    event: ({ event }) => {
      if (event.type === "session.status" || event.type === "session.created") {
        const sid = event.properties?.sessionID || event.properties?.info?.id
        if (!sid) return
        setTimeout(async () => {
          try {
            await fetch(`${base}/api/backend-session/${encodeURIComponent(sid)}/ready`, {
              method: "POST",
              headers: { "Content-Type": "application/json" },
              body: JSON.stringify({})
            })
          } catch {}
        }, 0)
      }

      if (event.type === "session.idle") {
        setTimeout(async () => {
          try {
            const pane = process.env.TMUX_PANE
            if (pane) {
              const paneNum = pane.replace("%", "")
              await fetch(`${base}/api/pane/${paneNum}/stopped`, { method: "POST" })
            }
          } catch {}
        }, 0)
      }
    },

    "chat.message": async (input, output) => {
      try {
        const status = await fetchStatus()
        const sessions = status.sessions || []
        const sid = resolveOuijaSessionName(input.sessionID, sessions)
        const messageID = output.message.id
        const sessionID = input.sessionID

        const current = JSON.stringify(
          sessions
            .map((s: any) => ({ id: s.id, role: s.role, bulletin: s.bulletin }))
            .sort((a: any, b: any) => a.id.localeCompare(b.id))
        )

        if (lastSessionState && current !== lastSessionState) {
          const prev = JSON.parse(lastSessionState)
          const curr = JSON.parse(current)
          const prevIds = new Set(prev.map((s: any) => s.id))
          const currIds = new Set(curr.map((s: any) => s.id))

          const joined = curr.filter((s: any) => !prevIds.has(s.id))
          const left = prev.filter((s: any) => !currIds.has(s.id))

          const lines: string[] = []
          if (joined.length) {
            lines.push(`<ouija-status type="mesh-update">joined:`)
            joined.forEach((s: any) => lines.push(`  - ${s.id}${s.role ? " | " + s.role : ""}`))
            lines.push(`</ouija-status>`)
          }
          if (left.length) {
            lines.push(`<ouija-status type="mesh-update">left: ${left.map((s: any) => s.id).join(", ")}</ouija-status>`)
          }

          if (lines.length) {
            output.parts.push({ type: "text", text: lines.join("\n"), id: crypto.randomUUID(), messageID, sessionID, synthetic: true })
          }
        }

        lastSessionState = current

        if (sid !== "(unknown)") {
          const me = sessions.find((s: any) => s.id === sid)
          if (me?.stale) {
            output.parts.push({
              type: "text",
              text: `<ouija-status type="stale">Your metadata is stale. Call session_update(id="${sid}", role="<what you're doing>") to stay discoverable.</ouija-status>`,
              id: crypto.randomUUID(),
              messageID,
              sessionID,
              synthetic: true,
            })
          }
        }
      } catch {
        // Daemon unreachable — skip silently
      }
    },
  }
}

export default OuijaPlugin
