use axum::extract::State;
use axum::response::Html;

use crate::state::SharedState;

pub async fn dashboard(State(state): State<SharedState>) -> Html<String> {
    let sessions = state.sessions.read().await;
    let peers = state.peers.read().await;
    let log = state.message_log.read().await;
    let transports = state.transports().await;
    let settings = state.settings.read().await;
    let scheduled_tasks = state.scheduled_tasks.read().await;
    let task_runs = state.task_runs.read().await;

    let any_ready = transports.values().any(|t| t.is_ready());

    let peer_count = peers.len();
    let msg_count = log.len();
    let task_count = scheduled_tasks.len();
    let task_run_count = task_runs.len();

    let mut local_sessions: Vec<_> = sessions
        .values()
        .filter(|s| matches!(s.origin, crate::state::SessionOrigin::Local))
        .collect();
    local_sessions.sort_by_key(|s| &s.id);

    let mut remote_sessions: Vec<_> = sessions
        .values()
        .filter(|s| matches!(s.origin, crate::state::SessionOrigin::Remote(_)))
        .collect();
    remote_sessions.sort_by_key(|s| &s.id);

    let local_count = local_sessions.len();
    let remote_count = remote_sessions.len();

    let mut sessions_html = String::new();
    for s in &local_sessions {
        let escaped_id = html_escape(&s.id);
        let pane = s.pane.as_deref().unwrap_or("--");
        let time = s.registered_at.format("%H:%M:%S");
        let actions = format!(
            r#"<button class="btn-sm" onclick="renameSession('{id}')">rename</button> <button class="btn-sm btn-danger" onclick="removeSession('{id}')">remove</button>"#,
            id = html_escape(&s.id)
        );
        sessions_html.push_str(&format!(
            "<tr><td class=\"id-cell\">{escaped_id}</td><td>{pane}</td><td class=\"dim\">{time}</td><td>{actions}</td></tr>",
        ));
    }
    if local_sessions.is_empty() {
        sessions_html.push_str(
            r#"<tr><td colspan="4" class="empty">No local sessions. Open Claude Code in tmux and say <b>"register me as web"</b></td></tr>"#,
        );
    }

    let mut remote_sessions_html = String::new();
    for s in &remote_sessions {
        let escaped_id = html_escape(&s.id);
        let time = s.registered_at.format("%H:%M:%S");
        let daemon = match &s.origin {
            crate::state::SessionOrigin::Remote(d) => html_escape(d),
            _ => String::new(),
        };
        remote_sessions_html.push_str(&format!(
            "<tr><td class=\"id-cell\">{escaped_id}</td><td class=\"dim\">{daemon}</td><td class=\"dim\">{time}</td></tr>",
        ));
    }
    if remote_sessions.is_empty() {
        remote_sessions_html.push_str(
            r#"<tr><td colspan="3" class="empty">No remote sessions. Connect a peer below.</td></tr>"#,
        );
    }

    let mut peers_html = String::new();
    for p in peers.values() {
        peers_html.push_str(&format!(
            "<tr><td class=\"id-cell\">{}</td><td class=\"dim\">{}</td><td class=\"dim\">{}</td></tr>",
            html_escape(&p.name),
            html_escape(&p.daemon_id),
            p.connected_at.format("%H:%M:%S"),
        ));
    }

    // --- Scheduled Tasks ---
    let mut tasks_html = String::new();
    let mut sorted_tasks: Vec<_> = scheduled_tasks.values().collect();
    sorted_tasks.sort_by_key(|t| &t.created_at);
    for t in &sorted_tasks {
        let enabled_checked = if t.enabled { "checked" } else { "" };
        let next = t.next_run.map_or("--".into(), |dt| dt.format("%H:%M:%S").to_string());
        let last = t.last_run.map_or("--".into(), |dt| dt.format("%H:%M:%S").to_string());
        let status = t.last_status.as_ref().map_or("--", |s| match s {
            crate::scheduler::TaskRunStatus::Ok => "ok",
            crate::scheduler::TaskRunStatus::Revived => "revived",
            crate::scheduler::TaskRunStatus::Failed => "failed",
        });
        let status_class = match t.last_status.as_ref() {
            Some(crate::scheduler::TaskRunStatus::Ok | crate::scheduler::TaskRunStatus::Revived) => "status-ok",
            Some(crate::scheduler::TaskRunStatus::Failed) => "status-fail",
            None => "dim",
        };
        tasks_html.push_str(&format!(
            r#"<tr>
<td class="id-cell">{id}</td>
<td>{name}</td>
<td class="dim">{cron}</td>
<td>{target}</td>
<td style="text-align:center;"><input type="checkbox" {enabled_checked} onchange="this.checked ? enableTask('{id}') : disableTask('{id}')"></td>
<td class="dim">{next}</td>
<td class="dim">{last}</td>
<td class="{status_class}">{status}</td>
<td>{run_count}</td>
<td>
  <button class="btn-sm" onclick="triggerTask('{id}')">trigger</button>
  <button class="btn-sm btn-danger" onclick="deleteTask('{id}')">delete</button>
</td>
</tr>"#,
            id = html_escape(&t.id),
            name = html_escape(&t.name),
            cron = html_escape(&t.cron),
            target = html_escape(&t.target_session),
            run_count = t.run_count,
        ));
    }
    if sorted_tasks.is_empty() {
        tasks_html.push_str(
            r#"<tr><td colspan="10" class="empty">No scheduled tasks. Create one via CLI: <b>ouija task add &lt;name&gt; &lt;cron&gt; &lt;target&gt; &lt;message&gt;</b></td></tr>"#,
        );
    }

    let mut task_runs_html = String::new();
    for r in task_runs.iter().rev().take(20) {
        let status_class = match r.status {
            crate::scheduler::TaskRunStatus::Ok | crate::scheduler::TaskRunStatus::Revived => "status-ok",
            crate::scheduler::TaskRunStatus::Failed => "status-fail",
        };
        let status_text = match r.status {
            crate::scheduler::TaskRunStatus::Ok => "ok",
            crate::scheduler::TaskRunStatus::Revived => "revived",
            crate::scheduler::TaskRunStatus::Failed => "failed",
        };
        let error = r.error.as_deref().unwrap_or("");
        task_runs_html.push_str(&format!(
            "<tr><td class=\"dim\">{}</td><td>{}</td><td>{}</td><td class=\"{status_class}\">{status_text}</td><td class=\"msg-cell\">{}</td></tr>",
            r.timestamp.format("%H:%M:%S"),
            html_escape(&r.task_name),
            html_escape(&r.target_session),
            html_escape(error),
        ));
    }
    if task_runs.is_empty() {
        task_runs_html.push_str(
            r#"<tr><td colspan="5" class="empty">No task runs yet.</td></tr>"#,
        );
    }

    let mut log_html = String::new();
    for entry in log.iter().rev().take(50) {
        let (status_icon, status_class) = if entry.delivered {
            ("&#10003;", "status-ok")
        } else {
            ("&#10007;", "status-fail")
        };
        log_html.push_str(&format!(
            "<tr><td class=\"dim\">{}</td><td>{}</td><td>{}</td><td class=\"msg-cell\">{}</td><td class=\"{status_class}\">{status_icon}</td></tr>",
            entry.timestamp.format("%H:%M:%S"),
            html_escape(&entry.from),
            html_escape(&entry.to),
            html_escape(&entry.message),
        ));
    }

    let p2p_status = if any_ready {
        let names: Vec<&str> = transports
            .values()
            .filter(|t| t.is_ready())
            .map(|t| t.transport_name())
            .collect();
        format!(
            r#"<span class="dot dot-on"></span> P2P ready <span class="dim">({})</span>"#,
            html_escape(&names.join(", "))
        )
    } else {
        r#"<span class="dot dot-off"></span> P2P initializing..."#.to_string()
    };

    let peers_empty = if peer_count == 0 {
        r#"<tr><td colspan="3" class="empty">No peers connected. Use the pairing section below to connect another machine.</td></tr>"#
    } else {
        ""
    };

    let log_empty = if msg_count == 0 {
        r#"<tr><td colspan="5" class="empty">No messages yet. Send one with <b>peer_send</b> from a Claude session.</td></tr>"#
    } else {
        ""
    };

    let saved_relays = crate::nostr_transport::load_relays(&state.config.data_dir);
    let saved_relays_json = serde_json::to_string(&saved_relays).unwrap_or_else(|_| "[]".into());
    let default_relay = saved_relays.first().cloned().unwrap_or_default();

    let html = format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>ouija — {name}</title>
<link rel="icon" href="data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100'><text y='.9em' font-size='90'>🔮</text></svg>">
<style>
@import url('https://fonts.googleapis.com/css2?family=JetBrains+Mono:wght@400;600;700&family=DM+Sans:wght@500;700&display=swap');

* {{ margin: 0; padding: 0; box-sizing: border-box; }}

body {{
  font-family: 'JetBrains Mono', monospace;
  font-size: 13px;
  background: #0c0e14;
  color: #c4c9d4;
  min-height: 100vh;
}}

.shell {{
  max-width: 960px;
  margin: 0 auto;
  padding: 24px 32px 48px;
}}

/* --- Header bar --- */
.header {{
  display: flex;
  align-items: baseline;
  justify-content: space-between;
  border-bottom: 1px solid #1e2230;
  padding-bottom: 16px;
  margin-bottom: 24px;
}}

.header h1 {{
  font-family: 'DM Sans', sans-serif;
  font-size: 22px;
  font-weight: 700;
  color: #e8ecf1;
  letter-spacing: -0.5px;
}}

.header h1 span {{
  color: #3ecf8e;
}}

.status-bar {{
  display: flex;
  gap: 20px;
  font-size: 11px;
  color: #6b7280;
}}

.status-bar .item {{
  display: flex;
  align-items: center;
  gap: 6px;
}}

.dot {{
  width: 7px;
  height: 7px;
  border-radius: 50%;
  display: inline-block;
}}

.dot-on {{
  background: #3ecf8e;
  box-shadow: 0 0 6px #3ecf8e88;
}}

.dot-off {{
  background: #f59e0b;
  animation: pulse 1.5s ease-in-out infinite;
}}

@keyframes pulse {{
  0%, 100% {{ opacity: 1; }}
  50% {{ opacity: 0.4; }}
}}

/* --- Sections --- */
.section {{
  margin-bottom: 28px;
}}

.section-head {{
  display: flex;
  align-items: center;
  gap: 10px;
  margin-bottom: 8px;
}}

.section-head h2 {{
  font-family: 'DM Sans', sans-serif;
  font-size: 13px;
  font-weight: 700;
  color: #8b93a1;
  text-transform: uppercase;
  letter-spacing: 1.2px;
}}

.count {{
  font-size: 11px;
  color: #3ecf8e;
  background: #3ecf8e15;
  padding: 1px 7px;
  border-radius: 8px;
}}

/* --- Tables --- */
table {{
  width: 100%;
  border-collapse: collapse;
}}

th {{
  font-size: 11px;
  font-weight: 600;
  color: #4b5263;
  text-transform: uppercase;
  letter-spacing: 0.8px;
  text-align: left;
  padding: 6px 12px;
  border-bottom: 1px solid #1e2230;
}}

td {{
  padding: 7px 12px;
  border-bottom: 1px solid #13151d;
}}

tr:hover td {{
  background: #12141c;
}}

.id-cell {{
  color: #e8ecf1;
  font-weight: 600;
}}

.dim {{
  color: #4b5263;
}}

.btn-sm {{
  font-size: 11px;
  padding: 2px 8px;
  background: #1e2230;
  color: #6b7280;
  border: 1px solid #2e3340;
  border-radius: 3px;
  cursor: pointer;
  font-family: 'JetBrains Mono', monospace;
}}

.btn-sm:hover {{
  background: #262b3a;
  color: #c4c9d4;
}}

.btn-danger:hover {{
  background: #3b1c1c;
  color: #ef4444;
  border-color: #ef444444;
}}

.msg-cell {{
  max-width: 340px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: #9ca3af;
}}

.empty {{
  color: #3e4451;
  font-style: italic;
  padding: 16px 12px;
  text-align: center;
}}

.empty b {{
  color: #6b7280;
  font-style: normal;
}}

/* --- Badges --- */
.badge {{
  font-size: 11px;
  padding: 2px 8px;
  border-radius: 4px;
  font-weight: 600;
}}

.badge-local {{
  color: #3ecf8e;
  background: #3ecf8e12;
  border: 1px solid #3ecf8e30;
}}

.badge-remote {{
  color: #818cf8;
  background: #818cf812;
  border: 1px solid #818cf830;
}}

/* --- Status icons --- */
.status-ok {{
  color: #3ecf8e;
  text-align: center;
}}

.status-fail {{
  color: #ef4444;
  text-align: center;
}}

/* --- Pairing --- */
.pairing {{
  background: #11131a;
  border: 1px solid #1e2230;
  border-radius: 8px;
  padding: 16px 20px;
  margin-bottom: 28px;
}}

.pairing h2 {{
  font-family: 'DM Sans', sans-serif;
  font-size: 13px;
  font-weight: 700;
  color: #8b93a1;
  text-transform: uppercase;
  letter-spacing: 1.2px;
  margin-bottom: 10px;
}}

.pairing .warn {{
  font-size: 11px;
  color: #f59e0b;
  margin-bottom: 12px;
  display: flex;
  align-items: center;
  gap: 6px;
}}

.ticket-value {{
  word-break: break-all;
  font-size: 11px;
  color: #6b7280;
  line-height: 1.5;
  margin-bottom: 10px;
}}

.ticket-actions {{
  display: flex;
  gap: 8px;
}}

.connect-row {{
  display: flex;
  gap: 8px;
  margin-top: 12px;
}}

.connect-row input {{
  flex: 1;
  background: #0c0e14;
  color: #c4c9d4;
  border: 1px solid #1e2230;
  border-radius: 4px;
  padding: 7px 10px;
  font-family: 'JetBrains Mono', monospace;
  font-size: 12px;
  outline: none;
  transition: border-color 0.15s;
}}

.connect-row input:focus {{
  border-color: #3e4451;
}}

.connect-row input::placeholder {{
  color: #2e3340;
}}

.connect-row button {{
  background: #1e2230;
  color: #c4c9d4;
  border: 1px solid #2e3340;
  border-radius: 4px;
  padding: 7px 16px;
  font-family: 'JetBrains Mono', monospace;
  font-size: 12px;
  cursor: pointer;
  transition: background 0.15s, border-color 0.15s;
  white-space: nowrap;
}}

.connect-row button:hover {{
  background: #262b3a;
  border-color: #3e4451;
}}

#connect-result {{
  font-size: 12px;
  margin-top: 8px;
  min-height: 18px;
}}

.label {{
  font-size: 11px;
  color: #4b5263;
  margin-bottom: 4px;
}}

/* --- Relay config --- */
.relay-config {{
  margin-top: 16px;
  padding-top: 16px;
  border-top: 1px solid #1e2230;
}}

.relay-header {{
  display: flex;
  align-items: baseline;
  gap: 10px;
  margin-bottom: 10px;
}}

.relay-label {{
  font-size: 11px;
  font-weight: 600;
  color: #8b93a1;
  text-transform: uppercase;
  letter-spacing: 0.8px;
}}

.relay-item {{
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 5px 0;
}}

.relay-item code {{
  font-size: 12px;
  color: #9ca3af;
  flex: 1;
}}

.relay-item .btn-sm {{
  opacity: 0.5;
  transition: opacity 0.15s;
}}

.relay-item:hover .btn-sm {{
  opacity: 1;
}}
</style>
</head>
<body>
<div class="shell">

<div class="header">
  <h1><span>ouija</span> / {name}</h1>
  <div class="status-bar">
    <div class="item">{p2p_status}</div>
    <div class="item"><span class="dim">port</span> {port}</div>
  </div>
</div>

<div class="section">
  <div class="section-head">
    <h2>Local Sessions</h2>
    <span class="count">{local_count}</span>
  </div>
  <table>
    <tr><th>ID</th><th>Pane</th><th>Registered</th><th></th></tr>
    {sessions_html}
  </table>
</div>

<div class="section">
  <div class="section-head">
    <h2>Remote Sessions</h2>
    <span class="count">{remote_count}</span>
  </div>
  <table>
    <tr><th>ID</th><th>Daemon</th><th>Discovered</th></tr>
    {remote_sessions_html}
  </table>
</div>

<div class="section">
  <div class="section-head">
    <h2>Peers</h2>
    <span class="count">{peer_count}</span>
  </div>
  <table>
    <tr><th>Name</th><th>Daemon ID</th><th>Connected</th></tr>
    {peers_html}
    {peers_empty}
  </table>
</div>

<div class="section">
  <div class="section-head">
    <h2>Scheduled Tasks</h2>
    <span class="count">{task_count}</span>
  </div>
  <table>
    <tr><th>ID</th><th>Name</th><th>Cron</th><th>Target</th><th style="text-align:center;">Enabled</th><th>Next Run</th><th>Last Run</th><th>Status</th><th>Runs</th><th></th></tr>
    {tasks_html}
  </table>
</div>

<div class="section">
  <div class="section-head">
    <h2>Recent Task Runs</h2>
    <span class="count">{task_run_count}</span>
  </div>
  <table>
    <tr><th>Time</th><th>Task</th><th>Target</th><th>Status</th><th>Error</th></tr>
    {task_runs_html}
  </table>
</div>

<div class="pairing">
  <h2>Pairing</h2>
  <div class="warn">&#9888; Tickets are secrets. Only pair with machines you trust.</div>
  {ticket_section}
  <div class="label">Connect to peer</div>
  <form onsubmit="connectPeer(event)">
    <div class="connect-row">
      <input type="text" id="ticket-input" placeholder="Paste a ticket from another machine" autocomplete="off">
      <button type="submit">Connect</button>
    </div>
  </form>
  <div id="connect-result"></div>
</div>

<div class="section">
  <div class="section-head">
    <h2>Messages</h2>
    <span class="count">{msg_count}</span>
  </div>
  <table>
    <tr><th>Time</th><th>From</th><th>To</th><th>Message</th><th style="text-align:center;">Status</th></tr>
    {log_html}
    {log_empty}
  </table>
</div>

<div class="card">
  <div class="card-header">
    <h2>Settings</h2>
  </div>
  <table>
    <tr>
      <th>Setting</th><th style="text-align:center;">Value</th>
    </tr>
    <tr>
      <td>Auto-register sessions on startup</td>
      <td style="text-align:center;">
        <input type="checkbox" id="auto-register" {auto_register_checked} onchange="updateSetting('auto_register', this.checked)">
      </td>
    </tr>
  </table>

  <div class="relay-config">
    <div class="relay-header">
      <span class="relay-label">Nostr relays</span>
      <span class="dim" style="font-size:11px;">Used for nostr ticket generation and P2P connections</span>
    </div>
    <div id="relay-list"></div>
    <div class="connect-row" style="margin-top:8px;">
      <input type="text" id="new-relay-input" placeholder="wss://relay.example.com" autocomplete="off">
      <button onclick="addRelay()">Add</button>
    </div>
  </div>
</div>

</div>

<script>
// Relay management — bootstrap from server-rendered data
var savedRelays = {saved_relays_json};
async function connectPeer(e) {{
  e.preventDefault();
  const ticket = document.getElementById('ticket-input').value.trim();
  if (!ticket) return;
  const el = document.getElementById('connect-result');
  el.textContent = 'Connecting...';
  el.style.color = '#6b7280';
  try {{
    const resp = await fetch('/api/connect', {{
      method: 'POST',
      headers: {{'Content-Type': 'application/json'}},
      body: JSON.stringify({{ticket}})
    }});
    const data = await resp.json();
    if (resp.ok) {{
      el.style.color = '#3ecf8e';
      el.textContent = 'Connected to ' + (data.peers || 0) + ' peer(s)';
      document.getElementById('ticket-input').value = '';
    }} else {{
      el.style.color = '#ef4444';
      el.textContent = 'Error: ' + (data.error || 'unknown');
    }}
  }} catch(err) {{
    el.style.color = '#ef4444';
    el.textContent = 'Error: ' + err.message;
  }}
}}

async function renameSession(oldId) {{
  const newId = prompt('Rename session "' + oldId + '" to:');
  if (!newId || newId === oldId) return;
  try {{
    const resp = await fetch('/api/rename', {{
      method: 'POST',
      headers: {{'Content-Type': 'application/json'}},
      body: JSON.stringify({{old_id: oldId, new_id: newId}})
    }});
    if (resp.ok) location.reload();
    else {{
      const data = await resp.json();
      alert('Error: ' + (data.error || 'unknown'));
    }}
  }} catch(err) {{ alert('Error: ' + err.message); }}
}}

async function removeSession(id) {{
  if (!confirm('Remove session "' + id + '"?')) return;
  try {{
    const resp = await fetch('/api/remove', {{
      method: 'POST',
      headers: {{'Content-Type': 'application/json'}},
      body: JSON.stringify({{id}})
    }});
    if (resp.ok) location.reload();
    else {{
      const data = await resp.json();
      alert('Error: ' + (data.error || 'unknown'));
    }}
  }} catch(err) {{ alert('Error: ' + err.message); }}
}}

async function deleteTask(id) {{
  if (!confirm('Delete task "' + id + '"?')) return;
  try {{
    const resp = await fetch('/api/tasks', {{
      method: 'DELETE',
      headers: {{'Content-Type': 'application/json'}},
      body: JSON.stringify({{id}})
    }});
    if (resp.ok) location.reload();
    else {{
      const data = await resp.json();
      alert('Error: ' + (data.error || 'unknown'));
    }}
  }} catch(err) {{ alert('Error: ' + err.message); }}
}}

async function enableTask(id) {{
  try {{
    const resp = await fetch('/api/tasks/enable', {{
      method: 'POST',
      headers: {{'Content-Type': 'application/json'}},
      body: JSON.stringify({{id}})
    }});
    if (!resp.ok) {{
      const data = await resp.json();
      alert('Error: ' + (data.error || 'unknown'));
      location.reload();
    }}
  }} catch(err) {{ alert('Error: ' + err.message); location.reload(); }}
}}

async function disableTask(id) {{
  try {{
    const resp = await fetch('/api/tasks/disable', {{
      method: 'POST',
      headers: {{'Content-Type': 'application/json'}},
      body: JSON.stringify({{id}})
    }});
    if (!resp.ok) {{
      const data = await resp.json();
      alert('Error: ' + (data.error || 'unknown'));
      location.reload();
    }}
  }} catch(err) {{ alert('Error: ' + err.message); location.reload(); }}
}}

async function triggerTask(id) {{
  try {{
    const resp = await fetch('/api/tasks/trigger', {{
      method: 'POST',
      headers: {{'Content-Type': 'application/json'}},
      body: JSON.stringify({{id}})
    }});
    const data = await resp.json();
    if (resp.ok) {{
      alert('Task triggered');
      location.reload();
    }} else {{
      alert('Error: ' + (data.error || 'unknown'));
    }}
  }} catch(err) {{ alert('Error: ' + err.message); }}
}}

function copyTicket(btn) {{
  const text = btn.closest('.pairing').querySelector('.ticket-value').textContent.trim();
  navigator.clipboard.writeText(text).then(() => {{
    const orig = btn.textContent;
    btn.textContent = 'copied!';
    setTimeout(() => btn.textContent = orig, 1500);
  }});
}}

async function generateNostrTicket() {{
  const input = document.getElementById('nostr-relay-input');
  const relay = input.value.trim();
  if (!relay) return;
  const el = document.getElementById('nostr-result');
  el.textContent = 'Generating...';
  el.style.color = '#6b7280';
  try {{
    const resp = await fetch('/api/ticket?relay=' + encodeURIComponent(relay));
    const data = await resp.json();
    if (resp.ok && data.ticket) {{
      el.style.color = '#3ecf8e';
      el.textContent = 'Ticket generated. Reloading...';
      location.reload();
    }} else {{
      el.style.color = '#ef4444';
      el.textContent = 'Error: ' + (data.error || 'unknown');
    }}
  }} catch(err) {{
    el.style.color = '#ef4444';
    el.textContent = 'Error: ' + err.message;
  }}
}}

async function regenerateTransport() {{
  if (!confirm('This will DESTROY your nostr identity (nsec). All peers must re-connect.')) return;
  try {{
    const resp = await fetch('/api/regenerate-ticket?confirm=true', {{method:'POST'}});
    const data = await resp.json();
    if (resp.ok) {{
      alert('New ticket generated. Reload to see it.');
      location.reload();
    }} else {{
      alert('Error: ' + (data.error || 'unknown'));
    }}
  }} catch(err) {{ alert('Error: ' + err.message); }}
}}

async function updateSetting(key, value) {{
  try {{
    const resp = await fetch('/api/settings', {{
      method: 'POST',
      headers: {{'Content-Type': 'application/json'}},
      body: JSON.stringify({{[key]: value}})
    }});
    if (!resp.ok) {{
      const data = await resp.json();
      alert('Error: ' + (data.error || 'unknown'));
      location.reload();
    }}
  }} catch(err) {{
    alert('Error: ' + err.message);
    location.reload();
  }}
}}

function renderRelays() {{
  const list = document.getElementById('relay-list');
  if (!list) return;
  if (savedRelays.length === 0) {{
    list.innerHTML = '<div class="dim" style="font-size:12px; padding:4px 0;">No relays configured.</div>';
    return;
  }}
  list.innerHTML = savedRelays.map((r, i) =>
    '<div class="relay-item"><code>' + r.replace(/</g, '&lt;') + '</code>' +
    '<button class="btn-sm btn-danger" onclick="removeRelay(' + i + ')">remove</button></div>'
  ).join('');
}}

async function addRelay() {{
  const input = document.getElementById('new-relay-input');
  const url = input.value.trim();
  if (!url) return;
  if (!url.startsWith('wss://') && !url.startsWith('ws://')) {{
    alert('Relay URL must start with wss:// or ws://');
    return;
  }}
  if (savedRelays.includes(url)) {{ input.value = ''; return; }}
  savedRelays.push(url);
  await saveRelays();
  input.value = '';
  renderRelays();
}}

async function removeRelay(idx) {{
  savedRelays.splice(idx, 1);
  await saveRelays();
  renderRelays();
}}

async function saveRelays() {{
  try {{
    const resp = await fetch('/api/relays', {{
      method: 'POST',
      headers: {{'Content-Type': 'application/json'}},
      body: JSON.stringify({{relays: savedRelays}})
    }});
    if (!resp.ok) {{
      const data = await resp.json();
      alert('Error: ' + (data.error || 'unknown'));
    }}
  }} catch(err) {{
    alert('Error saving relays: ' + err.message);
  }}
}}

renderRelays();

// Poll for updates without full page reload
setInterval(async () => {{
  try {{
    const resp = await fetch('/api/status');
    if (resp.ok) {{
      // Lightweight: just update the title to show liveness
      document.title = 'ouija — {name}';
    }}
  }} catch(_) {{
    document.title = 'ouija — {name} (offline)';
  }}
}}, 5000);
</script>
</body>
</html>"#,
        name = html_escape(&state.config.name),
        port = state.config.port,
        p2p_status = p2p_status,
        local_count = local_count,
        remote_count = remote_count,
        peer_count = peer_count,
        msg_count = msg_count,
        sessions_html = sessions_html,
        remote_sessions_html = remote_sessions_html,
        peers_html = peers_html,
        peers_empty = peers_empty,
        task_count = task_count,
        tasks_html = tasks_html,
        task_run_count = task_run_count,
        task_runs_html = task_runs_html,
        log_html = log_html,
        log_empty = log_empty,
        saved_relays_json = saved_relays_json,
        auto_register_checked = if settings.auto_register { "checked" } else { "" },
        ticket_section = {
            let nostr_ticket = transports.get("nostr").and_then(|t| t.ticket_string());

            if let Some(ticket) = &nostr_ticket {
                format!(
                    r#"<div class="label">Your ticket</div>
<div class="ticket-value">{ticket}</div>
<div class="ticket-actions">
  <button class="btn-sm" onclick="copyTicket(this)">copy</button>
  <button class="btn-sm btn-danger" onclick="regenerateTransport('nostr')">regenerate</button>
</div>"#,
                    ticket = html_escape(ticket),
                )
            } else {
                format!(r#"<div class="nostr-setup">
  <div class="dim" style="font-size:12px; margin-bottom:8px;">Enter relay URL to generate a nostr ticket:</div>
  <div class="connect-row" style="margin-top:0;">
    <input type="text" id="nostr-relay-input" placeholder="wss://relay.example.com" value="{default_relay}" autocomplete="off">
    <button onclick="generateNostrTicket()">Generate</button>
  </div>
  <div id="nostr-result" style="font-size:12px; margin-top:8px; min-height:18px;"></div>
</div>"#, default_relay = html_escape(&default_relay))
            }
        },
    );

    Html(html)
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

