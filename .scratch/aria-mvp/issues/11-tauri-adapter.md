# 11 — Tauri adapter: commands and RunEvent stream

Status: done

## What

Thin `src-tauri` layer: commands `create_conversation`, `list_conversations`,
`get_conversation`, `send_message`, `answer_approval`, `cancel_run`,
`resume_run`, `get_run_state`, `undo`, `search_messages`, `get_settings`,
`set_settings`. Forward each `RunEvent` to the webview under one event name.
On app start, resume any Run left `running`/`awaiting_approval` by re-driving its
loop from DB state.

## Acceptance

- Closing and reopening the window mid-Run shows the same Run and it completes.
- Pending Approvals survive app restart and can be answered.
- No business logic in `src-tauri` beyond mapping (assert by code review).

## Notes

Depends on 10.
