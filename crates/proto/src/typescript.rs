use std::fmt::Write as _;

use serde_json::Value;

use crate::catalog::Catalog;
use crate::render::{ref_name, required};

pub fn module(catalog: &Catalog) -> String {
    let mut out =
        String::from("// Generated from the catalog by `cargo xtask protocol`; do not edit.\n\n");
    let _ = write!(out, "export const VERSION = {};\n\n", catalog.protocol);
    out.push_str("/** Each method's params and result. */\nexport interface Methods {\n");
    for m in &catalog.methods {
        out.push_str(&doc(&m.summary, "  "));
        let _ = writeln!(
            out,
            "  {}: {{ params: {}; result: {} }};",
            m.name,
            ty(&m.params, "  "),
            ty(&m.result, "  ")
        );
    }
    out.push_str("}\n\nexport type Method = keyof Methods;\n");
    for (name, schema) in &catalog.defs {
        out.push('\n');
        out.push_str(&doc(description(schema), ""));
        if is_interface(schema) {
            let _ = writeln!(out, "export interface {name} {}", fields(schema, "", true));
        } else {
            let ty = ty(schema, "");
            let sep = if ty.starts_with('\n') { "" } else { " " };
            let _ = writeln!(out, "export type {name} ={sep}{ty};");
        }
    }
    out
}

fn description(schema: &Value) -> &str {
    schema
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn is_interface(schema: &Value) -> bool {
    schema.get("properties").is_some()
        && schema.get("oneOf").is_none()
        && schema.get("anyOf").is_none()
}

fn doc(text: &str, indent: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let one = format!("{indent}/** {text} */\n");
    if !text.contains('\n') && one.trim_end().len() <= 80 {
        return one;
    }
    let mut out = format!("{indent}/**\n");
    for (i, paragraph) in text.split("\n\n").enumerate() {
        if i > 0 {
            let _ = writeln!(out, "{indent} *");
        }
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            if !line.is_empty() && indent.len() + 3 + line.len() + 1 + word.len() > 80 {
                let _ = writeln!(out, "{indent} * {line}");
                line.clear();
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        let _ = writeln!(out, "{indent} * {line}");
    }
    let _ = writeln!(out, "{indent} */");
    out
}

fn fields(schema: &Value, indent: &str, block: bool) -> String {
    let required = required(schema);
    let properties: Vec<(&String, &Value)> = schema
        .get("properties")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .collect();
    let inner = format!("{indent}  ");
    let field = |&(name, property): &(&String, &Value)| {
        let mark = if required.contains(name.as_str()) {
            ""
        } else {
            "?"
        };
        format!("{name}{mark}: {}", ty(property, &inner))
    };
    if !block && properties.iter().all(|(_, p)| description(p).is_empty()) {
        let fields: Vec<String> = properties.iter().map(field).collect();
        return format!("{{ {} }}", fields.join("; "));
    }
    let mut out = String::from("{\n");
    for entry in &properties {
        out.push_str(&doc(description(entry.1), &inner));
        let _ = writeln!(out, "{inner}{};", field(entry));
    }
    let _ = write!(out, "{indent}}}");
    out
}

fn ty(schema: &Value, indent: &str) -> String {
    let Value::Object(map) = schema else {
        return "unknown".into();
    };
    if let Some(name) = ref_name(schema) {
        return name.to_string();
    }
    if let Some(value) = map.get("const") {
        return value.to_string();
    }
    if let Some(Value::Array(values)) = map.get("enum") {
        let literals: Vec<String> = values.iter().map(Value::to_string).collect();
        return literals.join(" | ");
    }
    if let Some(branches) = ["oneOf", "anyOf"]
        .iter()
        .find_map(|key| map.get(*key).and_then(Value::as_array))
    {
        let union = union(branches, indent);
        if !map.contains_key("properties") {
            return union;
        }
        let close = if union.starts_with('\n') {
            format!("\n{indent}")
        } else {
            String::new()
        };
        return format!("{} & ({union}{close})", fields(schema, indent, false));
    }
    match map.get("type").and_then(Value::as_str) {
        Some("string") => "string".into(),
        Some("integer" | "number") => "number".into(),
        Some("boolean") => "boolean".into(),
        Some("array") => {
            let items = map.get("items").map_or("unknown".into(), |i| ty(i, indent));
            if items.contains(['|', '&']) {
                format!("({items})[]")
            } else {
                format!("{items}[]")
            }
        }
        Some("object") if map.contains_key("properties") => fields(schema, indent, false),
        Some("object") => match map.get("additionalProperties") {
            Some(values @ Value::Object(_)) => format!("Record<string, {}>", ty(values, indent)),
            _ => "Record<string, unknown>".into(),
        },
        _ => "unknown".into(),
    }
}

fn union(branches: &[Value], indent: &str) -> String {
    let inner = format!("{indent}  ");
    if branches.iter().all(|b| description(b).is_empty()) {
        let types: Vec<String> = branches.iter().map(|b| ty(b, indent)).collect();
        return types.join(" | ");
    }
    let mut out = String::new();
    for branch in branches {
        out.push('\n');
        out.push_str(&doc(description(branch), &inner));
        let _ = write!(out, "{inner}| {}", ty(branch, &inner));
    }
    out
}
