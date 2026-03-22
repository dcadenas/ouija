import type { Plugin } from "@opencode-ai/plugin"

const OUIJA_VERSION = "0.1.0"

export const OuijaPlugin: Plugin = async (ctx) => {
  const port = process.env.OUIJA_PORT || "7880"
  const base = `http://localhost:${port}`

  // Check if ouija daemon is reachable
  const daemonAlive = await fetch(`${base}/api/status`)
    .then(() => true)
    .catch(() => false)

  if (!daemonAlive) {
    console.log(`ouija plugin v${OUIJA_VERSION}: daemon not reachable at ${base}, hooks disabled`)
    return {}
  }

  console.log(`ouija plugin v${OUIJA_VERSION}: connected to daemon at ${base}`)

  // Cache for session diff
  let lastSessionState: string = ""

  // Cache for resolved ouija session name (keyed by opencode session ID)
  const nameCache = new Map<string, string>()

  async function resolveOuijaSessionName(opencodeSid: string): Promise<string> {
    const cached = nameCache.get(opencodeSid)
    if (cached) return cached
    try {
      const status = await fetch(`${base}/api/status`).then(r => r.json())
      const match = (status.sessions || []).find(
        (s: any) => s.backend_session_id === opencodeSid
      )
      const name = match?.id || "(unknown)"
      if (name !== "(unknown)") nameCache.set(opencodeSid, name)
      return name
    } catch {
      return "(unknown)"
    }
  }

  return {
    // --- Hook 1: Inject ouija protocol into system prompt ---
    "experimental.chat.system.transform": async (input, output) => {
      const sid = await resolveOuijaSessionName(input.sessionID)
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
    // Note: opencode's plugin system does NOT await event hooks (fire-and-forget).
    // We use setTimeout to detach async work from the non-awaited handler chain.
    event: ({ event }) => {
      if (event.type === "session.status" || event.type === "session.created") {
        const sid = event.properties?.sessionID || event.properties?.info?.id
        if (!sid) return
        // Detach async work — the event handler isn't awaited by opencode
        setTimeout(async () => {
          try {
            let ouijaName = "(unknown)"
            for (let attempt = 0; attempt < 5; attempt++) {
              ouijaName = await resolveOuijaSessionName(sid)
              if (ouijaName !== "(unknown)") break
              await new Promise(r => setTimeout(r, 1000))
            }
            if (ouijaName === "(unknown)") return
            await fetch(`${base}/api/session/${encodeURIComponent(ouijaName)}/ready`, {
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

    // --- Hook 3: Mesh awareness on each message ---
    "chat.message": async (input, output) => {
      try {
        const sid = await resolveOuijaSessionName(input.sessionID)

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
        if (sid !== "(unknown)") {
          const me = (status.sessions || []).find((s: any) => s.id === sid)
          if (me?.stale) {
            output.parts.push({
              type: "text",
              text: `[ouija] Your metadata is stale. Call session_update(id="${sid}", role="<what you're doing>") to stay discoverable.`
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
