"""Writes the made-up gog JSON fixtures for lumbridge-mail-gog's tests. Shapes follow gogcli v0.29.0
(internal/cmd) and the Gmail API Message/Label structs it encodes. All people and addresses are invented."""
import base64, json, os, sys
out = sys.argv[1]
def b64u(b, pad=False):
    s = base64.urlsafe_b64encode(b).decode()
    return s if pad else s.rstrip("=")
def w(name, obj):
    with open(os.path.join(out, name), "w") as f:
        json.dump(obj, f, indent=2, ensure_ascii=False); f.write("\n")

def L(id, name, kind="system", **kw):
    d = {"id": id, "name": name, "type": kind, "messageListVisibility": "show", "labelListVisibility": "labelShow"}
    d.update(kw); return d
w("labels_list.json", {"labels": [
    L("INBOX", "INBOX"), L("SENT", "SENT"), L("DRAFT", "DRAFT"), L("TRASH", "TRASH"), L("SPAM", "SPAM"),
    L("STARRED", "STARRED"), L("IMPORTANT", "IMPORTANT"), L("UNREAD", "UNREAD"), L("CHAT", "CHAT"),
    L("CATEGORY_SOCIAL", "CATEGORY_SOCIAL"), L("CATEGORY_PERSONAL", "CATEGORY_PERSONAL"),
    L("Label_11", "Projects/Garden", "user", color={"textColor": "#000000", "backgroundColor": "#16a766"}),
    L("Label_12", "Receipts", "user", labelListVisibility="labelHide"),
]})
w("label_get_inbox.json", {"label": L("INBOX", "INBOX", messagesTotal=120, messagesUnread=3, threadsTotal=90, threadsUnread=2)})

def H(**h): return [{"name": k.replace("_", "-"), "value": v} for k, v in h.items()]
def meta(id, thread, labels, snippet, headers, mime="multipart/alternative", size=4200, hist="5001", date="1762761600000"):
    return {"id": id, "threadId": thread, "labelIds": labels, "snippet": snippet,
            "payload": {"partId": "", "mimeType": mime, "filename": "", "headers": headers, "body": {"size": 0}},
            "sizeEstimate": size, "historyId": hist, "internalDate": date}

M3, M2, M1, M0, M4, M9 = "18f0a1b2c3d4e503", "18f0a1b2c3d4e502", "18f0a1b2c3d4e501", "18f0a1b2c3d4e500", "18f0a1b2c3d4e504", "18f0a1b2c3d4e509"
T2 = "18f0a1b2c3d4e502"
m3_headers = H(From='"Lovelace, Ada" <Ada@Example.com>', To="Test User <test.user@example.com>, bob@example.org",
               Cc="=?UTF-8?B?w4lsaXNl?= <elise@example.net>", Subject="Re: Garden meeting =?UTF-8?Q?=E2=80=94_Friday?=",
               Date="Mon, 10 Nov 2025 08:00:00 +0000", Message_ID="<CAF-ada-3@mail.example.com>",
               In_Reply_To="<CAF-test-2@mail.example.com>", References="<CAF-ada-1@mail.example.com> <CAF-test-2@mail.example.com>",
               List_Unsubscribe="<mailto:unsub@example.com>")
w("m3_metadata.json", meta(M3, T2, ["INBOX", "UNREAD", "IMPORTANT", "Label_11", "CATEGORY_PERSONAL", "STARRED"],
   "Can we move the garden meeting to Friday? I&#39;ll bring the seeds &amp; the plan.", m3_headers, mime="multipart/mixed", size=48213, hist="5003", date="1762761600000"))
w("m2_metadata.json", meta(M2, T2, ["SENT"], "Sure, Thursday works.", H(From="Test User <test.user@example.com>", To="Ada <ada@example.com>",
   Subject="Re: Garden meeting", Message_ID="<CAF-test-2@mail.example.com>", In_Reply_To="<CAF-ada-1@mail.example.com>", References="<CAF-ada-1@mail.example.com>"), hist="4990", date="1762675200000"))
w("m1_metadata.json", meta(M1, T2, ["INBOX"], "Shall we meet about the garden?", H(From="ada@example.com", To="test.user@example.com",
   Subject="Garden meeting", Message_ID="<CAF-ada-1@mail.example.com>"), mime="text/plain", hist="4980", date="1762588800000"))
w("m0_metadata.json", meta(M0, "18f0a1b2c3d4e500", ["TRASH", "Label_12"], "Your receipt", H(From="Shop <orders@shop.example>", To="test.user@example.com",
   Subject="Your receipt"), mime="text/plain", hist="4000", date="1700000000000"))
w("m4_metadata.json", meta(M4, "18f0a1b2c3d4e504", ["INBOX", "UNREAD", "CATEGORY_SOCIAL"], "New comment on your post",
   H(From="Club <noreply@club.example>", To="test.user@example.com", Subject="New comment"), hist="5100", date="1762848000000"))
# m2 after being read elsewhere: still SENT but now also STARRED (a label change history does not report).
w("m2_metadata_starred.json", meta(M2, T2, ["SENT", "STARRED"], "Sure, Thursday works.", H(From="Test User <test.user@example.com>", To="Ada <ada@example.com>",
   Subject="Re: Garden meeting", Message_ID="<CAF-test-2@mail.example.com>"), hist="5101", date="1762675200000"))

def item(id, thread, labels): return {"id": id, "threadId": thread, "date": "2025-11-10 08:00", "from": "x", "subject": "x", "labels": labels}
w("search_full_page1.json", {"messages": [item(M3, T2, ["INBOX", "UNREAD", "IMPORTANT", "Projects/Garden", "CATEGORY_PERSONAL", "STARRED"]),
                                          item(M2, T2, ["SENT"]), item(M1, T2, ["INBOX"])], "nextPageToken": "page-2-token"})
w("search_full_page2.json", {"messages": [item(M0, "18f0a1b2c3d4e500", ["TRASH", "Receipts"])], "nextPageToken": ""})
w("search_reconcile.json", {"messages": [item(M4, "18f0a1b2c3d4e504", ["INBOX", "UNREAD", "CATEGORY_SOCIAL"]),
                                         item(M3, T2, ["INBOX", "UNREAD", "IMPORTANT", "Projects/Garden", "CATEGORY_PERSONAL", "STARRED"]),
                                         item(M2, T2, ["SENT", "STARRED"]), item(M1, T2, ["INBOX"])], "nextPageToken": ""})
w("search_empty.json", {"messages": [], "nextPageToken": ""})
w("history.json", {"historyId": "5200", "messages": [M4, M9, M4], "nextPageToken": ""})
w("history_probe_empty.json", {"historyId": "5050", "messages": [], "nextPageToken": ""})

text = "Can we move the garden meeting to Friday?\r\nI'll bring the seeds.\r\n"
html = "<p>Can we move the garden meeting to <b>Friday</b>?</p>"
latin = "Grüße aus dem Garten".encode("iso-8859-1")
png = bytes([0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3])
full = meta(M3, T2, ["INBOX", "UNREAD"], "Can we move", m3_headers, mime="multipart/mixed", size=48213, hist="5003")
full["payload"]["parts"] = [
    {"partId": "0", "mimeType": "multipart/alternative", "filename": "", "headers": H(Content_Type='multipart/alternative; boundary="b2"'), "body": {"size": 0}, "parts": [
        {"partId": "0.0", "mimeType": "text/plain", "filename": "", "headers": H(Content_Type='text/plain; charset="UTF-8"'), "body": {"size": len(text), "data": b64u(text.encode(), pad=True)}},
        {"partId": "0.1", "mimeType": "text/html", "filename": "", "headers": H(Content_Type="text/html; charset=UTF-8"), "body": {"size": len(html), "data": b64u(html.encode())}},
    ]},
    {"partId": "1", "mimeType": "text/plain", "filename": "", "headers": H(Content_Type="text/plain; charset=iso-8859-1"), "body": {"size": len(latin), "data": b64u(latin)}},
    {"partId": "2", "mimeType": "application/pdf", "filename": "garden-plan.pdf", "headers": H(Content_Type='application/pdf; name="garden-plan.pdf"', Content_Disposition='attachment; filename="garden-plan.pdf"'),
     "body": {"attachmentId": "ANGjdJ8-made-up-attachment-id", "size": 40960}},
    {"partId": "3", "mimeType": "image/png", "filename": "", "headers": H(Content_Type="image/png", Content_ID="<seed-photo@example.com>", Content_Disposition="inline"),
     "body": {"size": len(png), "data": b64u(png)}},
]
w("m3_full.json", full)
raw = ("From: \"Lovelace, Ada\" <ada@example.com>\r\nTo: test.user@example.com\r\nSubject: Garden meeting\r\n"
       "Message-ID: <CAF-ada-3@mail.example.com>\r\n\r\nCan we move the garden meeting to Friday?\r\n").encode()
w("m3_raw.json", {"id": M3, "threadId": T2, "labelIds": ["INBOX"], "raw": b64u(raw), "sizeEstimate": len(raw), "historyId": "5003", "internalDate": "1762761600000"})
w("m3_minimal.json", {"id": M3, "threadId": T2, "labelIds": ["INBOX"], "historyId": "5003", "internalDate": "1762761600000", "sizeEstimate": 48213})
w("drafts_list.json", {"drafts": [{"id": "r-1111111111111111111", "messageId": "18f0a1b2c3d4e5d1", "threadId": "18f0a1b2c3d4e5d1"},
                                  {"id": "r-2222222222222222222", "messageId": "18f0a1b2c3d4e5d2", "threadId": T2}], "nextPageToken": ""})
w("draft_create.json", {"draftId": "r-2222222222222222222", "message": {"id": "18f0a1b2c3d4e5d2", "threadId": T2, "labelIds": ["DRAFT"]}, "threadId": T2})
w("batch_modify.json", {"modified": [M3], "count": 1, "addedLabels": [], "removedLabels": ["UNREAD"]})
w("send.json", {"from": "test.user@example.com", "messageId": "18f0a1b2c3d4e5e1", "threadId": T2})
w("attachment_download.json", {"path": "/tmp/x/out/file", "cached": False, "bytes": 5})
w("version_0_29.json", {"version": "v0.29.0", "commit": "abc1234", "date": "2026-06-20"})
w("version_0_48.json", {"version": "v0.48.1", "commit": "def5678", "date": "2026-11-01"})
w("deleted.json", {"deleted": True, "draftId": "r-2222222222222222222"})
