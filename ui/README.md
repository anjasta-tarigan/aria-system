# Aria UI

Vanilla TypeScript on Vite. `pnpm dev` serves it on `:1420`; append `?mock=1` to
run entirely in the browser against `src/mock.ts` instead of the Rust backend.

## Settings modal

`src/settings.ts` owns the whole Settings dialog. It renders its own DOM subtree
rather than going through the app shell, so form state (the nested provider
form, tag input, dirty tracking) survives repaints.

Open it from anywhere with:

```ts
import { openSettings } from "./settings";

openSettings(call); // `call` is the app's Ipc adapter
```

- Only one dialog exists at a time; opening again replaces the old one.
- It remembers the last section in `sessionStorage` under `aria.settingsSection`.
- General is the only section with an explicit Save and a dirty state. The other
  sections persist as you act on them.
- All user-facing copy lives in the `STR` object at the top of the file.

### Adding a section

1. Add its id to the `SectionId` union.
2. Add a label to `STR.sections` (and a `STR.<id>` block for its copy).
3. Add it to a group in `SECTIONS` — this drives the sidebar and the arrow-key
   order automatically.
4. Add a `case` to `sectionPanel()` returning a panel built with `panel(id, ...)`.
   The `role="tabpanel"`, `aria-controls`/`aria-labelledby` wiring is handled for
   you.

### Adding a backend call

Add the command to `src/api.ts` (typed wrapper) and to `src/mock.ts` (in-browser
mock), then call it through the `ipc` passed to `openSettings`. Keep API keys
write-only: send them to the backend, never render them after save.

### Tests

- `node --test ui/tests/api.test.ts` — validation and formatting helpers.
- `ui/../` E2E flows run against `?mock=1`; see `validate-settings.cjs` in the
  QA harness for the ten required flows.
