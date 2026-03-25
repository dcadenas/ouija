# Workflow Examples

## autoresearch-workflow.py

An optimization/research loop workflow that drives an LLM session through iterative changes. Each iteration: read state, make one change, measure, keep or revert.

### Project setup

Your project directory needs:

- **INSTRUCTIONS.md** - What to optimize and how to measure (e.g., "minimize inference latency; run `python bench.py` and report the p99 value")
- A git repo with a clean working tree (the workflow commits improvements and reverts regressions)

The workflow creates these files automatically:

- **results.tsv** - Tab-separated log of all iterations (score, description, outcome)
- **FINDINGS.md** - Accumulated insights that survive context restarts
- **workflow-state.json** - Internal state (iteration counter, best score)

### Starting a session

```
session_start(
  name="optimizer",
  workflow="examples/autoresearch-workflow.py",
  workflow_params={"max_iterations": 30},
  prompt="Read INSTRUCTIONS.md for your task.",
  project_dir="/path/to/project"
)
```

Parameters:
- `workflow` - Path to the workflow script (relative to project_dir or absolute)
- `workflow_params` - `max_iterations` (default: 50)
- `prompt` - Additional prompt text (merged with workflow instructions)
- `project_dir` - Working directory for the session

### How it works

1. On `session_start`, the daemon calls `register` on the workflow. It returns instructions describing the `init`, `result`, `findings`, and `status` actions.
2. The session starts and calls `workflow("init")` to get its first task.
3. The LLM reads INSTRUCTIONS.md, makes one change, measures, and reports via `workflow("result", {score, description})`.
4. The workflow commits improvements to git and reverts regressions, then prompts the next iteration.
5. After `max_iterations`, the workflow tells the session to finish.

### Writing your own workflow

A workflow is any executable that speaks the JSON-over-stdin/stdout protocol:

**Input** (one JSON object on stdin):
```json
{"event": "register", "session_id": "optimizer", "params": {"max_iterations": 30}}
{"action": "init", "session_id": "optimizer", "params": null}
{"action": "result", "session_id": "optimizer", "params": {"score": 0.95, "description": "..."}}
{"event": "session_died", "session_id": "optimizer"}
```

**Output** (one JSON object on stdout):
```json
{"instructions": "...", "inject_on_start": "Call workflow('init') to begin."}
{"message": "Iteration 1/50. Make one change..."}
{"error": "missing required param: score"}
{}
```

The daemon spawns the workflow once per interaction. The process starts, reads stdin, writes stdout, and exits. State must be persisted to disk (the workflow's working directory is the session's `project_dir`).

Environment variables available: `OUIJA_API` (daemon URL), `OUIJA_SESSION_ID`.

Registration can also return:
- `max_calls` (number) — daemon-enforced call budget. The daemon refuses further workflow calls after this limit, preventing unbounded looping.
- `verify` field in runtime responses — machine-checkable success criteria appended to the message so the LLM knows how to verify its work before proceeding.

### Progressive disclosure: three levels of context

Workflow information reaches the LLM at three levels. Be intentional about what goes where:

**Level 1 — Tool description** (always in context): The `workflow` MCP tool description tells the LLM *when* and *how* to call the workflow. This is built into ouija and always visible. The LLM uses it to decide whether to call the workflow at all.

**Level 2 — Registration instructions** (loaded at session start): The `instructions` field from your workflow's `register` response. This is prepended to the session prompt. Write it like an **onboarding guide for a new team member**, not a technical specification:
- Explain the purpose and rhythm ("you're optimizing X, one change at a time")
- List the available actions and what they do, briefly
- Describe what success looks like
- Mention key constraints ("always measure before reporting, never skip verification")
- Keep it compact — specific details belong in Level 3

**Level 3 — Runtime responses** (loaded on demand): Each `workflow()` call returns step-specific instructions. This is where detailed context goes — current state, next task, verification criteria, recent history. The LLM gets this just-in-time, only when it needs it.

The principle: Level 1 helps recognize, Level 2 orients, Level 3 directs. Don't bleed information between levels — if you find yourself putting step-specific detail in registration instructions, move it to a runtime response instead.

### Writing good registration instructions

Think of `instructions` as onboarding a junior developer to a process:

**Good:**
```
You are running an optimization loop. Your job: make one change at a time,
measure the result, and report it. The workflow handles git state — it commits
improvements and reverts regressions.

Actions: init (get next task), result (report outcome), findings (save insight), status (check state).

Call workflow('init') after every restart. Always measure before calling 'result'.
```

**Too verbose** (front-loads detail that belongs in runtime responses):
```
You are running an optimization loop. On iteration 1, read INSTRUCTIONS.md and
FINDINGS.md. The INSTRUCTIONS.md file contains... The FINDINGS.md file contains...
results.tsv has columns: iteration, score, description, kept. When calling result,
provide score as a float and description as a string. If score > best_score the
workflow will run git add -A && git commit...
```

The second version wastes context window space with detail the LLM only needs when it's actually at that step — and which the `init` response already provides.
