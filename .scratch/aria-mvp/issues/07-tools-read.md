# 07 — Tools: read, list_dir, search_files, todos, web_fetch

Status: done

## What

The five non-mutating Tools, each: JSON schema for the model, argument parsing
with typed errors, path containment for file tools, and a Tool Result string.

- `list_dir`: names, sizes, dirs first; sorted.
- `search_files`: glob + content regex via `ignore`/`globset`; respects
  `.gitignore`; caps results.
- `todos`: upsert the Run's todo list (`items: [{text, done}]`).
- `web_fetch`: `reqwest` GET, rejects non-http(s), rejects any resolved address in
  loopback/private/link-local ranges (SSRF guard), returns text with a size cap.

## Acceptance

- Each Tool has a table-driven test for success, bad args, and (where relevant)
  containment violation.
- `web_fetch` refuses `http://127.0.0.1`, `http://10.0.0.1`, `file:///etc/passwd`,
  and a host that resolves to a private address (mock resolver).
- No Tool error escapes as a panic; all become marked-error Tool Results.

## Notes

`todos` is class read (no Approval). `web_fetch` is class network (Approval).
