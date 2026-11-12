# 01 — Cargo workspace scaffold and core crate skeleton

Status: done

## What

Create the Cargo workspace: root `Cargo.toml` with members `crates/aria-core` and
`src-tauri`; `crates/aria-core` with empty modules `db`, `provider`, `agent`,
`tools`, `workspace`, `journal`. Add `rust-toolchain.toml` pinning the installed
toolchain. `src-tauri` holds a Tauri 2 app that opens a window loading the Vite
dev server.

## Acceptance

- `cargo test -p aria-core` runs (zero tests) without linking Tauri.
- `pnpm tauri dev` opens a window showing a placeholder page.
- No Tauri type appears in `aria-core`'s dependency graph.

## Notes

Tauri 2 on Arch needs `webkit2gtk-4.1` dev headers; install system packages if
the build fails. See ADR-0001.
