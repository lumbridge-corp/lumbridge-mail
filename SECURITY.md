# Security

Report security bugs by email to security@lumbridgecorp.com, or use "Report a vulnerability" on this
repo's Security tab. Do not open a public issue with exploit details.

Include `lbmail --version`, your OS, whether the problem needs local access, a mail someone sends you, or
network access, steps to reproduce, and the impact you expect. Remove passwords, tokens and real mail from
logs and examples; a made-up mail that shows the problem is best.

The latest release and the latest `main` are supported. We answer in the same email thread. There is no
fixed response deadline.

## Trust model

The full version is in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) ("Protocol", "Remote daemon").

- **Mail is hostile input.** Nothing in a message is followed as an instruction, by code or by a model.
  Mail reaches a model only wrapped as untrusted data, after the rules. Mail that looks like it is talking to
  an assistant is marked suspicious, and a model's answer that looks like it obeyed the mail is thrown away.
- **Only a person sends.** The daemon writes a random person token (mode 0600) at every start. Only a
  connection that presents it may send, and by default only it may archive, delete or move mail. Agents,
  scripts and `lbmail` without a terminal can read, ask and save drafts.
- **Sensitive mail stays out of models.** Security codes, password resets, bank and government mail are
  never sent to a model and are shown to agents, logs and notifications as one fixed line.
- **Anon accounts are a wall.** They never go to a `hosted` model and are not mixed into the unified inbox
  or cross-account search.
- **Received HTML is sanitised.** No scripts, forms or remote loads; remote images only when you ask.
- **Secrets live in the OS keyring** (or the provider's own store for `gog` and Zoho), never in the config,
  the store or logs.

Limits, plainly: the person token stops honest agents and tools that use the protocol or `lbmail` as given.
It does not stop a hostile program that runs as your Unix user, which can read the token like the app does.
Run agents as a different Unix user for real separation. A model endpoint you configure sees the mail the
rules allow; choose one you trust.

In scope: a mail that makes the client or a model act on it, a way for an agent to send or change mail
without the person token, sensitive or anon mail reaching a model or a log, remote content loading without
consent, secrets written to disk or logs.
