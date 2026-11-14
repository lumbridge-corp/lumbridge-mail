# lumbridge-mail-render

Received mail to things that are safe to show, and drafts to MIME-ready text + HTML. Pure functions:
no I/O, no network, no UI, no GPUI. Mail is hostile input, so nothing here panics or fails on bad mail;
every entry point is bounded by `Limits` and reports what it cut in `Report::truncated`.

```rust
use lumbridge_mail_render::{blocks, sanitize_body, plain_text, snippet, header_info, compose, Policy, TextOptions};

let policy = Policy::default();                 // remote content blocked, colours off, folding on
let doc = blocks(&body, &policy);               // native block tree for the reader
let safe = sanitize_body(&body, &policy);       // Option<SafeHtml> for an HTML renderer (stage 2, print)
let row = snippet(&body, 140);                  // list-row line
let for_ai = plain_text(&body, &TextOptions::for_ai()); // new content + link targets, no history
let info = header_info(&body, meta.from.first()); // unsubscribe, list id, reply-to, SPF/DKIM/DMARC
let out = compose(markdown, Some(&original));   // Outgoing { text, html } for multipart/alternative
```

## What it does

| Function | Gives |
|---|---|
| `blocks(body, policy) -> Document` | The HTML part (or text part) as a block tree. See below. |
| `sanitize(html, attachments, policy) -> SafeHtml`, `sanitize_body` | Sanitised HTML fragments `html` / `signature` / `quoted` + `report`. |
| `plain_text(body, opts)`, `snippet(body, max)`, `text::document_text(doc, opts)` | Clean text. |
| `fold_text(text) -> FoldedText` | Plain text split into `body` / `signature` / `quoted`. |
| `header_info(body, from) -> HeaderInfo` | `unsubscribe` (web, one-click, mailto), `list_id`, `reply_to`, `reply_to_differs`, `auth`. |
| `headers::{list_unsubscribe, parse_addresses, auth_summary}` | The pieces of `header_info`. |
| `compose(md, original)`, `compose::compose_blocks`, `compose::markdown_blocks`, `compose::blocks_to_html`, `compose::subject_for` | Outgoing parts. |
| `charset::{decode, repair, decode_words}` | Bytes in a declared charset, mojibake repair, RFC 2047 words. |
| `links::{safe_href, link_warning, is_tracker_url, strip_tracking_params}` | URL rules. |

### Policy

```rust
Policy {
    remote: RemoteContent::Blocked,  // Allowed when the user presses "Load images" (per message / sender)
    keep_colors: false,              // sanitised HTML only; the block tree never has colours
    fold: true,                      // split signature and quoted history out
    cid_base: None,                  // Some("lbm-attachment://<msg>/") => cid: images point at <base><attachment id>
    limits: Limits::default(),       // 4 MiB input, 150k nodes, depth 64, 4000 blocks, 400k chars, 512 KiB data: images
}
```

### Report

Every `Document` and `SafeHtml` carries a `Report`:

- `remote_images` (real content images) and `remote_blocked` (they were not loaded): drive the
  "Load images" bar.
- `tracking_pixels`: 1x1 / hidden images and known tracker hosts. Always dropped, also when remote content
  is allowed.
- `remote_css`: remote loads through CSS (`<style>`, `<link rel=stylesheet>`, `url()` in inline styles,
  `background=`). Always dropped.
- `cid_images` / `cid_unresolved`.
- `suspicious_links: Vec<LinkWarning>` (max 50): `text_shows_other_domain {shown, actual}`,
  `punycode {host, unicode}`, `ip_address {host}`, `userinfo {actual}`.
- `removed`: counts of scripts, event handlers, forms, frames, stylesheets, meta refresh, base tags,
  svg/math, unsafe links, data: URIs, hidden elements.
- `truncated`: a limit was hit.

## The block tree (for the app)

`Document { blocks, signature, quoted, report }`. Show `blocks`; put `signature` and `quoted` behind a
"..." toggle (both are empty when there is nothing to fold). All types are serde with
`#[serde(tag = "type", rename_all = "snake_case")]`, so the daemon can send them over JSON-RPC as is.

```rust
enum Block {
    Paragraph { spans: Vec<Span> },                         // running text; '\n' inside span text = hard break
    Heading   { level: u8 /*1..=6*/, spans: Vec<Span> },
    List      { ordered: bool, start: u32, items: Vec<Vec<Block>> },   // each item is blocks (nested lists inside)
    Quote     { depth: u8, blocks: Vec<Block> },            // depth 1 = outermost; nested quotes are Quote blocks inside
    Code      { text: String },                             // preformatted, whitespace exact, monospace
    Table     { rows: Vec<Vec<Cell>>, header_rows: u32 },   // small text grids only; rows may differ in length
    Image(Image),
    Rule,                                                   // horizontal divider
}

struct Span  { text: String, bold: bool, italic: bool, code: bool, strike: bool, link: Option<Link> }
struct Link  { href: String /* http(s), mailto or tel only */, warning: Option<WarningKind> }
struct Cell  { spans: Vec<Span>, header: bool, colspan: u32 /* >= 1 */ }
struct Image { source: ImageSource, alt: Option<String>, width: Option<u32>, height: Option<u32>, link: Option<Link> }

enum ImageSource {                       // #[serde(tag = "kind")]
    Cid { content_id: String, attachment: Option<AttachmentId> }, // fetch the part; None = no such part
    Remote { url: String },              // only when remote content is allowed
    Blocked { host: String },            // placeholder box: alt text + size + "from <host>"
    Data { uri: String },                // small inline data:image/(png|gif|jpeg|webp);base64
    Unavailable,                         // paint the alt text only
}
```

JSON, for example:

```json
[{"type":"heading","level":1,"spans":[{"text":"Meet Widget Pro"}]},
 {"type":"paragraph","spans":[{"text":"Ships on "},{"text":"November 20","bold":true},{"text":"."}]},
 {"type":"image","source":{"kind":"blocked","host":"cdn.widgets.example.com"},"alt":"Hero","width":600,"height":300},
 {"type":"quote","depth":1,"blocks":[{"type":"paragraph","spans":[{"text":"Lunch?"}]}]}]
```

Painting notes:

- Theme: the tree has no colours, fonts or font sizes. Use the app's text colour, link colour and
  heading scale, so mail reads the same in light and dark mode.
- Spans: merge-adjacent is already done; render `code` in monospace, `strike` with a line-through.
  A span with `link.warning` should show the real host (and the warning) on hover / before opening.
- Images: scale down to the reader width, keep `width`/`height` as the aspect ratio hint. `Blocked`
  images are where the "Load images" bar applies; re-render with `Policy::remote_allowed()` to get
  `Remote` sources. Small icons (both sides <= 32 px) never appear as images: their alt text is inline.
- Quotes: indent by depth with a vertical bar; cap the visual indent (deep threads go 10+ levels).
- Tables: a simple grid; `header_rows` leading rows are headers. Layout tables never become `Table`;
  they are linearised into paragraphs and images in reading order.
- Lists: `start` is the first number for ordered lists.

## How it works

1. Input is cut to `max_input_bytes`, mojibake and control characters are repaired (`charset::repair`),
   and a byte scan cuts pathological nesting (>1500 open elements) before parsing.
2. html5ever parses the document; we copy it into a small owned tree with a depth (64) and node budget.
3. The cleaning pass (`clean.rs`) drops active content (script, style, iframe/object/embed, svg/math,
   forms' controls, meta, base, link, template, noscript, video/audio, ...), hidden elements
   (`display:none`, `max-height:0;overflow:hidden`, `opacity:0`, `mso-hide:all`, `hidden`), `on*`
   attributes and unknown tags (content kept). It vets every link (`links::safe_href`: http/https/mailto/tel;
   browsers' tab/newline stripping is applied first) and classifies every image (cid / data / remote /
   tracker). Inline styles go through an allowlist with no `url()`, no `expression`, no escapes, no
   `position`, and colours only with `keep_colors`.
4. Folding (`fold.rs`) splits at Gmail/Apple/Thunderbird/Yahoo/Outlook/Proton quote markers and
   signature markers; for text, at "On ... wrote:" + a quoted tail, "-----Original Message-----",
   Outlook "From:/Sent:/Subject:" blocks and trailing `>` runs; signatures at `-- ` and "Sent from my ...".
   Forwards and inline (interleaved) replies are never folded.
5. Sanitised HTML is serialised from the cleaned tree and then run through **ammonia** with its own
   strict allowlist (defence in depth), with `rel="noopener noreferrer nofollow"` on links and relative
   URLs denied. The block tree is built from the cleaned tree directly.

## Testing

    cargo test -p lumbridge-mail-render
    cargo clippy -p lumbridge-mail-render --all-targets -- -D warnings

`tests/corpus.rs` runs snapshot tests (insta, `tests/snapshots/`) over made-up mail in `tests/fixtures/`:
plain replies, inline replies, Outlook plain/HTML/web replies, Gmail/Apple/Thunderbird replies, forwards,
nested quotes, a layout-table newsletter with preheader, tracking pixels and List-Unsubscribe headers,
a receipt with a data table, cid images, hostile HTML (scripts, handlers, `javascript:` in many spellings,
SVG, MathML, mXSS-style noscript, comment tricks, iframes, forms, meta refresh, base, data: URIs, CSS
exfiltration with `@import`/attribute selectors/`url()`, CSS escapes, homograph and mismatched links), broken
encodings and broken markup. Huge and pathological input (5 MB, 30k-deep nesting of several tags, 500x200
tables, 40k links, 100k-line text) is generated in the tests and must finish within a time bound.

To accept intended snapshot changes: `INSTA_UPDATE=always cargo test -p
lumbridge-mail-render`, then review the diff in `tests/snapshots/`.

## Known gaps

- `<style>` blocks are dropped entirely (the CSS-exfiltration vector). A stage-2 Blitz reader that wants
  sender layout needs a real CSS sanitiser (cssparser) for them.
- No raw-MIME parsing: we take `Body` as providers decode it. `charset::decode` is there for a provider
  that hands over bytes. (`mail-parser` 1.0 needs Rust 1.98; the workspace is on 1.97.)
- Registrable domains use a small built-in suffix list, not the Public Suffix List; link warnings are
  hints, not a security boundary.
- Quoting HTML originals in replies keeps `cid:` images as `cid:` references; the outgoing MIME builder
  has to attach those parts (or drop them) for recipients to see them.
- `format=flowed` text is shown with its hard breaks.

## Credits

See the top-level `THIRD-PARTY-NOTICES`. Ideas from overlook (MIT) for hidden-content rules, quote markers
and the sanitiser allowlist shape. No code from GPL or unlicensed references.
