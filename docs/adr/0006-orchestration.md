# ADR-0006: Orchestration inside a Run

Status: accepted (2026-11-21)

## Context

A Run already survives long work (ADR-0004) and asks before risky Tools
(ADR-0001). Three gaps remained: a single 429 or timeout ended the Run, a
burst of chats could hit the provider all at once, and every read was
sequential even when the model asked for several independent files. There was
also no way to hand broad read-only exploration to a smaller context, and no
way for the model to stop a background task it had started.

## Decision

- A Turn is retried up to three attempts (300 ms, then 900 ms) when the
  provider fails transiently — rate limit, 5xx, timeout, connect — and no text
  has reached the reader yet. Once text streamed, a retry would duplicate it,
  so the Run ends with the error instead.
- At most four Turns are in flight across every chat. The permit covers only
  the provider call: a Run parked at an Approval, or busy in a long Tool, holds
  no slot, so it cannot starve another conversation.
- Tool Calls whose Tools only read (`read_file`, `list_dir`, `search_files`,
  `view_image`, `web_fetch`, `web_search`, `recall`, `poll_task`) run
  concurrently within a Turn. Anything that writes, runs a shell, replaces the
  plan or starts a background task keeps its order, so journal, cwd and
  replace-all semantics are unchanged.
- `delegate` runs a read-only sub-agent inside one Tool Call: its own message
  list, at most eight Turns, and a report capped at 8 KB. It can read, list,
  search, view images, fetch pages and recall memories; it cannot write, run a
  shell, change the plan or delegate again. It writes no rows and appears to
  the parent as one Tool Result. It is a Network Tool, so it asks once where
  the policy asks for network access.
- `kill_task` stops a background task that belongs to the same conversation.

## Trade-offs

- Retrying only before text streams keeps the transcript honest at the cost of
  not recovering a Turn that fails halfway. That is the rarer case, and the
  partial answer is already on screen.
- The concurrency bound is on Turns rather than Runs, because a parked Run is
  not using the provider. It does not bound CPU work from Tools; those stay
  sequential per Run.
- The sub-agent shares the parent's scope, settings and Tool Context, so its
  reads are exactly the parent's. It cannot be interrupted inside a provider
  call; cancellation is checked between its Turns. Its Turns are not recorded,
  so a failed delegation costs only the parent's Turn.
