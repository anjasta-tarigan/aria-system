# 06 — Configuration and secret storage

Status: done

## What

Config file holding Provider `base_url`, `model`, plus app settings (Caps
defaults). API key stored in the OS keyring (`keyring` crate) when a secret
service is available, otherwise a chmod-600 file next to the config. A
`Settings` screen in the UI to view and edit these; a "test connection" button.

## Acceptance

- Key is never written to the config file or the DB.
- On a machine without a keyring, the fallback file is 0600 and the app says so.
- Test connection reports the resolved model and a one-token completion.
