use std::collections::{BTreeSet, VecDeque};
use std::fmt::Write as _;

use serde_json::{Map, Value};

use crate::ErrorCode;
use crate::catalog::{Catalog, Description, Payload, refs};

const BEGIN: &str = "<!-- generated: ";
const END: &str = "<!-- end generated -->";

const NAMED: &[&str] = &[
    "Session",
    "AttachmentInfo",
    "StatusRecord",
    "Page",
    "Subscribed",
    "Sent",
    "Waited",
    "Read",
    "Tail",
    "Limits",
    "Catalog",
    "Description",
    "MethodInfo",
    "ErrorCode",
];

pub fn splice(doc: &str, catalog: &Catalog) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = doc;
    while let Some(at) = mark(rest, BEGIN) {
        if mark(&rest[..at], END).is_some() {
            return Err(format!("{END} without a mark"));
        }
        let line_end = rest[at..]
            .find('\n')
            .map(|i| at + i + 1)
            .ok_or("a generated mark ends the file")?;
        let what = rest[at + BEGIN.len()..line_end]
            .trim_end()
            .strip_suffix("-->")
            .ok_or_else(|| format!("unclosed mark {:?}", &rest[at..line_end]))?
            .trim();
        let end = mark(&rest[line_end..], END)
            .map(|i| line_end + i)
            .ok_or_else(|| format!("{what}: no {END}"))?;
        if mark(&rest[line_end..end], BEGIN).is_some() {
            return Err(format!("{what}: a mark inside the region"));
        }
        out.push_str(&rest[..line_end]);
        out.push_str(&render(what, catalog)?);
        out.push_str(END);
        rest = &rest[end + END.len()..];
    }
    if mark(rest, END).is_some() {
        return Err(format!("{END} without a mark"));
    }
    out.push_str(rest);
    Ok(out)
}

fn mark(text: &str, mark: &str) -> Option<usize> {
    text.match_indices(mark)
        .map(|(at, _)| at)
        .find(|&at| at == 0 || text[..at].ends_with('\n'))
}

fn render(what: &str, catalog: &Catalog) -> Result<String, String> {
    let defs = &catalog.defs;
    Ok(match what.split_once(' ') {
        Some(("type", name)) => {
            let schema = defs
                .get(name)
                .ok_or_else(|| format!("no definition {name}"))?;
            fields_table(schema, defs)
        }
        _ => match what {
            "frames" => table(
                &["Type", "Direction", "Channel", "Payload", "Name"],
                catalog.frames.iter().map(|f| {
                    vec![
                        format!("`0x{:02X}`", f.frame_type),
                        f.direction.to_string(),
                        f.channel.clone(),
                        match f.payload {
                            Payload::Json => "JSON".into(),
                            other => other.to_string(),
                        },
                        format!("`{}`", f.name),
                    ]
                }),
            ),
            "methods" => table(
                &["Method", "Class", "Params", "Result", "Errors"],
                catalog.methods.iter().map(|m| {
                    let params = field_names(resolve(&m.params, defs));
                    vec![
                        format!("`{}`", m.name),
                        m.class.to_string(),
                        if params.is_empty() {
                            String::new()
                        } else {
                            format!("`{params}`")
                        },
                        code(&shape(&m.result, defs)),
                        codes(&m.errors),
                    ]
                }),
            ),
            "events" => table(
                &["Event", "Fields", "When"],
                catalog.events.iter().map(|e| {
                    let mut fields = e.schema.clone();
                    if let Some(Value::Object(properties)) = fields.get_mut("properties") {
                        properties.shift_remove("event");
                    }
                    vec![
                        format!("`{}`", e.name),
                        format!("`{}`", field_names(&fields)),
                        one_line(&e.summary),
                    ]
                }),
            ),
            "errors" => table(
                &["Code", "Meaning"],
                catalog
                    .errors
                    .iter()
                    .map(|e| vec![format!("`{}`", e.code), one_line(&e.summary)]),
            ),
            _ => return Err(format!("nothing renders {what:?}")),
        },
    })
}

pub fn method(description: &Description) -> String {
    let m = &description.method;
    let defs = &description.defs;
    let mut out = format!("# {}\n\n{} ({})\n\n", m.name, m.summary, m.class);
    out.push_str("## Params\n\n");
    let params = resolve(&m.params, defs);
    out.push_str(&about(params));
    out.push_str(&fields_table(params, defs));
    let _ = write!(out, "\n## Result: {}\n\n", code(&shape(&m.result, defs)));
    let result = resolve(&m.result, defs);
    if result.get("properties").is_some() {
        out.push_str(&about(result));
        out.push_str(&fields_table(result, defs));
        out.push('\n');
    }
    let mut seen = BTreeSet::new();
    let mut queue = VecDeque::new();
    for top in [&m.params, &m.result] {
        seen.extend(ref_name(top).map(String::from));
        refs(resolve(top, defs), &mut queue);
    }
    while let Some(name) = queue.pop_front() {
        let Some(schema) = defs.get(&name) else {
            continue;
        };
        if !seen.insert(name.clone()) {
            continue;
        }
        if schema.get("properties").is_some() {
            let _ = write!(out, "## {name}\n\n");
            out.push_str(&about(schema));
            out.push_str(&fields_table(schema, defs));
            out.push('\n');
        }
        refs(schema, &mut queue);
    }
    let _ = writeln!(out, "## Errors\n\n{}", codes(&m.errors));
    for example in &m.examples {
        let _ = writeln!(
            out,
            "\n## Example\n\n```json\n{{\"method\": {:?}, \"params\": {}}}\n{{\"result\": {}}}\n```",
            m.name,
            serde_json::to_string(&example.params).expect("json"),
            serde_json::to_string(&example.result).expect("json"),
        );
    }
    out
}

fn codes(errors: &[ErrorCode]) -> String {
    errors
        .iter()
        .map(|e| format!("`{e}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn code(shape: &str) -> String {
    if shape.split(" | ").all(|part| NAMED.contains(&part)) {
        shape.to_string()
    } else {
        format!("`{shape}`")
    }
}

pub(crate) fn ref_name(schema: &Value) -> Option<&str> {
    schema.get("$ref")?.as_str()?.strip_prefix("#/$defs/")
}

fn resolve<'a>(schema: &'a Value, defs: &'a Map<String, Value>) -> &'a Value {
    ref_name(schema)
        .and_then(|name| defs.get(name))
        .unwrap_or(schema)
}

fn about(schema: &Value) -> String {
    schema
        .get("description")
        .and_then(Value::as_str)
        .map(|text| format!("{}\n\n", one_line(text)))
        .unwrap_or_default()
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn required(schema: &Value) -> BTreeSet<&str> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn field_names(schema: &Value) -> String {
    let required = required(schema);
    schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| {
            properties
                .keys()
                .map(|name| {
                    let mark = if required.contains(name.as_str()) {
                        ""
                    } else {
                        "?"
                    };
                    format!("{name}{mark}")
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

fn shape(schema: &Value, defs: &Map<String, Value>) -> String {
    if let Some(name) = ref_name(schema) {
        if NAMED.contains(&name) {
            return name.to_string();
        }
        return match defs.get(name) {
            Some(def) => shape(def, defs),
            None => name.to_string(),
        };
    }
    let Value::Object(map) = schema else {
        return "any".into();
    };
    if let Some(value) = map.get("const") {
        return format!(
            "`{}`",
            value.as_str().map_or(value.to_string(), String::from)
        );
    }
    if let Some(Value::Array(values)) = map.get("enum") {
        return values
            .iter()
            .map(|v| format!("`{}`", v.as_str().map_or(v.to_string(), String::from)))
            .collect::<Vec<_>>()
            .join(" | ");
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(Value::Array(branches)) = map.get(key) {
            return branches
                .iter()
                .map(|b| shape(b, defs))
                .collect::<Vec<_>>()
                .join(" | ");
        }
    }
    match map.get("type").and_then(Value::as_str) {
        Some("integer") => "int".into(),
        Some("number") => "number".into(),
        Some("string") => "string".into(),
        Some("boolean") => "bool".into(),
        Some("array") => {
            let items = map.get("items").map_or("any".into(), |i| shape(i, defs));
            if items.contains(" | ") {
                format!("({items})[]")
            } else {
                format!("{items}[]")
            }
        }
        Some("object") if map.contains_key("properties") => {
            format!("{{{}}}", field_names(schema))
        }
        Some("object") if map.get("additionalProperties").is_some_and(|a| a != false) => {
            "object".into()
        }
        Some("object") => "{}".into(),
        _ => "any".into(),
    }
}

fn fields_table(schema: &Value, defs: &Map<String, Value>) -> String {
    let required = required(schema);
    let rows = schema
        .get("properties")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(name, property)| {
            let mut shape = shape(property, defs);
            if !required.contains(name.as_str()) {
                if shape.contains(" | ") {
                    shape = format!("({shape})");
                }
                shape.push('?');
            }
            vec![
                format!("`{name}`"),
                shape,
                property
                    .get("description")
                    .and_then(Value::as_str)
                    .map(one_line)
                    .unwrap_or_default(),
            ]
        });
    table(&["Field", "Type", "Meaning"], rows)
}

fn table(head: &[&str], rows: impl Iterator<Item = Vec<String>>) -> String {
    let mut rows: Vec<Vec<String>> = rows
        .map(|row| {
            row.into_iter()
                .map(|cell| cell.replace('|', "\\|"))
                .collect()
        })
        .collect();
    rows.insert(0, head.iter().map(ToString::to_string).collect());
    let last = head.len() - 1;
    let widths: Vec<usize> = (0..head.len())
        .map(|i| match i {
            i if i == last => head[i].len(),
            i => rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0),
        })
        .collect();
    let line = |cells: Vec<String>| {
        let padded: Vec<String> = cells
            .iter()
            .zip(&widths)
            .enumerate()
            .map(|(i, (cell, width))| match i {
                i if i == last => cell.clone(),
                _ => format!("{cell:<width$}"),
            })
            .collect();
        let mut text = format!("| {}", padded.join(" | "));
        text.push_str(if text.ends_with(' ') { "|\n" } else { " |\n" });
        text
    };
    let mut out = String::new();
    let mut rows = rows.into_iter();
    out.push_str(&line(rows.next().expect("a head")));
    out.push_str(&line(widths.iter().map(|w| "-".repeat(*w)).collect()));
    for row in rows {
        out.push_str(&line(row));
    }
    out
}
