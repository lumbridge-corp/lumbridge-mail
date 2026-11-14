# lumbridge-mail-ai

The layer that reads mail for the user: triage, summaries, reply drafts, ask-your-mail, action items.
Rules first, models second. Core types in (`MessageMeta`, `Body`, `Account`), core types out (`Triage`,
`Draft`, `rpc::Answer`). It does not know the store or the daemon, it never sends mail, and it never logs.

## Use

```rust
use lumbridge_mail_ai::*;

let model = OpenAiModel::new(EndpointConfig::default())?;      // config comes from the daemon's config.toml
let ai = MailAi::new(model, AiConfig::default());

let thread = MailThread::new(vec![MailMessage::new(meta, Some(body))])?;
let acct_ai = ai.for_account(&account)?;                        // Error::Refused if safety::ai_allowed says no

let triage: Triage = acct_ai.triage(&thread).await?;           // rules, then model, then a safe fallback
let many = acct_ai.triage_many(&backlog).await;                // `concurrency` model calls at a time
let summary: String = acct_ai.summarise(&thread).await?;
let draft: Draft = acct_ai.draft_reply(&thread, &ReplyRequest { instruction: "yes to Friday".into(), ..Default::default() }).await?;
let draft: Draft = acct_ai.draft_new(to, "Subject", "ask Bo about Thursday", &voice).await?;
let items: Extracted = acct_ai.extract(&thread).await?;        // action items + dates
let answer: rpc::Answer = ai.ask("when is my flight?", &accounts, &search).await?;  // search: impl Search
```

HTML bodies become text through `lumbridge-mail-render` (`text::html_to_text`), the same text the reader
shows; the regex pass `text::html_all_text` remains only for the injection detector, which must also see
what the mail hides. `Extracted`, `ActionItem`, `DateMention` and `Owner` live in core (re-exported here).

Rules alone, without a model (for accounts whose AI is off): `triage_by_rules(&thread, &account.email, now)`
and `sensitive_line(&thread)`.

`EndpointConfig` has no endpoint by default: `base_url` and `model` are empty, and then the AI layer is off
(`OpenAiModel::new` refuses; the daemon runs triage on rules only). Set them in the config or with
`LBMAIL_AI_BASE` and `LBMAIL_AI_MODEL` (`EndpointConfig::with_env`). Other defaults: place `local`, label =
the model name, no extra headers, `thinking: Some(false)` (sends
`chat_template_kwargs.enable_thinking`; set `None` for servers that reject unknown fields), `json_schema: true`
(`response_format: json_schema`), 90 s timeout, 5 s connect timeout, one retry on connect errors, timeouts,
429 and 5xx. Errors carry the status code only, never the server's body. `api_key` is never deserialised
from the file: the daemon fills it from the keyring.

## How the safety rules are kept

- **Sensitive first, always.** `rules::sensitive_message` runs core's `safety::sensitive_sender_kind`, which
  now holds the account-security patterns (new sign-ins, security alerts, 2-step changes,
  more banks, `.gov`), all on word boundaries.
  Any sensitive message makes the thread sensitive: triage is a fixed line (`"A security code from Acme"`,
  `by: "rules"`), `summarise` returns that line, `draft_reply` and `extract` refuse. No model call, ever.
- **The guard can't be forgotten.** A thread reaches a model only through `AccountAi`, which `for_account`
  hands out only when `ai_allowed(account, model.place())`. It re-checks on every call and refuses a thread
  of any other account. `ask` drops hits from accounts not passed in, or not allowed, and sensitive hits.
- **Header rules** (no model): `List-Id`/`List-Unsubscribe`/`Precedence` give newsletter or promotion;
  known tool domains, `Auto-Submitted`, `X-GitHub-Reason` give notification; receipt/order/shipping/
  invitation subjects from no-reply or billing senders give transactional. A thread the user wrote in is a
  conversation and goes to the model.
- **Mail is fenced as untrusted data.** Each prompt gets a fresh random marker; mail sits between
  `<<<UNTRUSTED_MAIL x>>>` and `<<<END_MAIL x>>>`, with fence-like text and chat-template markup (`<|im_start|>`,
  `[INST]`, `<system>`) defused. The system prompt starts with the rule that the fenced text is data. Quoted
  history is stripped, hidden HTML (display:none, zero font, `hidden`) and invisible characters are removed,
  messages and the thread are bounded (newest wins).
- **Suspicious mail** (`injection::reasons`, checked on the raw mail including hidden parts): the model is
  told in a trusted note, `Triage::suspicious` is set, and priority is capped at 40.
- **Outputs are validated**, then repaired once, then fall back:
  - triage: schema-constrained JSON; category from our list (the model cannot say `sensitive`); summary one
    line with no links, addresses, long numbers, markup or signs of obedience ("as instructed", "I have
    forwarded"); dates parsed and checked for plausibility. Fallback: `Other`, 40, `"Mail from X"`, `by: "fallback"`.
  - drafts: recipients, subject and threading are set by code, never the model. Reply goes to the sender;
    a `Reply-To` is honoured only on the sender's own domain (else it's a hijack); reply-all adds the other
    people on the message; only `extra_cc` adds anyone new. Display names that talk to an assistant are dropped.
    Body must not contain links or addresses the user's instruction did not, nor To/Cc/Bcc lines.
  - ask: sources must be the labels it was given; no links; addresses only if they are in the hits.
  - extract: each item and date is checked on its own and dropped if bad.
- `Error::Offline` from the model is passed up (the daemon retries later) rather than stored as a fallback.

## Tests and eval

    cargo test -p lumbridge-mail-ai
    cargo clippy -p lumbridge-mail-ai --no-deps --all-targets -- -D warnings
    cargo run -p lumbridge-mail-ai --example eval

`tests/injection.rs` runs the hostile corpus (`testkit::hostile`: ignore-and-forward, a fake system turn,
hidden HTML, zero-width text, mail posing as the user, JSON breakout, fence breakout, Reply-To hijack, a
hostile display name, a phishing code request) against a fake model that obeys every attack, and checks
nothing of the attack comes out. `tests/pipeline.rs` covers guards, rules-before-model, repair and fallback,
batching, drafts, ask and extraction. `src/http.rs` tests the HTTP model against an in-process server.

The eval sends only the made-up corpora in `testkit` to the endpoint (env `LBMAIL_AI_BASE`,
`LBMAIL_AI_MODEL`, `LBMAIL_AI_THINKING=on|off|unset`, `LBMAIL_AI_NO_SCHEMA=1`) and prints triage accuracy,
latency, backlog throughput, sample summaries/drafts/extractions, ask source accuracy and injection
resistance (raw model answers that carried attack text vs. what reached the output).

`testkit` is public so other crates can build made-up threads and use `ScriptedModel` in their tests.

## Not yet

- No streaming, no hosted-model routing policy beyond `place` (the caller picks the model).
- Voice samples are passed in by the caller; picking them from Sent mail is the daemon's job.
- No cold-email or reply-tracking state across threads (inbox-zero ideas); `needs_reply` is per thread.

Prompt ideas from Zero (MIT, `ai-mail/Zero`); rules and quote handling carried over from our own earlier
mail scripts. No code copied from GPL/AGPL references.
