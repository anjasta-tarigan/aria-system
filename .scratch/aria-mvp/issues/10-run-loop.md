# 10 — Agent Run loop with approvals, caps and cancellation

Status: done

## What

The core loop from `spec.md`: persist Run and Turns, stream provider events onto
an `mpsc` of `RunEvent`, park on Approvals (persisted, no timeout), feed Tool
Results back, apply Caps with a soft nudge at 70%, and expose `cancel`,
`resume` (from `budget_exhausted`) and `answer_approval`.

## Acceptance

- With a scripted fake Provider, a multi-Turn Run with two Tool Calls completes
  and persists messages, tool_calls, todos.
- Pausing on an Approval and restarting the loop from DB state produces the same
  transcript as never pausing.
- Denying an Approval yields a "denied" Tool Result and the Run continues.
- Hitting the Turn Cap ends in `budget_exhausted`; `resume` continues it.
- Cancel during streaming and during a parked Approval both end `cancelled`.
- Every state transition is a row update; no state exists only in memory.

## Notes

ADR-0001. Test against a `Provider` trait with a fake, so no network.
