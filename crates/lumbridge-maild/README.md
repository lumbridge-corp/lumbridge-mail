# lumbridge-maild

The Lumbridge Mail daemon and the `lbmail` CLI. One daemon owns the mail: it syncs every account into the
store, sends the action queue to the providers, triages new mail in the background, and serves everything
to the app, `lbmail` and an assistant's tools over a JSON-RPC socket (docs/ARCHITECTURE.md, "Protocol").

## Run it

    lumbridge-maild                 # foreground; SIGTERM/SIGINT/`lbmail quit` stop it, SIGHUP re-reads config.toml
    lbmail account add --id personal --persona personal --email sam@example.com --name "Sam Example" --provider demo
    lbmail status
    lbmail ls                       # the unified inbox (anon accounts only with --account)

Files (`lumbridge_mail_core::paths`): config `~/.config/lumbridge-mail/config.toml`, store
`~/.local/share/lumbridge-mail/mail.db`, attachments `~/.cache/lumbridge-mail/attachments/`, socket, lock and
person token in `$XDG_RUNTIME_DIR/lumbridge-mail/`. `LUMBRIDGE_MAIL_HOME=<dir>` moves all of them under one
directory (`config/`, `data/`, `cache/`, and the socket at the top); tests and sandboxes set it.

Install: build `cargo build --release -p lumbridge-mail-app -p lumbridge-maild` (on a build machine), then
`deploy/install-linux.sh` (all three binaries to `~/.local/bin`, the launcher and icon; no sudo). The app
starts the daemon itself when it is not running. To keep mail syncing with no window open,
`deploy/install-linux.sh --systemd` also installs `deploy/lumbridge-maild.service` to
`~/.config/systemd/user`; then `systemctl --user enable --now lumbridge-maild`. On macOS the daemon and
`lbmail` ship inside `Lumbridge Mail.app/Contents/MacOS/` (`deploy/app/make-macos-app.sh`).

## Stopping it, all of it

    lbmail pause                    # every provider call stops now (sync, bodies, actions, triage fetches);
                                    # kept across restarts; the app does not start a paused daemon
    lbmail resume                   # undo pause (with no daemon running: clears the markers so it may start)
    lbmail quit                     # the daemon exits and writes $XDG_RUNTIME_DIR/lumbridge-mail/maild.stopped:
                                    # the app will not start it again until someone starts it on purpose

`lbmail quit` also works with no daemon running (it just writes the marker), so it is the one command that
guarantees nothing starts mail again. Starting `lumbridge-maild` by hand (or `systemctl --user start
lumbridge-maild`) removes the marker. The app shows "stopped on purpose" and points to `lbmail resume`. Under
systemd, `systemctl --user stop lumbridge-maild` stops it too, but the app could then start its own one: use
`lbmail quit` (the unit keeps `RuntimeDirectoryPreserve=yes` so the marker survives). A gog process that is
already running when the daemon stops finishes on its own (it is never killed half-way; each has a 60 s
deadline), and the shared ssh connection is closed (`ssh -O exit`).

## A daemon on another machine (over ssh)

    [daemon]                       # in the CLIENT's config.toml; the daemon ignores these keys
    remote = "homeserver"          # ssh destination; unset or "local" = this machine's daemon
    # remote_socket = "/run/user/1000/lumbridge-mail/maild.sock"   # default: the server's own default
    # remote_lbmail = ".local/bin/lbmail"                          # relative to the ssh user's home
    # ssh = "ssh"
    # remote_multiplex = true      # one shared ssh connection (ControlMaster, 60 s)

Or `lbmail --remote homeserver ...` / `LBMAIL_REMOTE=homeserver` (`local` forces this machine). Every command
then runs over `ssh -T homeserver lbmail pipe` (`src/remote.rs`); `lbmail status` names the daemon
(`daemon 0.1.0: remote:homeserver (as agent)`), `--json status` adds `via` and `person`. At a terminal lbmail
fetches the person token first (`ssh homeserver lbmail person-token`, memory only); without one it is an
agent, as on one machine. `lbmail account ...` (except `purge`) refuses in remote mode: run it on the
server. `lbmail tail` reconnects with backoff when the link drops; other commands exit 3 (`offline`).

The server's end: `lbmail pipe` (stdin/stdout to the daemon's socket, bytes only, never the token) and
`lbmail person-token` (the running daemon's token, into a pipe only; refused on a terminal and to
`--agent` / `LBMAIL_AGENT`). Install a server with `deploy/install-server.sh <host>` after
`scripts/remote-arm64 <name>`; the `vendored-dbus` feature builds libdbus from source for such cross builds.
Design and the person rule: docs/ARCHITECTURE.md, "Remote daemon".

## config.toml

```toml
[daemon]
sync_interval_s = 300          # longest single wait for new mail (IMAP IDLE; gog just sleeps)
max_backoff_s = 300            # offline retries double from 2 s up to this
triage = true                  # background triage of new mail
fetch_bodies_for_triage = true # triage may fetch a body on the way to a model (never for rule-classified mail)
triage_bodies_per_min = 20     # ...at most this many per account per minute, newest first; else the snippet
log_budget = true              # one log line a minute per busy budget (gog process starts per host, Zoho calls per sign-in)
first_retry_s = 5              # action queue backoff (store RetryPolicy)
max_retry_s = 1800
max_attempts = 12
body_cache_days = 0            # 0 = keep cached bodies
body_fill = false              # fetch the bodies of stored mail that has none, slowly, newest first ("Body fill")
body_fill_per_min = 10         # ...at most this many fetches per account per minute, one at a time

[agents]                       # callers without the person token (an assistant, scripts, lbmail without a terminal)
actions = false                # archive / trash / star / read state / move / delete a draft: refused for
                               # agents (true allows them on accounts that are not read-only); reading,
                               # asking and saving drafts are always allowed, sending never is

[ai]                           # every lumbridge-mail-ai EndpointConfig / AiConfig field, flattened
enabled = true
base_url = "http://127.0.0.1:8000/v1"   # no default: without base_url and model (or LBMAIL_AI_BASE and
model = "my-model"                      # LBMAIL_AI_MODEL) the AI is off and triage runs on rules only
place = "local"                # local | hosted: decides which accounts may use it
# assistant_names = ["Ava"]    # mail that orders your own assistant by name is marked suspicious
concurrency = 4
# api_key_env = "LBMAIL_AI_KEY"   # name of an env var holding a bearer token (never the token itself)

[gog]                          # the hard budget on gog processes (lumbridge-mail-gog README, "The budget")
ssh_max_in_flight = 2          # gog processes at once per ssh host, across all its accounts
local_max_in_flight = 3        # ... for gog on this machine
max_starts_per_min = 120       # process starts per host per minute (token bucket), bursts of `burst`
burst = 2
account_max_in_flight = 2      # one account's calls at once, inside the host's limit
retries = 3                    # after a transport failure (ssh 255, reset, timeout), after the backoff:
backoff_base_s = 2             # 2 s doubling per failure in a row, with jitter, up to backoff_max_s
backoff_max_s = 120
breaker_failures = 5           # this many transport failures in a row: the host's accounts go offline...
breaker_cool_s = 300           # ...for this long, then one probe call
nice = 15                      # nice -n for gog (and ionice -c3 where it exists), remote and local
ionice = true
ssh_multiplex = true           # one shared ssh connection per host (ControlMaster, socket in the runtime dir)
ssh_persist_s = 300            # kept open this long after the last call
ssh_max_sessions = 8           # ssh_max_in_flight is capped to this (OpenSSH MaxSessions defaults to 10)
timeout_s = 60                 # one gog call's deadline

[[accounts]]
id = "personal"                # a-z 0-9 - _, stable: the store keys on it
persona = "personal"           # personal | work | anon | { other = "..." }
label = "Personal"
email = "sam@example.com"
name = "Sam Example"
color = "#3b82f6"
ai = "local_only"              # off | local_only | any (anon never goes to a hosted model anyway)
readonly = true                # refuse every action that would change the server
# body_fill = false            # opt this account out of the body fill (true: in, whatever [daemon] says)
# body_fill_per_min = 6        # this account's fill rate (default [daemon] body_fill_per_min)
[accounts.provider]
kind = "gog"                   # Gmail through gog
ssh_host = "gog-host"          # run gog there (it holds the tokens); or binary = "/usr/local/bin/gog" locally
binary = "~/.local/bin/gog"    # with ssh_host, ~/ is the ssh user's home on that host (expanded there);
env_file = "~/.config/gog.env" # sourced before gog runs; ~user/... and relative paths are refused
window_days = 90               # first sync: only the last 90 days...
window_max = 2000              # ...and at most 2000 messages, whichever is smaller (0 and 0 = all mail)
# backfill = false             # true: fill in all older mail on its own, one page per sync round
# backfill_every_s = 60        # shortest time between two backfill pages
# page_size = 100              # messages per listing (one gog process each)
# search_bodies = true         # listings carry body text for snippets (same process count, bigger output)
# max_in_flight = 2            # this account's gog calls at once (default [gog] account_max_in_flight)
# triage_bodies_per_min = 20   # default [daemon] triage_bodies_per_min

[[accounts]]
id = "fastmail"
persona = "work"
email = "sam@work.example"
[accounts.provider]
kind = "imap"
imap_host = "imap.work.example"  # imap_port, imap_security = "tls" | "start_tls" | "plain" (loopback only)
smtp_host = "smtp.work.example"  # leave out for a read-only account
auth = "password"                # or "oauth2" (the secret is an access token; refresh is not done yet)
# password_env = "WORK_IMAP_PASSWORD"         # headless: the secret from an env var
# password_command = ["pass", "show", "work"] # ... or from a command; else the OS keyring

[[accounts]]
id = "shop"
persona = "work"
email = "hello@shop.example"     # the From of mail sent from this account
name = "Shop"
[accounts.provider]
kind = "resend"                  # Resend's API: Sent (+ delivery state), Scheduled, Inbox (received)
# addresses = ["@shop.example"]  # what this account shows and sends from (default: the email's domain)
# receiving = false              # the domain does not receive through Resend: no Inbox
# reply_to = "support@shop.example"
# api_key_env = "SHOP_RESEND_KEY"            # headless: the key from an env var
# api_key_command = ["pass", "show", "resend/shop"]  # ... or a command; else the OS keyring
# requests_per_second = 4                    # Resend allows 10 per team, shared with the websites
# base_url = "https://api.resend.com"

[[accounts]]
id = "zoho-one"                  # one account per address; a second address on the same Zoho sign-in
persona = "work"                 # gets its own [[accounts]] block with the same four connection keys
email = "sam@one.example.test"   # picks the Zoho mail account (list_accounts) and is the From
readonly = true                  # first live run: read only
[accounts.provider]
kind = "zoho"                    # Zoho Mail through Zoho's hosted MCP server
mcp_url = "https://mcp.example.test/..."           # the MCP message URL (https)
token_url = "https://accounts.example.test/..."    # the OAuth token endpoint (https)
client_id = "..."                                  # the OAuth client the token was issued to
token_file = "~/.config/lumbridge-mail/zoho/tokens.json"  # mcp-remote format, 0600; never in this file
# zoho_account_id = "123..."     # skip the lookup by address
# window_days = 90               # first sync: the last 90 days ...
# window_max = 2000              # ... and at most 2000 messages per folder
# backfill = false               # true: fill in older mail on its own, one page per round
# backfill_every_s = 60
# page_size = 100                # list_mails page in a full sync / backfill (max 200)
# poll_size = 50                 # newest messages per folder an incremental round looks at
# poll_interval_s = 60           # rounds at most this often (Zoho has no push)
# max_in_flight = 2              # the connection's budget, shared by every account on the token file
# max_calls_per_min = 60
# flag = "important"             # the Zoho flag a star sets: info | important | follow_up
# sync_archive = false           # true: an Archive view (live Zoho currently refuses the option: leave off)
# triage_bodies_per_min = 20

[[accounts]]
id = "demo"
persona = "work"
email = "sam@work.example"
[accounts.provider]
kind = "demo"                  # made-up mail, no network; set = "personal" | "work" | "anon"
```

**Removing an account.** `lbmail account remove <id>` needs a person at a terminal (agents are refused).
Without a terminal (a person over ssh with no tty, a script the person runs on purpose): edit
`config.toml`, delete that `[[accounts]]` block (and its `[accounts.provider]`), then reload with
`systemctl --user reload lumbridge-maild`, `kill -HUP <pid of lumbridge-maild>`, or `lbmail account list`
followed by any `lbmail account add`/`remove` (they reload too). Either way the daemon stops the account
and **keeps** its stored mail, hidden from every list and search, so adding it back (same id) resumes
without a resync. `lbmail status` lists kept accounts; `lbmail account purge <id>` (a person at a
terminal; the account must be out of the config) deletes the stored mail. Nothing on the server changes.

`lbmail account add/list/remove` edit this file (comments are not kept) and ask a running daemon to reload
(`config.reload`). `lbmail account add` with no `--email`/`--provider`, at a terminal, asks instead: "Try
with demo mail" (three made-up accounts), Gmail through gog (binary here or `ssh:<host>`; read-only unless
you say no) or IMAP (servers guessed from the address, see `setup::discover`; the password goes to the
keyring). The app's first-run screen does the same through `accounts.add`, which only a person's
connection may call; the daemon writes the entry, stores the password with its `SecretStore` (the OS
keyring; tests pass a stand-in), and reloads. An account removed from the config is stopped and its mail
kept until purged (above). Secrets are never in the file: IMAP passwords come from `password_env`, then
`password_command`, then the OS keyring (service `lumbridge-mail`, user `imap:<id>`; `lbmail account
set-password <id>` writes it). Resend API keys the same way: `api_key_env`, `api_key_command`, then the
keyring (user `resend:<id>`; `lbmail account set-key <id>` writes it, `lbmail account add --provider resend
--email hello@shop.example --keyring` asks for it). gog keeps its own OAuth tokens.

`lbmail resend domains <account>` lists the team's domains with their verification status and `lbmail
resend keys <account>` the API keys' names and last use (never a key), through `resend.domains` /
`resend.api_keys`. Both need a full-access key. See `crates/lumbridge-mail-resend/README.md` for what a
Resend account can do and what stays local.

Zoho accounts are added by hand in this file (no `lbmail account add` flow yet). The OAuth token stays in
its own `token_file`, which the daemon reads and, on a refresh, rewrites atomically keeping its mode; it
refreshes only when the access token has expired or a call came back 401 (never on a timer), after reading
the file again, so another program holding the same refresh token can keep using it. Accounts with the
same `mcp_url`, `token_url`, `client_id` and `token_file` share one connection: one token, one refresh at a
time, one budget (it shows in `daemon.info` as `zoho:<token file name>`). See
`crates/lumbridge-mail-zoho/README.md` for the tool calls, the cursor and what stays local. `lbmail zoho
probe --account <id>` checks a live account read-only and prints only the answers' shapes (see there).

## What the engine does

- **Pause.** `daemon.pause` (`lbmail pause`) stops every provider call at once: sync loops wait, a sync in
  flight is let go, body and attachment fetches fail as `Offline` ("paused"), the action queue holds its
  actions (they stay applied locally), triage stops. Persisted as `data/paused`; `status` shows
  `paused: true` for every account and the app's status bar says "Paused".
- **Gog budget.** Every gog account's calls go through `lumbridge_mail_gog::GatedRunner` with a host gate
  shared by all accounts on the same `ssh_host` (`[gog]`); a config reload restarts only changed accounts
  and never kills a gog process in flight. Once a minute while gog is busy the daemon logs
  `gog ssh:<host>: N process starts in the last minute (limit 120/min), r of 2 running, T since start`
  (plus any backoff or open breaker); a busy Zoho sign-in logs `zoho: N calls in the last minute (limit
  60/min), r of 2 in flight, T since start (sign-in <token file>)` (`ProviderLoad::describe`). `lbmail
  status` prints the same lines.
- **Sync loop, per account.** `sync(cursor)` with the stored cursor; pages (`Delta.more`) are fetched back to
  back; then `wait_for_change(sync_interval_s)` (IMAP IDLE) or `sync.now`, and again. `Offline`: status
  offline, retry with doubling backoff (2 s to `max_backoff_s`). `Auth`: status shows it and the loop waits
  until the user acts (`sync.now`, e.g. after signing in again). `ResyncNeeded`: drop the cursor, sync from
  `None` (a full sync whose pages are `reset`, so mail that vanished meanwhile is dropped; with a window,
  only inside the window). New unseen inbox mail outside a full sync is announced as `NewMail` (sensitive
  mail as its one line); backfill pages (`Delta::backfill`) never are. `sync.backfill` (`lbmail sync
  --backfill [--since DATE]`) asks the provider to fill in older mail, one page per sync round.
- **Action runner, per account.** `act` writes the action to the store (applied locally at once, in the same
  transaction), the runner sends one action at a time to the provider: done (the outcome's draft id is kept),
  retried with the store's backoff on `Offline`, failed for good and rolled back otherwise
  (`ActionFailed`). After each success the account syncs to reconcile. Actions interrupted by a crash are
  marked and retried at the next start (gog and IMAP check Sent before re-sending).
- **Bodies and attachments** are fetched on demand and cached; attachments are written 0600 under
  `cache/attachments/<hash of account+message>/<safe file name>`. A body fetch asks the provider for a
  `BodyBundle` with the message's thread: gog returns the whole thread in one process, and the daemon
  caches the other bodies (never over a cached one) and stores the full records (which fill in what an
  older gog's listing lacked).
- **Triage**, in the background, newest first, of threads whose triage is missing or older than their
  newest message: rules first on what is stored (sensitive mail, newsletters, notifications, receipts by
  sender and header, and Gmail's own Promotions / Social / Forums labels; never a model, never a body
  fetch), then the model when `safety::ai_allowed(account, model place)`, else a plain "Mail from X" row.
  On the way to a model the latest body is fetched only if `fetch_bodies_for_triage`, the account's
  `triage_bodies_per_min` bucket has room (20 a minute, up to 5 saved), and Gmail did not file it under
  Updates; otherwise the model gets the snippet. With a fetched body the rules run again first (List-Id
  and friends). `ai.concurrency` threads at a time; failures wait before they are tried again. `ai.ask`
  reads cached bodies, fetches within the same bucket, and uses snippets for the rest.

- **Body fill** (`src/fill.rs`; off by default). Bodies are otherwise fetched only when a thread is opened
  or triage needs one, so embeddings, semantic search and parsed facts cover only that mail. With
  `[daemon] body_fill = true` each account gets a fill task that fetches the bodies of stored mail without
  one, newest first, at most `body_fill_per_min` a minute (evenly spaced), one fetch at a time, through the
  provider's thread bundle (gog: one process per thread) and its own budget. Lowest priority: a fetch starts
  only when the account is online with its first sync done, nothing else of the account has used the
  provider for 2 s (sync, an open, a triage fetch, an attachment, an action), and the provider's budget has
  nothing in flight, no backoff or open breaker, and less than half of its per-minute budget used (gog
  processes per host, Zoho calls per sign-in). `lbmail pause` stops it. Drafts, mail with a body and mail
  set aside as not fetchable (`NotFound` / `Unsupported` from the server) are left out; offline and other
  errors retry the same message after 30 s doubling to 30 min. The walk's marks are in the store (schema
  V5), so a restart goes on where it stopped; new mail comes first; a finished walk is walked again once a
  day (mail that arrived below the marks). `lbmail status` shows `bodies have/total (%), rate, fetches in
  the last hour, what it waits for`. `lbmail sync --bodies [--account A] [--since DATE]` (`sync.bodies`)
  wakes it, runs it until done even when `body_fill` is off, and with `--since` looks again at everything
  from that day on without a body (what was set aside too). Per account `body_fill = false` opts out (also
  of `sync --bodies`). Sensitive mail is filled into the local store as opening it would (that is today's
  rule); the mirror never copies its body and the workers never embed or parse it (`tests/pg_work.rs`).
  Production, conservatively: `body_fill = true`, `body_fill_per_min = 6`: ~3,000 messages in at most 8 h
  20 min per account, less where gog brings whole threads.

## Safety rules the daemon enforces (not just the clients)

- `Action::Send` only from a connection that showed the person token **and** says `Caller::Person`. The token
  is written 0600 at every start; `hello {person_token}` presents it. Anything else is an agent. `lbmail` shows
  it only when stdin and stdout are a terminal (and not with `--agent` / `LBMAIL_AGENT=1`), and has no send
  command at all.
- Agents change nothing unless `[agents] actions = true`: from an agent connection (whatever its `caller`
  says) archive, trash, star, read state, move and deleting a draft are `Refused` ("... needs a person"),
  like Send; saving a draft is allowed. With `actions = true` they are allowed on accounts that are not
  read-only; Send stays a person's. Person connections are not affected.
- No AI draft of sensitive mail, for anyone: `ai.draft` on a thread with a sensitive message is `Refused`
  with the one line ("A security code from Acme: sensitive mail is never given to a model ..."), before any
  body is fetched; `ai.ask` drops sensitive hits before the model sees them, so no answer can carry them.
- Agent connections see sensitive mail only as its one line: thread rows and message metadata get the line as
  subject and no snippet; bodies and attachments of sensitive mail are `Refused`.
- Read-only accounts (`readonly = true`): every `act` is `Refused` and the runner never calls `apply`.
- Actions an account cannot do at all (`Provider::capabilities()` says `Unsupported`) are refused with
  `Unsupported` before anything changes locally. `accounts.capabilities` tells clients what each account
  can do and what is local only (Resend: read state, stars, archive, drafts); a read-only account reports
  nothing it can do.
- The anon wall: `threads.list`, search, `counts` and `ai.ask` leave anon accounts out unless named.
- Socket 0600 in a 0700 directory owned by the user; connections from another Unix user are dropped (peer
  credentials); one daemon per runtime directory (flock), a stale socket is replaced. A runtime path too
  long for a socket moves the socket and token to `/tmp/lumbridge-mail-<hash>/` (`core::paths`), checked the
  same way; `lbmail` and the app check the daemon's uid before they send the token.

**Limits of the person token, plainly:** it separates honest agents (an assistant's tools, scripts, a model driving
`lbmail` without a terminal) from the person. It does not stop a hostile program running as the same Unix user:
that program can read the token file exactly as the app does, or fake a terminal. Real separation is running
agents as a different Unix user, which cannot open the socket at all. A narrower agent socket for that case is
not built yet.

## The Postgres mirror (`[pg]`)

Off by default. When on, a background task copies the store into the `mail` schema of a Postgres
database, one way, for other devices and agent tools (`src/mirror.rs`; schema, roles and commands in
`crates/lumbridge-mail-pg/README.md`; design in `docs/MAIL-POSTGRES.md`).

    [pg]
    enabled = true
    dsn_file = "~/.config/lumbridge-mail/pg.dsn"   # 0600, the mail_writer DSN; never the DSN itself here
    # ca_file, batch = 500, poll_ms = 1000, max_backoff_s = 300

    [[accounts]]
    mirror = false                                 # per account (anon accounts never are)

The daemon works the same without Postgres: changes wait in the store's outbox (durable, across
restarts) and go when it is back. Anon and `mirror = false` accounts are never sent; sensitive mail goes
as metadata and its one line only. `lbmail pg status | backfill | migrate`; RPC `mirror.status`,
`mirror.backfill {account?, retry_stuck?}`.

### Embeddings and parsing (`[pg.embed]`, `[pg.parse]`)

Both off by default; they need `[pg]` on and schema version 2 (`lbmail pg migrate`). They drain the
`mail.jobs` queue the mirror fills, as `mail_writer` (`src/pgwork/`; design, extractors and privacy rules
in `docs/MAIL-POSTGRES.md`).

    [pg.embed]
    enabled = true
    base_url = "http://<embed-host>:<port>/v1"     # OpenAI-compatible /v1/embeddings
    model = "<embedding model name>"               # written with every vector; searches use it
    place = "local"                                # local | hosted (local_only accounts need local)
    headers = [["x-no-trace", "1"]]       # sent on every request
    # api_key_env = "LBMAIL_EMBED_KEY"             # an env var holding a bearer token, if needed
    # input_type = false                           # true: send input_type passage/query (NIM)
    # query_prefix = "", passage_prefix = ""       # for models that want them
    # dimensions = 2048                            # ask for N dims (Matryoshka); must match the column
    # timeout_s = 60, connect_timeout_s = 5
    # batch_jobs = 32, batch_inputs = 16           # jobs per round, texts per request
    # max_per_minute = 120, concurrency = 1        # gentle on a shared GPU box
    # chunk_chars = 3000, chunk_overlap = 300, max_chunks = 4
    # max_attempts = 5, retry_s = 60, max_retry_s = 3600, lease_s = 600, poll_ms = 2000

    [pg.parse]
    enabled = true
    # model = true                                 # use the [ai] endpoint (false: rules only)
    # max_per_minute = 30, concurrency = 1         # model calls
    # max_chars = 6000, min_confidence_pct = 50
    # triage_wait_s = 600                          # wait for a thread's triage before parsing it
    # batch_jobs = 32, max_attempts = 5, retry_s = 60, max_retry_s = 3600, lease_s = 600, poll_ms = 2000

The parsing worker's model calls use the `[ai]` endpoint with its `headers` (set the untraced header there
too) and its `place`. RPCs: `pg.work.status`, `pg.requeue {kind, account?, all?, parked_only?}`,
`pg.parsed {kind?, since?, accounts?, limit?}`, `search.semantic {q, accounts?, limit?, since?, until?}`.

## lbmail

    lbmail status | accounts | counts   # status: accounts, paused, gog load per host, kept accounts
    lbmail sync [--account A] [--backfill [--since YYYY-MM-DD]]
    lbmail sync --bodies [--account A] [--since YYYY-MM-DD]   # wake / widen the background body fill
    lbmail pause | resume | quit
    lbmail ls [--account A]... [--unread] [--category C] [--mailbox inbox|<id>] [--limit N]
              [--since YYYY-MM-DD|36h|3d|2w] [--needs-reply] [--min-priority N]   # the last two from triage
    lbmail search <query...>            # from: to: subject: filename: has:attachment is:unread|read|starred
                                        # before: after: newer_than:3d older_than:2w in: category:
                                        # -part (not), a OR b; e.g. -from:ci@example.com is:unread
    lbmail search --semantic <question...> [--account A]... [--limit N]   # by meaning, from Postgres;
                                        # falls back to the local search and says why
    lbmail parsed [--kind receipt|travel|package|subscription|recruiting|bill|event|newsletter]
                  [--since YYYY-MM-DD] [--account A]... [--limit N]
    lbmail pg migrate [--dsn-file F] [--dry-run] [--embedding-dim 2048] [--embedding-type halfvec]
    lbmail pg status                    # mirror + workers + queues (ready, waiting, parked, per hour/day)
    lbmail pg backfill [--account A] [--retry-stuck]
    lbmail pg reembed [--account A] [--all] | reparse [--account A] | requeue --parked [--kind K]
    lbmail show <account/thread>        # plain text via render; sensitive mail shows only its one line;
                                        # --json per message: text, new_text (no quoted history),
                                        # mailing_list, list_id, suspicious (+ why), reply_to_differs
    lbmail archive|trash <account/thread>...
    lbmail star|read <account/thread>... [--off]
    lbmail draft <account/thread | account/-> <instruction...> [--all] [--to addr]... [--save]
    lbmail ask <question...> [--account A]...
    lbmail tail [--count N] [--timeout S]
    lbmail account add|list|remove|purge|set-password ...

Every command takes `--json`. Exit codes: 0 ok, 2 bad input, 3 daemon offline, 4 refused, 5 not found,
6 the model gave no usable answer or draft (`no_answer`; trying again may help), 1 other.

## Tests

    cargo test -p lumbridge-maild
    cargo clippy -p lumbridge-maild --all-targets -- -D warnings

`tests/e2e.rs` starts the daemon in-process on a temp `LUMBRIDGE_MAIL_HOME` with demo accounts (personal,
work with AI off, anon, and a read-only one) and a `ScriptedModel`, connects over the socket and checks: list,
thread, body, attachment, search (with operators and HTML-only bodies), act, sync, status, events (NewMail,
sensitive as one line), counts, JSON-RPC errors; send refused for agents (also when they claim to be a
person) and for wrong tokens; sensitive mail redacted for agents and never in a prompt; read-only accounts;
an offline provider whose queued action is retried and lands; an auth error that waits for the user; a
forgotten history that forces a resync and drops vanished mail; restart persistence (store, queue, stale
socket, second instance refused); background triage (rules vs model, suspicious flag, AI-off account never
reaching a model) and every `ai.*` method. `tests/imap.rs` runs the daemon with a real `ImapProvider` from
config against the imap crate's scripted IMAP/SMTP servers (sync, body, flag change, a send with a Sent copy).

`tests/gentle.rs`: pause stops every provider call (sync.now included, bodies refused) and survives a
restart; `daemon.quit` stops the daemon and leaves the marker, a deliberate start removes it, and stopping
right after starting is quick; triage fetches bodies only for model-bound mail, within its per-minute
budget, newest first; a gog account (on `FakeRunner` through the real budget) syncs a window in 5
processes, a body fetch caches its thread sibling, and a backfill page is stored without `NewMail`;
removed accounts keep their mail until a person purges it; a config reload does not kill a gog call in
flight.

`tests/fill.rs`: the body fill fetches every body newest first and never a draft; keeps to its rate, one
fetch at a time; a person's open is served at once and the fill waits until the account is quiet; pause
stops it; an outage waits; mail the server lost is set aside after one try and stays aside after a restart;
a restart goes on where the fill stopped (each message fetched once across both runs); per-account
opt-out and `sync.bodies` (refused for an opted-out account, runs a fill the config has off until done);
gog fills whole threads in one process each, one process at a time, inside the host's budget (at most half
of the minute's starts). `tests/pg_work.rs` also proves a filled sensitive body never reaches Postgres, the
embedder or a model.

`tests/agents.rs`: with `[agents] actions = false` every change but saving a draft is refused to an agent
(also when it claims to be a person) and nothing changes locally, a person is unaffected; with `true`
agents may change mail but not on a read-only account and never send; `ai.draft` of sensitive mail is
refused to agents and persons with the one line only, `ai.ask` never brings the code back, no prompt ever
holds it; a model with no usable draft is `no_answer` (exit 6); `needs_reply`, `min_priority`, `since`,
negation, `OR` and `category:` through the daemon.

`tests/pg_work.rs` (with `LUMBRIDGE_MAIL_PG_TEST_URL`): the workers against a fake embeddings server
and a scripted model: every allowed body embedded once with the configured model and header, AI-off,
anon and sensitive mail never embedded or parsed, quoted history never embedded, the flight (schema.org),
the newsletter (headers) and the receipt (model) parsed, mail that talks to an assistant never in a prompt,
most mail costing no call, status counters, semantic search ranking and the local fallback (anon named,
endpoint down, Postgres down), idempotence (requeue all: no new calls; one changed body: one call),
retries and parking with the endpoint down and requeue after, a model change re-embedding everything, a
hosted endpoint never given local-only mail; and a throughput run on a typical mix (printed).

## Not done yet

- No agent socket / second Unix user setup (see the token's limits above).
- OAuth token refresh for IMAP XOAUTH2 accounts; gog refreshes its own.
- Notifications to the desktop (events exist; nothing shows them yet).
- One store connection behind a mutex: long searches wait behind a sync write (fine at demo and test sizes,
  not measured on a 500k mailbox through the daemon).
- `lbmail` has no way to send by design; the app is the only sender.
- `accounts.add` cannot edit or remove accounts yet (`lbmail account remove` can); no IMAP OAuth sign-in.
- The budget is per daemon: two daemons (two users, two machines) on the same gog host each get their own.
- The app has no Pause / Resume / Quit buttons yet (`lbmail` has them; the status bar shows "Paused").
