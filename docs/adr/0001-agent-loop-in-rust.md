# ADR-0001: Agent loop, runs and approvals live in Rust

Status: accepted (2026-11-11)

## Context

The desktop app is Tauri 2 with a thin webview UI. The agent needs to stream model
output, call tools on the local filesystem and shell, and pause for user approval.
Two designs were possible: the frontend drives the model/tool loop and calls Tauri
commands as tools, or Rust owns the loop and the UI is a renderer.

The UI-driven design loses all run state when the window closes and puts the
safety-critical parts (path containment, shell execution, approval gating) in
JavaScript.

## Decision

Rust owns the agent loop. The workspace is a Cargo workspace: `crates/aria-core`
(DB, Provider client, Run loop, Tools, approvals) with no Tauri dependency, and
`src-tauri` as a thin adapter exposing commands and a typed `RunEvent` stream.

Runs are **detached**: the DB is the source of truth for run state, the UI
resubscribes on open (`get_run_state`). **Approvals** are persisted rows with no
timeout; they park the Run until answered or cancelled. Every Tool Call of class
mutate, exec or network needs an Approval; reads do not.

A Run is bounded by **Caps** (default 50 Turns, 200k prompt tokens) with a soft
nudge at 70%; hitting a Cap ends the Run in a resumable "budget exhausted" state.
Tool failures (including nonzero shell exits) are fed back as Tool Results, not
Run failures.

## Trade-offs

- The core crate cannot use Tauri's event system; the adapter translates a
  channel of `RunEvent` into Tauri events. Tests drive the loop directly.
- Detached runs require durable run/approval tables and resubscription logic.

## Alternatives rejected

- Frontend-owned loop: simpler, but run state dies with the window and safety
  logic sits in the least trusted component.
- In-memory runs: far less code, incompatible with the durability requirement.
