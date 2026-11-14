//! schema.org objects out of received HTML: JSON-LD (`<script type="application/ld+json">`) and
//! microdata (`itemscope` / `itemtype` / `itemprop`). Senders put these in transactional mail for
//! machines (Gmail's "email markup"): orders, flight and hotel reservations, parcel deliveries,
//! events. They are still written by the sender: data for the rules in [`crate::rules`], validated
//! like anything else, never shown and never followed.
//!
//! The output is bounded: at most [`MAX_OBJECTS`] objects, nesting [`MAX_DEPTH`], strings
//! [`MAX_STRING`] characters, arrays [`MAX_ARRAY`] items, [`MAX_BYTES`] serialised in all.

use html5ever::tendril::TendrilSink;
use html5ever::{ParseOpts, parse_document};
use lumbridge_mail_core::Body;
use markup5ever_rcdom::{Handle, NodeData, RcDom};
use serde_json::{Map, Value};

pub const MAX_OBJECTS: usize = 20;
pub const MAX_DEPTH: usize = 6;
pub const MAX_STRING: usize = 500;
pub const MAX_ARRAY: usize = 20;
pub const MAX_BYTES: usize = 60 << 10;
/// HTML beyond this is not looked at.
const MAX_HTML: usize = 2 << 20;
/// Elements visited per document.
const MAX_NODES: usize = 50_000;

/// The schema.org objects in a body's HTML, as a JSON array (`[]` when none or no HTML).
pub fn from_body(body: &Body) -> Value {
    match body.html.as_deref() {
        Some(h) if !h.is_empty() => Value::Array(from_html(h)),
        _ => Value::Array(Vec::new()),
    }
}

/// The schema.org objects in `html`.
pub fn from_html(html: &str) -> Vec<Value> {
    let html = if html.len() > MAX_HTML {
        let mut end = MAX_HTML;
        while !html.is_char_boundary(end) {
            end -= 1;
        }
        &html[..end]
    } else {
        html
    };
    // Cheap test first: most mail has neither.
    let lower = html.to_ascii_lowercase();
    if !lower.contains("ld+json") && !lower.contains("itemscope") {
        return Vec::new();
    }
    let dom: RcDom = parse_document(RcDom::default(), ParseOpts::default()).one(html);
    let mut out = Vec::new();
    let mut visited = 0usize;
    let mut stack: Vec<Handle> = vec![dom.document.clone()];
    while let Some(h) = stack.pop() {
        visited += 1;
        if visited > MAX_NODES || out.len() >= MAX_OBJECTS {
            break;
        }
        if let NodeData::Element { name, attrs, .. } = &h.data {
            let tag = local_str(&name.local).to_ascii_lowercase();
            let attr = |n: &str| {
                attrs
                    .borrow()
                    .iter()
                    .find(|a| local_is(&a.name.local, n))
                    .map(|a| a.value.to_string())
            };
            if tag == "script" {
                if attr("type")
                    .is_some_and(|t| t.trim().eq_ignore_ascii_case("application/ld+json"))
                {
                    let text = text_of(&h, 256 << 10);
                    if let Ok(v) = serde_json::from_str::<Value>(text.trim()) {
                        flatten(v, &mut out);
                    }
                }
                continue;
            }
            if attr("itemscope").is_some() && attr("itemprop").is_none() {
                if let Some(v) = microdata_item(&h, 0) {
                    out.push(v);
                }
                continue;
            }
        }
        stack.extend(h.children.borrow().iter().rev().cloned());
    }
    let mut objects: Vec<Value> = out
        .into_iter()
        .filter_map(|v| normalise(v, 0))
        .filter(|v| v.get("@type").and_then(Value::as_str).is_some())
        .take(MAX_OBJECTS)
        .collect();
    while !objects.is_empty()
        && serde_json::to_string(&objects)
            .map(|s| s.len())
            .unwrap_or(0)
            > MAX_BYTES
    {
        objects.pop();
    }
    objects
}

fn local_str(l: &html5ever::LocalName) -> &str {
    l
}

fn local_is(l: &html5ever::LocalName, n: &str) -> bool {
    local_str(l).eq_ignore_ascii_case(n)
}

/// Top-level arrays and `@graph` become separate objects.
fn flatten(v: Value, out: &mut Vec<Value>) {
    match v {
        Value::Array(a) => {
            for x in a.into_iter().take(MAX_OBJECTS) {
                flatten(x, out);
            }
        }
        Value::Object(mut o) => match o.remove("@graph") {
            Some(Value::Array(g)) => {
                for x in g.into_iter().take(MAX_OBJECTS) {
                    if out.len() < MAX_OBJECTS {
                        out.push(x);
                    }
                }
            }
            _ => {
                if out.len() < MAX_OBJECTS {
                    out.push(Value::Object(o));
                }
            }
        },
        _ => {}
    }
}

/// `http://schema.org/FlightReservation` -> `FlightReservation`; the first of several.
fn short_type(v: &Value) -> Option<String> {
    let s = match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().find_map(Value::as_str)?.to_string(),
        _ => return None,
    };
    let t = s.trim().trim_end_matches('/');
    let t = t.rsplit(['/', '#', ':']).next().unwrap_or(t);
    (!t.is_empty() && t.len() <= 80 && t.chars().all(|c| c.is_ascii_alphanumeric()))
        .then(|| t.to_string())
}

fn clean_string(s: &str) -> String {
    let c = lumbridge_mail_ai::text::clean(s);
    let c = c.trim();
    c.chars().take(MAX_STRING).collect()
}

/// Bounded copy: known shapes only, `@context` and `@id` dropped, `@type` shortened.
fn normalise(v: Value, depth: usize) -> Option<Value> {
    if depth > MAX_DEPTH {
        return None;
    }
    Some(match v {
        Value::String(s) => Value::String(clean_string(&s)),
        Value::Number(_) | Value::Bool(_) => v,
        Value::Null => return None,
        Value::Array(a) => Value::Array(
            a.into_iter()
                .filter_map(|x| normalise(x, depth + 1))
                .take(MAX_ARRAY)
                .collect(),
        ),
        Value::Object(o) => {
            let mut m = Map::new();
            for (k, val) in o.into_iter().take(100) {
                if k == "@context" || k == "@id" || k.len() > 60 {
                    continue;
                }
                if k == "@type" {
                    if let Some(t) = short_type(&val) {
                        m.insert(k, Value::String(t));
                    }
                    continue;
                }
                if let Some(x) = normalise(val, depth + 1) {
                    m.insert(k, x);
                }
            }
            Value::Object(m)
        }
    })
}

/// All text under `h`, at most `max` bytes.
fn text_of(h: &Handle, max: usize) -> String {
    let mut out = String::new();
    let mut stack: Vec<Handle> = h.children.borrow().iter().rev().cloned().collect();
    while let Some(n) = stack.pop() {
        match &n.data {
            NodeData::Text { contents } => out.push_str(&contents.borrow()),
            NodeData::Element { name, .. } => {
                if matches!(&*name.local, "script" | "style" | "template") {
                    continue;
                }
                out.push(' ');
                stack.extend(n.children.borrow().iter().rev().cloned());
            }
            _ => {}
        }
        if out.len() > max {
            break;
        }
    }
    out
}

fn attr_of(h: &Handle, n: &str) -> Option<String> {
    match &h.data {
        NodeData::Element { attrs, .. } => attrs
            .borrow()
            .iter()
            .find(|a| local_is(&a.name.local, n))
            .map(|a| a.value.to_string()),
        _ => None,
    }
}

/// One microdata item: its type and properties (nested items as objects).
fn microdata_item(h: &Handle, depth: usize) -> Option<Value> {
    if depth > MAX_DEPTH {
        return None;
    }
    let mut obj = Map::new();
    if let Some(t) = attr_of(h, "itemtype").and_then(|t| {
        short_type(&Value::String(
            t.split_whitespace().next().unwrap_or_default().to_string(),
        ))
    }) {
        obj.insert("@type".into(), Value::String(t));
    }
    let mut stack: Vec<Handle> = h.children.borrow().iter().rev().cloned().collect();
    let mut visited = 0;
    while let Some(n) = stack.pop() {
        visited += 1;
        if visited > MAX_NODES {
            break;
        }
        let NodeData::Element { name, .. } = &n.data else {
            continue;
        };
        let props = attr_of(&n, "itemprop");
        let scope = attr_of(&n, "itemscope").is_some();
        match (props, scope) {
            (Some(props), true) => {
                if let Some(v) = microdata_item(&n, depth + 1) {
                    for p in props.split_whitespace().take(4) {
                        push_prop(&mut obj, p, v.clone());
                    }
                }
            }
            (Some(props), false) => {
                let tag = local_str(&name.local).to_ascii_lowercase();
                let value = attr_of(&n, "content")
                    .or_else(|| match tag.as_str() {
                        "time" => attr_of(&n, "datetime"),
                        "a" | "link" | "area" => attr_of(&n, "href"),
                        "img" | "audio" | "video" | "source" | "embed" | "iframe" => {
                            attr_of(&n, "src")
                        }
                        "meta" => None,
                        "data" | "meter" => attr_of(&n, "value"),
                        _ => None,
                    })
                    .unwrap_or_else(|| text_of(&n, 4096));
                for p in props.split_whitespace().take(4) {
                    push_prop(&mut obj, p, Value::String(clean_string(&value)));
                }
                // Properties further down belong to this item too.
                stack.extend(n.children.borrow().iter().rev().cloned());
            }
            // Another top-level item: not ours.
            (None, true) => {}
            (None, false) => stack.extend(n.children.borrow().iter().rev().cloned()),
        }
    }
    Some(Value::Object(obj))
}

fn push_prop(obj: &mut Map<String, Value>, p: &str, v: Value) {
    let key = p.rsplit(['/', '#']).next().unwrap_or(p).to_string();
    if key.is_empty() || key.len() > 60 {
        return;
    }
    match obj.get_mut(&key) {
        None => {
            obj.insert(key, v);
        }
        Some(Value::Array(a)) => {
            if a.len() < MAX_ARRAY {
                a.push(v);
            }
        }
        Some(old) => {
            let first = old.take();
            *old = Value::Array(vec![first, v]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_ld_objects_arrays_and_graphs() {
        let html = r#"<html><head>
<script type="application/ld+json">{"@context": "http://schema.org", "@type": "FlightReservation",
 "reservationNumber": "RXJ34P", "reservationFor": {"@type": "Flight", "flightNumber": "110",
 "airline": {"@type": "Airline", "name": "Example Air", "iataCode": "EX"},
 "departureAirport": {"@type": "Airport", "iataCode": "SFO"}, "arrivalAirport": {"@type": "Airport", "iataCode": "LIS"},
 "departureTime": "2027-03-14T08:05:00-07:00"}}</script>
<script type="application/ld+json">[{"@type": "http://schema.org/Order", "orderNumber": "A-1"}, {"no": "type"}]</script>
<script type="application/ld+json">{"@graph": [{"@type": "ParcelDelivery", "trackingNumber": "1Z999AA10123456784"}]}</script>
<script type="application/ld+json">{ not json </script>
<script>{"@type": "Order", "orderNumber": "ignored: not ld+json"}</script>
</head><body><p>Hi</p></body></html>"#;
        let v = from_html(html);
        assert_eq!(v.len(), 3, "{v:#?}");
        assert_eq!(v[0]["@type"], "FlightReservation");
        assert!(v[0].get("@context").is_none());
        assert_eq!(v[0]["reservationFor"]["airline"]["iataCode"], "EX");
        assert_eq!(v[1]["@type"], "Order");
        assert_eq!(v[2]["trackingNumber"], "1Z999AA10123456784");
    }

    #[test]
    fn microdata_items_with_nested_items() {
        let html = r#"<div itemscope itemtype="http://schema.org/ParcelDelivery">
  <div itemprop="carrier" itemscope itemtype="http://schema.org/Organization"><meta itemprop="name" content="Example Post"/></div>
  <span itemprop="trackingNumber">EP123456789XX</span>
  <time itemprop="expectedArrivalUntil" datetime="2026-11-20">Thursday</time>
  <a itemprop="trackingUrl" href="https://post.example/t/EP123456789XX">Track</a>
  <div itemscope itemtype="http://schema.org/Order"><span itemprop="orderNumber">O-9</span></div>
</div>"#;
        let v = from_html(html);
        assert_eq!(
            v.len(),
            1,
            "a nested top-level item is not a property: {v:#?}"
        );
        assert_eq!(v[0]["@type"], "ParcelDelivery");
        assert_eq!(v[0]["carrier"]["name"], "Example Post");
        assert_eq!(v[0]["trackingNumber"], "EP123456789XX");
        assert_eq!(v[0]["expectedArrivalUntil"], "2026-11-20");
        assert_eq!(v[0]["trackingUrl"], "https://post.example/t/EP123456789XX");
        assert!(v[0].get("orderNumber").is_none());
    }

    #[test]
    fn output_is_bounded_and_plain_mail_costs_nothing() {
        assert!(from_html("<p>Hello <b>there</b></p>").is_empty());
        let big = format!(
            r#"<script type="application/ld+json">{{"@type": "Order", "description": "{}", "x": "a{}b"}}</script>"#,
            "y".repeat(5000),
            "\u{200B}".repeat(3)
        );
        let v = from_html(&big);
        assert_eq!(
            v[0]["description"].as_str().unwrap().chars().count(),
            MAX_STRING
        );
        assert_eq!(v[0]["x"], "ab", "invisible characters removed");
        let mut deep = String::from(r#"{"@type": "Order", "a": "#);
        for _ in 0..50 {
            deep.push_str(r#"{"b": "#);
        }
        deep.push('1');
        for _ in 0..50 {
            deep.push('}');
        }
        deep.push('}');
        let v = from_html(&format!(
            r#"<script type="application/ld+json">{deep}</script>"#
        ));
        assert_eq!(v.len(), 1);
        assert!(
            serde_json::to_string(&v).unwrap().len() < 400,
            "depth capped"
        );
        let many: String = (0..100)
            .map(|i| format!(r#"<script type="application/ld+json">{{"@type": "Order", "orderNumber": "{i}"}}</script>"#))
            .collect();
        assert_eq!(from_html(&many).len(), MAX_OBJECTS);
        let body = Body {
            html: Some(
                r#"<script type="application/ld+json">{"@type":"Event","name":"Launch"}</script>"#
                    .into(),
            ),
            ..Body::default()
        };
        assert_eq!(
            from_body(&body),
            json!([{"@type": "Event", "name": "Launch"}])
        );
        assert_eq!(from_body(&Body::default()), json!([]));
    }
}
