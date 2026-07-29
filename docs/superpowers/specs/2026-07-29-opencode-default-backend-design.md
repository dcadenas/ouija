# OpenCode Default Backend Design

## Goal

When a caller omits Ouija's `--backend` selection, use OpenCode instead of
Claude Code.

## Design

Change only the default name used by `BackendRegistry::default_registry()`
from `claude-code` to `opencode`. The registry remains the single fallback
source for CLI, API, scheduler, and lifecycle paths, so the behavior stays
consistent. Explicit backend arguments and backend metadata already stored on
an existing session continue to take precedence.

Adding a persistent `default_backend` setting and configuration UI is a
separate follow-up owned by an isolated worker.

## Verification

Add a focused unit test that observes `default_registry().default().name()`.
Run that test red before the implementation, then run the backend module tests,
formatting, and targeted clippy/build verification. Do not run ignored
Stateright or unrelated end-to-end suites.
