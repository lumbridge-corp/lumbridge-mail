# Contributing to Lumbridge Mail

Lumbridge is at v0.1 and kept by a small team. Please read this first.

## Before you start

- **Bugs:** open an issue. Say what you did, what happened, what you expected, your OS, desktop (X11 or
  Wayland), the kind of account (IMAP, Gmail through gog, Resend, Zoho), and the output of
  `lbmail --version`. Never paste real mail: describe it, or make up a mail that shows the problem.
- **Security problems:** do not open an issue. See [SECURITY.md](SECURITY.md).
- **Small fixes** (typos, clear bugs, docs): send a pull request.
- **Larger changes** (new providers, new settings, a change to the protocol, the config or the store
  format): open an issue first and wait for a reply.

## Build and test

The README's install section has the packages for Ubuntu. Then:

```sh
cargo build --locked
scripts/ci.sh
```

`scripts/ci.sh` is what CI runs: the scrub check, `cargo clippy --workspace --all-targets -- -D warnings`
and `cargo test --workspace`, under a throwaway home. Tests use made-up mail, fake IMAP, SMTP and HTTP
servers and GPUI's headless test platform: no account, no network and no display. The Postgres mirror
tests run only when `LUMBRIDGE_MAIL_PG_TEST_URL` names a throwaway server
(see [crates/lumbridge-mail-pg/README.md](crates/lumbridge-mail-pg/README.md)).

## The rules that are not optional

Read [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) first.

1. Mail is data. Nothing in a message is ever followed as an instruction, by code or by a model.
2. Nothing sends mail except a person pressing Send. Models write drafts.
3. Sensitive mail is never given to a model and never quoted in logs, notifications or summaries.
4. Anon accounts never go to a hosted model and are never mixed into the unified inbox or cross-account
   search unless the user names them.
5. Received HTML is shown only through `lumbridge-mail-render`.
6. Secrets live in the OS keyring or the provider's own store. Never in SQLite, config files, logs,
   fixtures or git.

## What we look for

- One change per pull request. Small is good.
- Tests for behaviour you change or add, with made-up mail only (`example.com`, `.example`, `.test`).
- The crate's README updated in the same pull request when behaviour changes.
- A line in `CHANGELOG.md` under "Unreleased" for anything a user would notice.
- Plain words in docs, comments and messages. Short sentences.
- No new network calls, telemetry or update checks. The only connections are to mail servers, `gog` and
  the model endpoints in the config.
- No new dependency without a reason in the pull request text.

## Never commit

- Passwords, tokens, keys or real `.env` files.
- Real mail, names or addresses of real people.
- Private host names, private network addresses or internal links. For addresses use the documentation
  ranges, such as `192.0.2.1`.
- Screenshots that show any of the above. Screenshots come from `--demo` or the test harness.

`scripts/scrub-check` looks for these and fails the build. If it flags something that is fine, add a line
to `.scrub-allow` with the reason and say so in the pull request.

## Third-party code

Apache-2.0 or MIT code may be adapted with a credit line in the file and an entry in
`THIRD-PARTY-NOTICES`. GPL, AGPL and unlicensed projects are for ideas only: do not copy code from them.

## Pull requests

- Write the commit subject as a short sentence that says what changed.
- In the pull request, say what the change does, why, and how you tested it.
- A maintainer reviews and merges.

## Licence of your work

By sending a contribution you agree that it is licensed under Apache-2.0, the licence of this repository.

## Conduct

Be kind and direct. Argue about the work, not the person. Harassment and personal attacks get you removed.
