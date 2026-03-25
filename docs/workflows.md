# Workflow Actors

A workflow actor is an external program that drives an LLM session through a deterministic process. The LLM calls a `workflow()` tool; the program controls state, verification, and progression. Think of it as a process engine where the LLM is the task worker.

## The idea

Most agent harnesses work like this: code calls the LLM API, gets a response, decides the next step, calls the LLM again. The code is the orchestrator; the LLM is a stateless function.

Ouija inverts this. The LLM is a persistent, autonomous session with full tool access. But it needs guidance — left alone, it drifts, skips steps, or declares victory early. A workflow actor provides that guidance without taking away the LLM's autonomy within each step.

The pattern is the same as an MCP tool: the LLM calls deterministic code and gets a response. But this "tool" is custom per session — set at start time, managing its own state, controlling what the LLM does next. The LLM operates inside the steps; the workflow operates between them.

```
Classical harness:    Code → LLM API → Code → LLM API → Code → ...
                      (code is the loop, LLM is a function)

Workflow actor:       LLM → workflow() → LLM → workflow() → LLM → ...
                      (LLM is the loop, workflow is a function)
```

Same control flow, inverted. One continuous LLM session instead of many API calls. The LLM retains full context and tool access. The workflow provides deterministic checkpoints.

## Progressive disclosure

A workflow doesn't explain itself upfront. It reveals the process one step at a time, like [HATEOAS](https://en.wikipedia.org/wiki/HATEOAS) in REST APIs — the response tells the client what it can do next, so the client never needs a map of the full API.

```
LLM: workflow('init')
  → "Implement the auth module. Call workflow('chunk_done', {chunk: 'auth'}) when finished."

LLM: workflow('chunk_done', {chunk: 'auth'})
  → "Tests pass. Next: implement the logging module. Call workflow('chunk_done', {chunk: 'logging'})."

LLM: workflow('chunk_done', {chunk: 'logging'})
  → "All chunks done. Review your diff, then call workflow('verify', {summary: '...'})."
```

The LLM didn't know about the verify phase until it got there. The prompt didn't describe a three-phase process. Each response disclosed exactly what was needed — no more, no less.

This is the same principle as BPM systems (Camunda, Activiti): the task participant sees only their current task form, not the process diagram. The engine routes work based on outcomes.

### Why this matters

- **Less context consumed** — the LLM holds only the current step, not a full process description
- **Less drift** — the LLM can't skip ahead because it doesn't know what's ahead
- **Survives restarts** — `workflow('init')` reconstructs state from the state file; no prompt memory needed
- **Adaptable** — the workflow can change the next step based on results without conflicting with the LLM's cached understanding

### Three levels of context

Information reaches the LLM at three levels:

| Level | What | When loaded | Purpose |
|---|---|---|---|
| 1. Tool description | The `workflow` MCP tool description | Always in context | Tells the LLM the tool exists and how to call it |
| 2. Registration instructions | From the workflow's `register` response | At session start | Orients the LLM — purpose, rhythm, constraints |
| 3. Runtime responses | From each `workflow()` call | On demand | Step-specific: current state, next task, verification criteria |

Level 1 helps recognize. Level 2 orients. Level 3 directs. Don't bleed between levels — if you're putting step-specific detail in registration instructions, move it to a runtime response.

## How it works

### Protocol

A workflow is any executable that reads JSON from stdin and writes JSON to stdout. The daemon spawns it once per interaction (stateless process, stateful files).

**Registration** (called by the daemon at `session_start`):
```json
// stdin
{"event": "register", "session_id": "worker-1", "params": {"issue_id": 123}}

// stdout
{"instructions": "You are a worker...", "inject_on_start": "Call workflow('init').", "max_calls": 200}
```

**Runtime** (called when the LLM uses the `workflow()` MCP tool):
```json
// stdin
{"action": "chunk_done", "session_id": "worker-1", "params": {"chunk": "auth"}}

// stdout
{"message": "Tests pass. Next: implement logging.", "verify": "cargo test --lib logging passes"}
```

**Lifecycle events** (called by the daemon on session death/restart):
```json
// stdin
{"event": "session_died", "session_id": "worker-1"}

// stdout
{}
```

### What the daemon provides

- **MCP tool**: Routes LLM `workflow()` calls to the executable, injects trusted `session_id`
- **Registration**: Calls the workflow at session start, merges instructions into the prompt
- **Effort budgets**: Enforces `max_calls` from registration — refuses further calls when exhausted
- **Lifecycle events**: Notifies the workflow when sessions die or restart
- **Stall detection**: If the LLM stops calling the workflow, the daemon injects the reminder
- **Serialization**: Per-workflow mutex prevents concurrent state file corruption
- **Push channel**: The workflow calls ouija's REST API to inject messages, spawn sessions, or send notifications asynchronously

### What the daemon does NOT do

- Interpret workflow responses (just passes them through)
- Know the workflow's state machine (black box)
- Manage the workflow's state (the workflow owns its state files)
- Decide when to restart or stop (the workflow communicates this via its responses)

## The bidirectional channel

The workflow communicates with LLM sessions in two directions:

**LLM → Workflow** (synchronous, via MCP tool): The LLM calls `workflow('action')`, the daemon pipes to the executable, returns the response. This is request-response — the LLM initiates.

**Workflow → LLM** (asynchronous, via ouija REST API): The workflow calls `POST /api/inject` to push text into any session at any time. This is how a reviewer's approval can wake up an idle worker, or how a coordinator can dispatch new tasks.

```python
# Inside a workflow script — push a message to another session
import requests, os
requests.post(f"{os.environ['OUIJA_API']}/api/inject", json={
    "pane": worker_pane_id,
    "message": "Review approved. Call workflow('init') to continue."
})
```

Without the async push channel, the workflow would be limited to request-response — a polling-based model where the LLM must keep calling `workflow('status')` to check for updates. The ouija REST API makes it reactive.

## Multi-session coordination

Multiple LLM sessions can share one workflow. The workflow distinguishes them by `session_id` and manages their state independently:

```
                  ┌──────────────┐
                  │   Workflow    │
                  │  (one script) │
                  │   state.json  │
                  └──┬───┬───┬──┘
                     │   │   │
              ┌──────┘   │   └──────┐
              ↕          ↕          ↕
         ┌─────────┐ ┌─────────┐ ┌──────────┐
         │ Worker  │ │ Worker  │ │ Reviewer │
         │  (LLM)  │ │  (LLM)  │ │  (LLM)   │
         └─────────┘ └─────────┘ └──────────┘
```

Each session calls `workflow('init')` and gets role-appropriate instructions. The workflow is the coordinator — no coordinator LLM session needed, zero tokens spent on orchestration logic.

The workflow can:
- Assign different roles at registration based on `workflow_params`
- Gate worker progress on reviewer approval
- Spawn new sessions via the REST API
- Track a kanban board, manage concurrent slots
- Interact with external systems (Forgejo, GitHub, Slack) deterministically

This replaces patterns where a coordinator LLM session reads a prompt, calls `session_start` to spawn workers, polls for `done:` messages, and manages state through conversation context. The workflow script does all of this with deterministic code.

## Verification

Workflow responses can include a `verify` field with machine-checkable success criteria:

```json
{
  "message": "Implement the rate limiter for the /api/upload endpoint.",
  "verify": "cargo test --lib rate_limiter passes with 0 failures"
}
```

The daemon appends this to the message: "Verify before proceeding: cargo test --lib rate_limiter passes with 0 failures." The LLM runs the check before calling the next workflow action.

This is the VERIFY phase from gather-act-verify-repeat loops. The workflow defines what "done" means; the LLM checks it. If verification fails, the LLM fixes the issue before proceeding — the workflow never sees an unverified result.

## Effort budgets

Workflows set a `max_calls` limit at registration. The daemon enforces it — after the limit, further `workflow()` calls return an error. This prevents the biggest failure mode in multi-agent systems: unbounded looping.

The limit is set by the workflow (deterministic code), not the LLM (probabilistic). The LLM can't override it.

## Writing workflows

See [`examples/`](../examples/) for a reference implementation and authoring guide covering the protocol, progressive disclosure, and patterns for good registration instructions.

A workflow can be written in any language. It reads one JSON object from stdin, writes one JSON object to stdout, and exits. State goes in files. The daemon handles everything else.
