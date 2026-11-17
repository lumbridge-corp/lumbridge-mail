# Changelog

## Unreleased

## 0.1.1 - 2026-11-17

- README: a header image, in `docs/assets/`. No code changes.

## 0.1.0 - 2026-11-13

First open-source release. In testing: run end to end on made-up mail only.

- `lumbridge-maild`, a daemon that syncs, stores (SQLite with full-text search), sends and runs the AI,
  with a JSON-RPC protocol over a Unix socket, a person token for sending, and an action queue that
  survives restarts.
- Providers: Gmail through `gog`, IMAP and SMTP (servers found from the address, password in the OS
  keyring), Resend sending domains, Zoho Mail.
- `lumbridge-mail`, a GPUI desktop app for Linux and macOS: unified inbox, personas (personal, work, anon),
  reader with sanitised HTML and phishing warnings, composer, command palette, full keyboard map, light and
  dark, and `--demo` with made-up mail.
- `lbmail`, a command line for the same daemon, and a daemon on another machine over ssh.
- AI over any OpenAI-compatible endpoint you name: triage, summaries, reply drafts, ask-your-mail, action
  items. Rules first; sensitive mail and anon accounts' mail kept from models. No endpoint is set by
  default.
- An optional one-way mirror into a Postgres you run, with embeddings for search and parsed facts.
