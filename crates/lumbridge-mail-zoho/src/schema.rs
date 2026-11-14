//! The input schemas of the allowed tools, as Zoho's MCP server publishes them (`tools/list`), trimmed
//! to [`crate::mcp::ALLOWED_TOOLS`] (`tool-schemas.json`). Every call is checked against its tool's
//! schema before it is sent: only the sections and keys the tool declares, every required path key, and
//! the declared JSON type of each value. Zoho answers an undeclared key with a plain-text error ("Invalid
//! keys found in path variable"), so this keeps such mistakes in our tests instead of the live server.
//! The fake server checks requests against the same schemas.

use lumbridge_mail_core::{Error, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::OnceLock;

const SCHEMAS: &str = include_str!("tool-schemas.json");

/// The sections of a tool's arguments.
pub const SECTIONS: [&str; 3] = ["path_variables", "query_params", "body"];

fn all() -> &'static HashMap<String, Value> {
    static S: OnceLock<HashMap<String, Value>> = OnceLock::new();
    S.get_or_init(|| {
        let list: Vec<Value> = serde_json::from_str(SCHEMAS).unwrap_or_default();
        list.into_iter()
            .filter_map(|t| {
                let name = t.get("name")?.as_str()?.to_owned();
                Some((name, t.get("inputSchema")?.clone()))
            })
            .collect()
    })
}

/// The input schema of `tool`, if it is one of ours.
pub fn of(tool: &str) -> Option<&'static Value> {
    all().get(tool)
}

fn type_ok(want: &str, v: &Value) -> bool {
    match want {
        "string" => v.is_string(),
        "integer" => v.as_i64().is_some() || v.as_u64().is_some(),
        "number" => v.is_number(),
        "boolean" => v.is_boolean(),
        "array" => v.is_array(),
        "object" => v.is_object(),
        _ => true,
    }
}

/// Checks `args` against `tool`'s schema. The error names the tool, section and key (never a value).
pub fn check(tool: &str, args: &Value) -> Result<()> {
    let bad = |what: String| {
        Err(Error::Invalid(format!(
            "{tool}: {what} (not as its schema says)"
        )))
    };
    let Some(schema) = of(tool) else {
        return bad("no schema".into());
    };
    let props = schema.get("properties").and_then(Value::as_object);
    let Some(args) = args.as_object() else {
        return bad("arguments are not an object".into());
    };
    for (section, v) in args {
        let Some(sec) = props.and_then(|p| p.get(section)) else {
            return bad(format!("section {section} is not declared"));
        };
        let declared = sec.get("properties").and_then(Value::as_object);
        let Some(obj) = v.as_object() else {
            return bad(format!("{section} is not an object"));
        };
        for (k, val) in obj {
            let Some(d) = declared.and_then(|p| p.get(k)) else {
                return bad(format!("{section}.{k} is not declared"));
            };
            if let Some(t) = d.get("type").and_then(Value::as_str)
                && !type_ok(t, val)
            {
                return bad(format!("{section}.{k} must be {t}"));
            }
        }
    }
    // Required sections and keys (path variables matter: the server builds its URL from them).
    let required = |o: &Value| -> Vec<String> {
        o.get("required")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    for section in required(schema) {
        if section == "path_variables" && !args.contains_key(&section) {
            return bad(format!("{section} is missing"));
        }
    }
    if let Some(sec) = props.and_then(|p| p.get("path_variables")) {
        let given = args.get("path_variables").and_then(Value::as_object);
        for k in required(sec) {
            if given.is_none_or(|g| !g.contains_key(&k)) {
                return bad(format!("path_variables.{k} is missing"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_allowed_tool_has_a_schema() {
        for t in crate::mcp::ALLOWED_TOOLS {
            assert!(of(t).is_some(), "{t}");
        }
        assert_eq!(
            all().len(),
            crate::mcp::ALLOWED_TOOLS.len(),
            "only the allowed tools are kept"
        );
    }

    #[test]
    fn calls_are_checked() {
        let ok = json!({"path_variables": {"accountId": "1", "messageId": "2"}, "query_params": {"rawFormat": true}});
        assert!(check("ZohoMail_get_mail_headers", &ok).is_ok());
        let extra =
            json!({"path_variables": {"accountId": "1", "messageId": "2", "folderId": "3"}});
        let e = check("ZohoMail_get_mail_headers", &extra)
            .unwrap_err()
            .to_string();
        assert!(e.contains("path_variables.folderId"), "{e}");
        assert!(
            check(
                "ZohoMail_get_mail_headers",
                &json!({"path_variables": {"accountId": "1"}})
            )
            .is_err()
        );
        assert!(
            check(
                "ZohoMail_list_mails",
                &json!({"path_variables": {"accountId": "1"}, "query_params": {"limit": "5"}})
            )
            .is_err()
        );
        assert!(
            check(
                "ZohoMail_list_mails",
                &json!({"path_variables": {"accountId": "1"}, "body": {}})
            )
            .is_err()
        );
        assert!(check("ZohoMail_list_accounts", &json!({})).is_ok());
        assert!(check("ZohoMail_deleteUser", &json!({})).is_err());
    }
}
