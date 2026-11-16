# ADR-0005: Settings modal architecture and write-only secrets

Status: accepted (2026-11-14)

## Context

The initial settings implementation consisted of a basic set of flat fields (`base_url`, `model`, `api_key`) persisted directly in `settings.json` with an incomplete IPC surface. Supporting multiple OpenAI-compatible AI providers, embedding vector search configurations with re-index guards, granular tool execution gating, and external search backends required an accessible, production-ready Settings modal that protects sensitive secrets and survives complex async interactions.

## Decision

1. **Self-Managed Modal Lifecycle (`ui/src/settings.ts`)**:
   The Settings modal renders and replaces its own subtree within a `.settings-backdrop` overlay on `document.body` rather than re-rendering through the main app shell. This preserves nested provider form state, focus, tag input, and scroll position. A `closed` guard prevents async callbacks (e.g. model tests, latency probes) from resurrecting an unmounted dialog. Focus is trapped within the dialog using capture-phase event listeners and returned to the trigger button upon closure.

2. **Write-Only Secret Storage**:
   API keys are never rendered in full after saving, never returned over IPC, and stored in the OS keyring under `provider:{id}` (or a `0600` restricted file fallback). Masking uses character-boundary-aware slicing (`sk••••cdef`) safe for multibyte UTF-8 input.

3. **Active Provider Runtime Synchronization**:
   One configured provider is marked `is_default`. `Config::api_key()` automatically resolves the default provider's secret, and `Settings::sync_runtime_endpoint()` keeps the core agent loop's flat endpoint in sync with the default provider.

4. **Dynamic Gating and Immediate Feedback**:
   Form validity, duplicate name detection, model count limits (max 50), and the embedding re-index acknowledgment requirement update dynamically without destroying user input.

5. **Parity Between Mock and Tauri Runtimes**:
   All 16 settings flows and accessibility checks are verified against both the in-browser mock (`?mock=1`) and the Tauri/Rust core.

## Trade-offs

- Running the modal outside the main app shell requires explicit state management within `settings.ts`, but guarantees zero form loss and eliminates cross-component render collisions.
- Write-only secrets prevent the user from copying an existing API key out of the app; editing an existing provider requires entering a replacement key or leaving the field blank to retain the current secret.
