# OpenCode Default Backend Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make OpenCode the fallback whenever no Ouija backend is selected.

**Architecture:** Keep `BackendRegistry` as the single default-selection
boundary. Change its default name without changing explicit selections or
persisted per-session backend metadata.

**Tech Stack:** Rust, Cargo unit tests.

## Global Constraints

- Preserve exact-owner lifecycle and backend-binding invariants.
- Do not add the persistent setting or UI in this slice.
- Do not run ignored Stateright or unrelated end-to-end tests.

---

### Task 1: Change the registry fallback

**Files:**
- Modify: `src/backend/mod.rs`
- Test: `src/backend/mod.rs`

**Interfaces:**
- Consumes: `BackendRegistry::default_registry()` and `BackendRegistry::default()`
- Produces: an `opencode` default backend when no caller-specific selection exists

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn default_registry_uses_opencode() {
    let registry = BackendRegistry::default_registry();
    assert_eq!(registry.default().name(), "opencode");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run:
`cargo test backend::tests::default_registry_uses_opencode -- --exact`

Expected: FAIL because the current value is `claude-code`.

- [ ] **Step 3: Write the minimal implementation**

In `BackendRegistry::default_registry()`, change the `BackendRegistry::new`
default argument from `"claude-code"` to `"opencode"`.

- [ ] **Step 4: Run focused verification**

Run:

```bash
cargo test backend::tests::default_registry_uses_opencode -- --exact
cargo test backend::tests
cargo fmt --check
cargo clippy --all-targets --all-features
cargo build
```

Expected: every command exits successfully.

- [ ] **Step 5: Review and commit**

Inspect `git diff --check`, `git diff`, and `git status --short`; commit only
the design, plan, focused test, and default change.
