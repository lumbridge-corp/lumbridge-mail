# Architecture

## Shape

    GPUI app ─┐
    lbmail CLI ├─ JSON-RPC over a Unix socket ─ lumbridge-maild ─┬─ store (SQLite + search index)
    agent tools┘                                                 ├─ sync engine ─ providers (gog, IMAP/SMTP, Resend, Zoho, JMAP)
                                                                 └─ AI layer ─ your own model server, hosted if allowed

One daemon owns the mail: it syncs, stores, searches, sends and runs the AI. Everything else is a client.
That is what makes the client AI-native rather than AI-decorated: the models, agents and the UI all work on
the same store through the same protocol, and triage happens whether or not a window is open.

The app starts the daemon if none is running. The daemon can also live on an always-on machine (a home
server) and the app and `lbmail` elsewhere reach it over ssh, with the same protocol ("Remote daemon").

## Crates

| Crate | Does | Area |
|---|---|---|
| `lumbridge-mail-core` | Model, `Provider` trait, actions, safety rules, protocol types. No I/O. | foundation |
| `lumbridge-mail-store` | SQLite cache of accounts, mailboxes, messages, bodies, drafts, triage; the durable action queue; full-text search. | store |
| `lumbridge-mail-gog` | `Provider` for Gmail through the `gog` CLI's JSON (the tokens already exist). | gog |
| `lumbridge-mail-imap` | `Provider` for IMAP (IDLE, CONDSTORE where offered) + SMTP send. | imap |
| `lumbridge-mail-resend` | `Provider` for Resend's HTTP API: sent mail with delivery state, scheduled mail, mail received on a domain; send with an idempotency key. Flags, folders and drafts are local only. | resend |
| `lumbridge-mail-zoho` | `Provider` for Zoho Mail through Zoho's hosted MCP server (JSON-RPC `tools/call`, JSON or SSE): an allowlist of mail tools, the `mcp-remote` token file (refresh only on expiry or 401), one shared connection and budget per sign-in. Drafts local only. | zoho |
| `lumbridge-mail-render` | MIME body to something safe to show: sanitised HTML, a native block tree, plain text, quote folding. | render |
| `lumbridge-mail-ai` | Triage, summaries, reply drafts, ask-your-mail, over an OpenAI-compatible endpoint. Rules first, models second. | ai |
| `lumbridge-maild` | The daemon and the `lbmail` CLI: sync engine, action queue runner, JSON-RPC server, events. | daemon |
| `lumbridge-mail-app` | The GPUI app (`lumbridge-mail`): unified inbox, reader, composer, command palette. | app |
| `lumbridge-mail-e2e` | Tests only: the app's window, through its real `Rpc`, against the real daemon. | end to end |

## Decisions

- **Local-first.** Every change is written to the store and the action queue first, shown at once, and sent
  in the background. Actions are idempotent; a send interrupted mid-way is reconciled against the server
  before any retry so nothing is sent twice.
- **Providers are thin.** They hold a connection, not state. The engine owns the cursor and the store.
- **Gmail through gog first.** `gog` already holds OAuth for the Gmail accounts, with `--gmail-no-send` and
  `--enable-commands-exact` guards. It saves us registering a Google OAuth client. A native Gmail API
  provider can replace it later behind the same trait.
- **IMAP/SMTP second, JMAP third.** IMAP covers everything else, including Proton through its Bridge.
- **Resend for domains.** A Resend account is a sending domain: Sent with each email's delivery state,
  Scheduled, and an Inbox of mail received on the domain. Resend keeps no read state, folders or drafts,
  so those are local only, and the provider says so (`Provider::capabilities`). Polling (two requests per
  sync); a webhook receiver needs a public endpoint and is a later step (see the crate README).
- **Zoho through its MCP server.** Zoho's hosted MCP server already holds the OAuth client; one sign-in
  covers several addresses (one account each, sharing the connection). The provider calls only an
  allowlist of mail tools (the server also offers org-admin tools) and polls (no history API: the cursor
  keeps each folder's newest page; see the crate README). Drafts are local (no draft tool). Its listings
  have no thread ids: the store groups them with IMAP's scheme (`core::threading`; headers once a body is
  fetched, normalised subject and people before that), and regrouped an older daemon's one-message threads
  once at start.
- **Personas.** Every account is personal, work or anon. Anon is a wall, enforced in core, not a colour.
- **AI.** Rules run first (sensitive mail, obvious newsletters and notifications by header). Models see only
  what the rules let through, with mail wrapped as untrusted data. There is no default endpoint: you
  name one in the config (`~/.config/lumbridge-mail/config.toml`) or with `LBMAIL_AI_BASE` and
  `LBMAIL_AI_MODEL`, and say whether it is `local` (your own machines) or `hosted`. With none, triage runs on
  rules only.
- **HTML mail.** Stage 1: sanitise and convert to a native block tree (text, links, quotes, lists, images
  on request). Stage 2: real layout with Blitz for mail that needs it. The reader takes whatever
  `lumbridge-mail-render` hands it, so stage 2 does not touch the app.
- **GPUI.** `gpui-kit` from crates.io, pinned exactly, same snapshot family as lumbridge-app.

## Names

App `lumbridge-mail` (`com.lumbridgecorp.mail`), daemon `lumbridge-maild`, CLI `lbmail`.
Config `~/.config/lumbridge-mail`, data `~/.local/share/lumbridge-mail`, socket
`$XDG_RUNTIME_DIR/lumbridge-mail/maild.sock`. `LUMBRIDGE_MAIL_HOME` moves all three under one directory
(tests and sandboxes must set it). Everyone (app, daemon, `lbmail`) computes these with
`lumbridge_mail_core::paths::Paths::from_env()`. The person token (`person.token`) and the app's
single-instance socket (`app.sock`) sit next to `maild.sock`. When `maild.sock` would not fit a Unix
socket path (104 bytes on macOS, 108 on Linux; a deep `LUMBRIDGE_MAIL_HOME`), those three move to
`/tmp/lumbridge-mail-<hash of the runtime dir>/` (`Paths::socket_dir`); the daemon refuses that directory
unless it is a plain 0700 directory of its own user, and clients check the peer's uid before they show
the token.

## Protocol

JSON-RPC 2.0 over the daemon's Unix socket (`paths::Paths::socket`, mode 0600 in a 0700 directory), one JSON
object per line in both directions, UTF-8, lines up to 16 MiB. Requests are answered in any order (match on
`id`); the daemon also sends notifications with `method: "event"` and an `rpc::Event` as `params` to every
connected client. Types are the serde forms of `lumbridge_mail_core::rpc` and the model. `PROTOCOL_VERSION`
is 1; every change below is additive (new optional fields default, new methods), so it stayed 1.

| Method | Params | Result |
|---|---|---|
| `hello` | `HelloParams` `{person_token?, client?}` or `{}` | `Hello {protocol, version}` |
| `accounts.list` | `{}` | `[Account]` |
| `mailboxes.list` | `{account?}` | `[Mailbox]` |
| `threads.list` | `Query` | `ThreadPage` (search = `Query.text`) |
| `thread.get` | `ThreadRef` | `[MessageMeta]`, oldest first |
| `message.body` | `MessageRef` | `Body` (fetched and cached on demand) |
| `message.attachment` | `AttachmentRef` | `{path}` (saved under the cache directory) |
| `message.attachment.read` | `AttachmentRead {account, message, attachment, offset, max}` | `AttachmentChunk {filename, mime, size, offset, data (base64), eof}` (at most `ATTACHMENT_CHUNK_MAX` = 512 KiB a call; for clients on another machine) |
| `act` | `Act {account, action, caller}` | `{queued: id}` |
| `sync.now` | `{account?}` | `{}` |
| `status` | `{}` | `[AccountStatus]` |
| `counts` | `CountsRequest {accounts?, role?}` | `[CategoryCount {category, threads, unread}]` |
| `ai.triage` | `ThreadRef` | `Triage` (also stored) |
| `ai.summarise` | `ThreadRef` | `{summary}` |
| `ai.draft` | `DraftRequest {account, thread?, instruction, reply_all, extra_cc}` | `Draft` (never sent) |
| `ai.ask` | `{question, accounts?}` | `Answer` |
| `ai.extract` | `ThreadRef` | `Extracted {items, dates, by}` |
| `config.reload` | `{}` | `{accounts: n}` |
| `accounts.add` | `NewAccount {id?, persona, label, email, name, readonly, provider, password?}` | `Account` (person only; config written, password to the keyring, reloaded) |
| `accounts.discover` | `{email}` | `Discovered {imap_host, imap_port, imap_security, smtp_host, ...}` (a table + a guess; no network) |
| `accounts.capabilities` | `{account?}` | `[AccountCapabilities {account, capabilities}]` (what each account can do: `server`, `local`, `unsupported`) |
| `resend.domains` | `{account}` | `[DomainInfo {id, name, status, region, capabilities}]` (Resend accounts only) |
| `resend.api_keys` | `{account}` | `[ApiKeyInfo {id, name, created_at, last_used_at}]` (names only; never a key) |
| `sync.backfill` | `BackfillParams {account?, since?}` | `{accounts: n}` (fill in mail older than the sync window, slowly) |
| `sync.bodies` | `BodyFillParams {account?, since?}` | `{accounts: n}` (wake the background body fill; runs it until done even when the config has it off; `since` looks again at mail from then on) |
| `daemon.info` | `{}` | `DaemonInfo {version, paused, load: [ProviderLoad], kept_accounts}` |
| `daemon.pause` / `daemon.resume` | `{}` | `DaemonInfo` (pause = no provider call at all; persisted) |
| `daemon.quit` | `{}` | `{}`, then the daemon exits, leaving `maild.stopped` next to the socket |
| `accounts.purge` | `{account}` | `{purged: true}` (person only; the account must be out of the config) |
| `event` (notification) | `Event` | |

**Errors.** A failed request has `error: {code, message, data}` (`rpc::ErrorObject`). `data` is always a core
`Error` (`{"kind": "refused", "detail": "sending needs a person"}`); clients should switch on `data.kind` and
use `code` only when `data` is missing. Codes: `-32700` unparseable line, `-32600` not a request, `-32601`
unknown method (`data.kind = unsupported`), `-32602` bad params (`invalid`), `-32000` every other mail
error. Kinds: `offline` (retry later), `auth` (the user must sign in again), `not_found`, `unsupported`,
`refused` (a safety rule or read-only account; never retried), `invalid`, `resync_needed`, `no_answer`
(the model answered with nothing usable: no draft, summary or list; trying again may help), `other`.

**Who is calling.** `Act.caller` is self-declared, so the daemon does not trust it alone. At every start it
writes a random person token to `paths::Paths::person_token` (mode 0600, next to the socket). A connection
that sent it in `hello` may act as `Caller::Person`; on any other connection `caller: person` is treated as
`agent`. `Action::Send` is refused unless both say person, every other change but saving a draft is refused
to agents unless the daemon's config says `[agents] actions = true` (then still not on read-only accounts),
and agent connections see sensitive mail only as its one line (subject replaced, no snippet, no body). Limits: the token stops honest agents and model-driven
tools that use the protocol or `lbmail` as given (they never read the token); it does not stop a hostile
program running as the same Unix user, which can read the file like the app does. Real separation is a
different Unix user for agents (an assistant run as its own user), which cannot open the 0600 socket at all. A daemon on
another machine applies the same rule to connections that come through `lbmail pipe` ("Remote daemon").

**Later wire changes** (additive):
- `Provider::apply` returns `ApplyOutcome {server_id?}`; `Draft.server_id?` and `Action::DeleteDraft.server_id?`
  (omitted when unknown).
- `Delta.mailboxes` is `Option<[Mailbox]>` (`None` = unchanged); `Delta.reset` (a full sync's pages).
- `Error::ResyncNeeded` (kind `resync_needed`): the cursor cannot be used; the engine drops it and syncs
  from `None`.
- `Triage.suspicious` (default false); `ai` no longer prefixes the summary with "Suspicious".
- `safety::ModelPlace` serialises as `local` / `hosted`; `sensitive_kind` matches whole words and holds the
  ai crate's extra patterns (`sensitive_sender_kind` also looks at the display name).
- `Category::as_str`/`parse`, `MailboxRole::as_str`/`parse`, `core::paths`, `BODY_HEADERS` (headers every
  provider copies into `Body::headers`: List-Id, List-Unsubscribe, List-Unsubscribe-Post, Precedence,
  Auto-Submitted, Reply-To, Authentication-Results, X-GitHub-Reason).
- `Action::Move {from: null}` = add to `to`, remove nothing (IMAP: a copy).
- New methods `ai.extract` (types `Extracted`, `ActionItem`, `DateMention`, `Owner` moved to core),
  `counts`, `config.reload`; `DraftRequest.reply_all`, `DraftRequest.extra_cc`; `HelloParams`;
  `rpc::ErrorObject` and `rpc::code`.
- Integration (app meets daemon): `Account.readonly` (clients offer no actions on it), `Hello.person`
  (this connection showed the token), `accounts.add` / `accounts.discover` (first run), the
  `Paths::socket_dir` fallback above. `act`'s `{queued: id}` is the queue row's id, not a count: whether
  it went out is in `status` / `Event::Status`. Thread and message ids are unique only within an
  account; clients key them by `(account, id)`.

**Resend changes** (additive, protocol still 1):
- `ProviderKind::Resend`; `NewProvider::Resend {addresses?, receiving?}` (the API key in `NewAccount.password`,
  kept in the keyring as user `resend:<id>`).
- `Provider::capabilities() -> Capabilities` with a default (everything on the server); `Support` is
  `server | local | unsupported`. The daemon refuses `unsupported` actions before queuing them (kind
  `unsupported`) and serves `accounts.capabilities`; a read-only account reports nothing it can do.
- `core::DELIVERY_HEADER` (`X-Lumbridge-Delivery`) in `Body::headers` and `DeliveryState` /
  `Body::delivery()`: the last delivery event of mail sent through an API that reports it.
- New methods `accounts.capabilities`, `resend.domains`, `resend.api_keys`.

**Gentle backends** (after the first live gog run overloaded the gog host; all additive):
- `MessageMeta.partial` (default false): a record from a listing; the store keeps its stored values for every
  empty field (addresses, snippet, ids, references, size, date 0, attachments false).
- `Delta.window_floor`: with `reset`, the full sync covered only mail dated at or after this time; the store
  drops only unreported messages at or after it (gog: the oldest listed date, never earlier than one day
  inside `newer_than:<days>d`). `Delta.backfill`: older mail filled in; stored, never announced.
- `Provider::fetch_body_bundle(id, thread?)` -> `BodyBundle {body, others, metas}` (default: just the body);
  `Provider::request_backfill(since?)` (default `Unsupported`); `Provider::load()` -> `ProviderLoad`.
- `AccountStatus.paused`; `rpc::BackfillParams`, `rpc::DaemonInfo`; methods above.
- `paths::stopped_marker_for(socket)` (`maild.stopped`), `Paths::paused_file()` (`data/paused`),
  `Paths::ssh_control_dir()` (`ssh/` next to the socket, 0700).
- Store schema 3: accounts removed from the config are kept (`Store::keep_account`), hidden from every
  all-accounts listing, until `remove_account` (purge) or added again; `Store::merge_metas` stores records
  without touching the cursor.

**Body fill and search** (additive, protocol still 1): `Error::NoAnswer` (kind `no_answer`); `method::SYNC_BODIES`,
`BodyFillParams`, `BodyFillStatus` and `AccountStatus.body_fill` (only in `status` answers, not in status
events); `Query.since`, `Query.needs_reply`, `Query.min_priority` (left out when unset). The search grammar
gained `-part`, `OR`, `newer_than:`, `older_than:` and `category:` (store side only). Store schema 5 (body
fill marks).

**Zoho changes** (additive, protocol still 1): `ProviderKind::Zoho`; `daemon.info`'s `load` also lists
each Zoho connection (scope `zoho:<token file name>`). Zoho accounts are configured by hand (no
`NewProvider` variant yet).

Clients must not start a daemon while `maild.stopped` (or `data/paused`) exists: the app checks before its
autostart and says "stopped on purpose"; a daemon started by hand removes the marker.

## Remote daemon

One daemon on an always-on server (e.g. `homeserver`, aarch64, headless); the app and `lbmail` on other
machines use it as if it were local. No new listener: the transport is ssh.

    client                                                       server
    app / lbmail ── stdin/stdout ── ssh -T <host> lbmail pipe ── lbmail pipe ── maild.sock ── lumbridge-maild

- **Choosing it.** Client side: `[daemon] remote = "<ssh host>"` in `config.toml` (optional
  `remote_socket`, `remote_lbmail` (default `.local/bin/lbmail`, from the ssh user's home), `ssh`,
  `remote_multiplex`), or `LBMAIL_REMOTE=<host>`, or `--remote <host>` on `lbmail` and `lumbridge-mail`;
  `local` means this machine; an explicit `--socket` is always local. `lumbridge_mail_core::remote` holds
  the rule and the command lines; the daemon ignores these keys.
- **The pipe.** Clients run `ssh -T -e none -o BatchMode=yes -o ConnectTimeout=8 -o ServerAliveInterval=10
  -o ServerAliveCountMax=3 -o ClearAllForwardings=yes [-o ControlMaster=auto -o ControlPath=<ssh dir>/r-%C
  -o ControlPersist=60] -- <host> lbmail pipe` and speak the protocol above over its stdin/stdout, events
  included. `lbmail pipe` copies bytes between its stdio and the daemon's socket (after the same peer-uid
  check every client makes) and does nothing else. The shared connection's socket sits in `ssh/` next to
  the client's own sockets, in a directory that must be ours, 0700 and short enough for the socket path
  (`remote::MAX_CONTROL_DIR`); otherwise every ssh connects on its own.
- **Who is a person.** `lbmail pipe` never reads or adds the person token, so being able to run it gives
  nothing: the connection is an agent unless its own `hello` carries the token. A client that is a person
  fetches the token first with a second ssh command, `lbmail person-token`, which prints the running
  daemon's token only into a pipe (refused when stdout is a terminal, and with `--agent` / `LBMAIL_AGENT`),
  and only while that daemon answers on its socket. The client keeps it in memory for that connection's
  `hello`: never on a command line, in a file or in a config on the client. Who fetches it is the rule
  from one machine, unchanged: the app always; `lbmail` only at a terminal (stdin and stdout) and without
  `--agent`; `Rpc::remote_agent` never. So an agent tool handed `lbmail` (no terminal), or one that runs
  `ssh <host> lbmail pipe` itself, is an agent, and the daemon refuses its Send. The limit is also the
  local one: a program that can run any command as the user on the server (`ssh <host> cat …/person.token`)
  can read the token, as it could read the file on one machine. Separating those is a different Unix user,
  or an ssh key restricted to `command="lbmail pipe"` in `authorized_keys`. A token from a daemon that
  restarted between the fetch and the `hello` is refused: `lbmail` fetches once more, the app drops that
  connection and fetches again on the next.
- **Never started from a client.** In remote mode the app has no autostart and no stop: the server's
  systemd user unit owns the daemon (`deploy/install-server.sh`). `lbmail quit` asks the remote daemon to
  quit (it leaves `maild.stopped` there; nothing is written on the client); `lbmail resume` with no daemon
  says to start it there. `lbmail account ...` edits a config file, so in remote mode it says to run it on
  the server (`ssh -t <host> lbmail account add`); `account purge` goes over the protocol.
- **Offline.** When ssh cannot connect, the daemon is not running there, or the link drops, requests fail
  with `offline` and ssh's own words (`remote:homeserver: ... No route to host`), after at most ssh's
  ConnectTimeout (8 s) plus the token fetch (20 s): nothing hangs. The app says "maild on <host>: not
  connected, retrying in N s" and why, keeps what is on screen, and dials again with backoff (0.25 s
  doubling to 30 s; a connection that dies within 5 s counts as a failure, so a flapping link backs off).
  A dead link under an open connection is noticed by ssh's keepalive within ~30 s. The action queue lives
  in the daemon, so nothing is queued on the client while it is offline. `lbmail tail` reconnects the
  same way; other `lbmail` commands are one-shot and exit 3.
- **Attachments.** `message.attachment` answers with a path in the daemon's cache, which means nothing
  on another machine. A remote client reads the bytes with `message.attachment.read` instead: pieces of
  at most 512 KiB (base64 in the JSON line), written as they arrive to a temporary 0600 file in its own
  cache (`<cache>/attachments/remote/<hash of host, account, message, part>/`, 0700) and renamed to the
  mail's file name made safe (`core::files::safe_file_name`) when complete; a copy with the right size is
  reused. Files over 256 MiB are refused with their size. The app shows progress on the attachment's chip.
  Nothing from mail gets an executable bit, and the app never opens what `core::files::dangerous_to_open`
  flags (programs, scripts, installers, macro documents): those are only saved, locally or remotely.

## Running

See the top-level README, "Install and run". In short: `deploy/install-linux.sh` puts `lumbridge-mail`,
`lumbridge-maild` and `lbmail` in `~/.local/bin`; the app starts the daemon (next to itself first, then on
`PATH`, no arguments) and, with no accounts, offers "Try with demo mail" or "Add an account". Without the
app:

    lumbridge-maild &                                      # one per user; LUMBRIDGE_MAIL_HOME moves its files
    lbmail account add                                     # asks; or --email ... --provider demo|gog|imap|resend
    lbmail status && lbmail ls && lbmail show demo-work/t-q4

Accounts, the AI endpoint and timings are in `config.toml` (see `crates/lumbridge-maild/README.md` for every
key). As a user service: `deploy/install-linux.sh --systemd`, then `systemctl --user enable --now
lumbridge-maild` (or `--enable`, which does both). On a headless server, `scripts/remote-arm64 <name>`
builds the daemon and `lbmail` for aarch64 (static, musl) and `deploy/install-server.sh <host>` installs
them with the same unit (see "Remote daemon" and the README). The first live Gmail account should be added with
`readonly = true`: the daemon then refuses every action that would change the server, and reads only.

Gmail through gog runs inside a hard budget (per ssh host: 2 gog processes at once, ~2 starts a second,
niced, one shared ssh connection, backoff and a circuit breaker; see `crates/lumbridge-mail-gog/README.md`)
and the first sync fetches only the last 90 days or 2,000 messages. To stop everything: `lbmail pause`
(no provider calls, daemon stays) or `lbmail quit` (daemon exits; nothing starts it again until it is
started by hand); see `crates/lumbridge-maild/README.md`, "Stopping it, all of it".
