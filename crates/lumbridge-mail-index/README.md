# lumbridge-mail-index

What makes mail in Postgres searchable by meaning and parsed into structured facts. Pure logic plus one
HTTP client (the embedding endpoint); the daemon's workers (`crates/lumbridge-maild/src/pgwork/`) read
and write Postgres through `lumbridge-mail-pg::work`. Design and commands: `docs/MAIL-POSTGRES.md`.

| Module | Does |
|---|---|
| `chunk` | The text that is embedded: `Subject: ...\nFrom: <sender name>\n\n` + a window of the new body text (quoted history removed, invisible characters removed). Windows of 3,000 characters (head included) with 300 overlap, cut at a paragraph, sentence or word; at most 4 per message. Each chunk carries a sha256: unchanged mail is never embedded again. |
| `embed` | `Embedder` trait and `OpenAiEmbedder` (POST `{base_url}/embeddings`, `encoding_format: float`, the configured headers on every request, optional `input_type: passage/query`, query/passage prefixes, `dimensions`). Errors carry the status only, never the server's body. |
| `pace` | `Pacer`: at most N starts per minute, spread evenly, and at most M in flight. |
| `structured` | schema.org objects from received HTML: JSON-LD and microdata, flattened (`@graph`, arrays), `@type` shortened, bounded (20 objects, depth 6, 500-character strings, 60 KiB). |
| `rules` | Deterministic extractors (below). |
| `model` | Model extractors: which kind to ask about (`candidate`), the fenced prompt, the JSON schema, the call, validation. |
| `fact` | Fact kinds, `MailInput`, and `validate` (below). |
| `fake` | An in-process OpenAI-compatible embeddings server with deterministic topic vectors, request recording, failures, delay, down. For tests here and in the daemon. |

## Extractors

Rules first (no model, zero cost), then at most one model call per message.

| Kind | Rules (`parser`) | Model (`model/<label>`) |
|---|---|---|
| receipt (receipt, order, invoice) | schema.org `Order`, paid `Invoice` (`rules/jsonld`) | merchant, total, currency, date, order id |
| travel (flight, train, bus, hotel, car, ferry) | `FlightReservation`, `TrainReservation`, `BusReservation`, `LodgingReservation`, `RentalCarReservation` | carrier, number, from/to, depart/arrive, confirmation code |
| package | `ParcelDelivery`; UPS `1Z...` and USPS tracking numbers in shipping mail (`rules/text`) | carrier, tracking id, status, expected date |
| subscription | | service, event (started, renewal, trial ending, cancelled, ...), amount, period, renews on |
| recruiting | | company, role, recruiter, stage, asks (up to five short lines) |
| bill | unpaid schema.org `Invoice` with a due date | payee, amount, currency, due date |
| event | `EventReservation`, `FoodEstablishmentReservation`, `*Event`; calendar invites (`text/calendar` part, `rules/invite`) | what, when, end, where |
| newsletter | `List-Id`, `List-Unsubscribe` (https and mailto targets), `List-Unsubscribe-Post` one-click (`rules/headers`) | never |

Which kind to ask the model about is decided in code: the thread's triage category allows a set
(`transactional`: everything but recruiting; `notification`: package, event; `important`/`personal`/`other`:
recruiting, event, travel; `newsletter`/`promotion`/`spam`/`sensitive`: nothing; not triaged with list
headers: nothing), the kinds the rules found are skipped, and the remaining kind with the most words for it
(subject hit 2, body hit 1, at least 2) wins. Most mail costs no call.

## Validation (`fact::validate`)

Every fact, from rules or a model: known fields only; names one line, at most 80-120 characters, no links,
addresses, chat markup, invisible characters, or signs the writer obeyed the mail (the AI crate's
detectors); enums checked; amounts numbers in 0..=1,000,000 with at most two decimals; currencies from an
ISO 4217 list; dates real calendar dates (`YYYY-MM-DD` or `YYYY-MM-DDTHH:MM[:SS][offset]`) in a window
around the mail's own date (a receipt from the last 400 days, a trip up to 550 days ahead, arrival after
departure, ...); required fields per kind. From a model, amounts and identifiers (order id, tracking id,
confirmation code, flight number) must appear in the mail's text. Anything that fails stores nothing.

## Mail is data

Mail that talks to an assistant (`lumbridge_mail_ai::injection::reasons` on subject and body) gets the
rules only: no model reads it. Everything else reaches a model fenced as untrusted data under a per-call
marker (`lumbridge_mail_ai::prompt`), with fence-like and chat markup defused, under the AI crate's
preamble; answers are JSON-schema constrained and then validated as above. Nothing here logs.

## Tests

    cargo test -p lumbridge-mail-index
    cargo clippy -p lumbridge-mail-index --all-targets -- -D warnings

Chunking (short, long, overlap, caps, no quoted history, hashes), the pacer (spacing, concurrency), the
embedding client against the fake (headers, bearer, order, errors without bodies, unreachable), structured
data (JSON-LD objects, arrays, graphs, broken JSON, microdata with nested items, bounds), every rule with
good and hostile/absurd structured data, the candidate choice (most mail: none), the model extractors with
a scripted model: a good case per kind, mail that addresses an assistant never sent, fence-breaking text
defused, wrong totals, invented ids, absurd dates, links, obedience, bad JSON, low or out-of-range confidence.
