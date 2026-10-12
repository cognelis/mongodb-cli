use std::borrow::Cow;
use std::collections::HashSet;
use std::io::{self, Write};

use console::{Term, measure_text_width, truncate_str};
use serde_json::{Map, Value, json};

pub(super) fn width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .or_else(|| {
            Term::stdout()
                .size_checked()
                .map(|(_, width)| usize::from(width))
        })
        .unwrap_or(100)
        .clamp(8, 512)
}

/// Return whether any displayed text was shortened; the JSON payload is never modified.
pub(super) fn write(
    output: &mut impl Write,
    command: &str,
    data: &Value,
    width: usize,
    current_profile: Option<&str>,
) -> io::Result<bool> {
    let mut renderer = Renderer {
        output,
        command,
        width: width.max(8),
        shortened: false,
    };
    if command == "profile.list"
        && let Some(profiles) = data.as_object()
    {
        renderer.profiles(profiles, current_profile)?;
    } else if command == "config.show"
        && let Some(profiles) = data.get("profiles").and_then(Value::as_object)
    {
        let mut details = data
            .as_object()
            .expect("profiles belongs to an object")
            .clone();
        details.remove("profiles");
        renderer.value(&Value::Object(details), 0)?;
        renderer.heading("Profiles", 0)?;
        renderer.profiles(
            profiles,
            data.get("current_profile").and_then(Value::as_str),
        )?;
    } else {
        renderer.value(data, 0)?;
    }
    Ok(renderer.shortened)
}

struct Renderer<'a, W> {
    output: &'a mut W,
    command: &'a str,
    width: usize,
    shortened: bool,
}

impl<W: Write> Renderer<'_, W> {
    fn profiles(&mut self, profiles: &Map<String, Value>, current: Option<&str>) -> io::Result<()> {
        let rows: Vec<Value> = profiles
            .iter()
            .map(|(name, profile)| {
                let mut row = json!({
                    "current": if current == Some(name) { "*" } else { "" },
                    "name": name,
                    "access": profile.get("access").unwrap_or(&Value::Null),
                    "target": profile.get("target").unwrap_or(&Value::Null),
                });
                // The column appears only once some profile is described.
                if let Some(description) = profile.get("description") {
                    row["description"] = description.clone();
                }
                row
            })
            .collect();
        self.array(&rows, 0)
    }

    fn value(&mut self, value: &Value, indent: usize) -> io::Result<()> {
        if let Some(text) = ejson_scalar(value) {
            return self.line(&safe_text(&text), indent);
        }
        match value {
            Value::Array(rows) => self.array(rows, indent),
            Value::Object(object) => self.details(object, indent),
            Value::Null => self.line("(no data)", indent),
            scalar => self.line(&cell(scalar), indent),
        }
    }

    fn details(&mut self, object: &Map<String, Value>, indent: usize) -> io::Result<()> {
        if object.is_empty() {
            return self.line("(empty object)", indent);
        }
        let keys = ordered_keys(self.command, object.keys().map(String::as_str));
        let available = self.available(indent);
        let label_width = keys
            .iter()
            .filter(|key| !is_nested(&object[**key]))
            .map(|key| measure_text_width(&safe_text(key)))
            .max()
            .unwrap_or(1)
            .min((available / 3).max(1))
            .min(30);
        for key in &keys {
            let value = &object[*key];
            if is_nested(value) {
                continue;
            }
            let label = self.fit(&safe_text(key), label_width);
            let value = self.fit(
                &cell(value),
                available.saturating_sub(label_width + 2).max(1),
            );
            let padding = " ".repeat(label_width.saturating_sub(measure_text_width(&label)));
            self.line(&format!("{label}{padding}: {value}"), indent)?;
        }
        for key in keys {
            let value = &object[key];
            if is_nested(value) {
                self.heading(key, indent)?;
                self.value(value, self.child_indent(indent))?;
            }
        }
        Ok(())
    }

    fn array(&mut self, rows: &[Value], indent: usize) -> io::Result<()> {
        if rows.is_empty() {
            return self.line("(no rows)", indent);
        }
        // Rows with nested collections, such as documents with embedded arrays, get a section each.
        if rows.iter().any(contains_collection) {
            return self.records(rows, indent);
        }
        let columns = ordered_keys(
            self.command,
            rows.iter().flat_map(|row| match row.as_object() {
                Some(object) if ejson_scalar(row).is_none() => {
                    object.keys().map(String::as_str).collect::<Vec<_>>()
                }
                _ => vec!["value"],
            }),
        );
        let cells: Vec<Vec<String>> = rows
            .iter()
            .map(|row| {
                columns
                    .iter()
                    .map(|key| match row.as_object() {
                        Some(object) if ejson_scalar(row).is_none() => {
                            object.get(*key).map(cell).unwrap_or_else(|| "—".into())
                        }
                        _ if *key == "value" => cell(row),
                        _ => "—".into(),
                    })
                    .collect()
            })
            .collect();
        let headers: Vec<_> = columns.iter().map(|key| safe_text(key)).collect();
        let mut widths: Vec<_> = headers
            .iter()
            .enumerate()
            .map(|(index, header)| {
                cells
                    .iter()
                    .map(|row| measure_text_width(&row[index]))
                    .chain(std::iter::once(measure_text_width(header)))
                    .max()
                    .unwrap_or(1)
                    .clamp(1, self.available(indent))
            })
            .collect();
        let minimums: Vec<_> = widths.iter().map(|width| (*width).min(8)).collect();
        let spacing = 2 * columns.len().saturating_sub(1);
        let available = self.available(indent);
        if minimums.iter().sum::<usize>() + spacing > available {
            // Keep every field visible, including fields that appear only in later records.
            return self.records(rows, indent);
        }
        while widths.iter().sum::<usize>() + spacing > available {
            let index = (0..widths.len())
                .filter(|index| widths[*index] > minimums[*index])
                .max_by_key(|index| widths[*index])
                .expect("minimum widths fit");
            widths[index] -= 1;
        }
        self.table_line(&headers, &widths, indent)?;
        self.table_line(
            &widths
                .iter()
                .map(|width| "-".repeat(*width))
                .collect::<Vec<_>>(),
            &widths,
            indent,
        )?;
        for row in cells {
            self.table_line(&row, &widths, indent)?;
        }
        Ok(())
    }

    fn records(&mut self, rows: &[Value], indent: usize) -> io::Result<()> {
        for (index, row) in rows.iter().enumerate() {
            self.heading(&format!("[{}]", index + 1), indent)?;
            self.value(row, self.child_indent(indent))?;
        }
        Ok(())
    }

    fn table_line(&mut self, cells: &[String], widths: &[usize], indent: usize) -> io::Result<()> {
        let mut line = String::new();
        for (index, (value, width)) in cells.iter().zip(widths).enumerate() {
            let value = self.fit(value, *width);
            line.push_str(&value);
            if index + 1 < cells.len() {
                line.push_str(&" ".repeat(width.saturating_sub(measure_text_width(&value)) + 2));
            }
        }
        self.line(&line, indent)
    }

    fn heading(&mut self, text: &str, indent: usize) -> io::Result<()> {
        self.line(&format!("{}:", safe_text(text)), indent)
    }

    fn fit(&mut self, value: &str, width: usize) -> String {
        let text = truncate_str(value, width.max(1), "…");
        self.shortened |= matches!(text, Cow::Owned(_));
        text.into_owned()
    }

    fn line(&mut self, text: &str, indent: usize) -> io::Result<()> {
        let indent = indent.min(self.width.saturating_sub(4));
        let text = self.fit(text, self.available(indent));
        writeln!(self.output, "{}{text}", " ".repeat(indent))
    }

    fn available(&self, indent: usize) -> usize {
        self.width.saturating_sub(indent).max(1)
    }

    fn child_indent(&self, indent: usize) -> usize {
        (indent + 2).min(self.width.saturating_sub(4))
    }
}

/// Display a relaxed Extended JSON wrapper such as `{"$oid": "…"}` as one value.
fn ejson_scalar(value: &Value) -> Option<String> {
    let object = value.as_object()?;
    if object.len() != 1 {
        return None;
    }
    let (key, inner) = object.iter().next()?;
    match (key.as_str(), inner) {
        (
            "$oid" | "$date" | "$numberDecimal" | "$numberLong" | "$numberInt" | "$numberDouble"
            | "$symbol" | "$uuid",
            Value::String(text),
        ) => Some(text.clone()),
        ("$date", Value::Object(_)) => Some(format!("Date({})", inner["$numberLong"].as_str()?)),
        ("$timestamp", Value::Object(timestamp)) => Some(format!(
            "Timestamp({}, {})",
            timestamp.get("t")?,
            timestamp.get("i")?
        )),
        ("$regularExpression", Value::Object(regex)) => Some(format!(
            "/{}/{}",
            regex.get("pattern")?.as_str()?,
            regex.get("options")?.as_str()?
        )),
        ("$binary", Value::Object(binary)) => Some(format!(
            "Binary({}, subtype {})",
            binary.get("base64")?.as_str()?,
            binary.get("subType")?.as_str()?
        )),
        ("$minKey" | "$maxKey", _) => Some(key.trim_start_matches('$').into()),
        ("$undefined", _) => Some("undefined".into()),
        _ => None,
    }
}

fn is_nested(value: &Value) -> bool {
    value.is_array() || (value.is_object() && ejson_scalar(value).is_none())
}

fn contains_collection(row: &Value) -> bool {
    row.as_object().is_some_and(|object| {
        ejson_scalar(row).is_none()
            && object.values().any(|value| {
                value
                    .as_array()
                    .is_some_and(|items| items.iter().any(is_nested))
            })
    })
}

/// Preferred columns first, then every other key in first-seen order.
fn ordered_keys<'a>(command: &str, keys: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let preferred: &[&str] = match command {
        "doctor" => &["name", "status", "message"],
        "profile.list" | "config.show" => &[
            "current",
            "name",
            "access",
            "description",
            "target",
            "current_profile",
        ],
        "skills.install" => &["agent", "skill", "status", "path"],
        "databases.list" => &["name", "sizeOnDisk", "empty"],
        "collections.list" => &["name", "type"],
        "collections.schema" => &["path", "count", "types"],
        "indexes.list" => &["name", "key"],
        "server.logs" => &["t", "s", "c", "id", "ctx", "msg"],
        _ => &["_id", "name"],
    };
    let mut seen = HashSet::new();
    let unique: Vec<&str> = keys.filter(|key| seen.insert(*key)).collect();
    let mut ordered: Vec<&str> = preferred
        .iter()
        .filter_map(|preferred| unique.iter().copied().find(|key| key == preferred))
        .collect();
    ordered.extend(unique.into_iter().filter(|key| !preferred.contains(key)));
    ordered
}

fn cell(value: &Value) -> String {
    if let Some(text) = ejson_scalar(value) {
        return safe_text(&text);
    }
    match value {
        Value::String(value) => safe_text(value),
        value => safe_text(&value.to_string()),
    }
}

pub(super) fn safe_text(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            character
                if character.is_control()
                    || matches!(character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') =>
            {
                result.extend(character.escape_unicode());
            }
            character => result.push(character),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(command: &str, data: &Value, width: usize) -> (String, bool) {
        let mut output = Vec::new();
        let shortened = write(&mut output, command, data, width, Some("prod")).unwrap();
        (String::from_utf8(output).unwrap(), shortened)
    }

    #[test]
    fn documents_render_extended_json_scalars_in_first_seen_columns() {
        let data = json!([
            {"name": "a", "_id": {"$oid": "65f1a2b3c4d5e6f708192a3b"}, "at": {"$date": "2026-10-10T00:00:00Z"}},
            {"_id": {"$oid": "65f1a2b3c4d5e6f708192a3c"}, "name": "b", "extra": {"$numberDecimal": "1.50"}}
        ]);
        let before = data.clone();
        let (output, shortened) = render("documents.find", &data, 140);
        let lines: Vec<_> = output.lines().collect();
        let header: Vec<_> = lines[0].split_whitespace().collect();
        assert_eq!(header, ["_id", "name", "at", "extra"]);
        assert!(lines[2].contains("65f1a2b3c4d5e6f708192a3b"));
        assert!(lines[2].contains("2026-10-10T00:00:00Z"));
        assert!(lines[3].contains("1.50"));
        assert!(lines[3].contains('—'), "missing fields use a placeholder");
        assert!(!output.contains("$oid"));
        assert!(!shortened);
        assert_eq!(data, before);
    }

    #[test]
    fn documents_with_embedded_document_arrays_render_one_section_each() {
        let data = json!([
            {"_id": 1, "items": [{"sku": "s1"}, {"sku": "s2"}]},
            {"_id": 2, "items": []}
        ]);
        let (output, _) = render("documents.find", &data, 100);
        assert!(output.contains("[1]:"));
        assert!(output.contains("[2]:"));
        assert!(output.contains("items:"));
        assert!(output.contains("s2"));
    }

    #[test]
    fn profile_lists_mark_the_default_and_show_access_and_target() {
        let data = json!({
            "prod": {"target": "mongodb://prod.example.com", "description": "Orders", "access": "read-only"},
            "test": {"access": "read-write"}
        });
        let (output, _) = render("profile.list", &data, 120);
        let lines: Vec<_> = output.lines().collect();
        assert_eq!(
            lines[0].split_whitespace().collect::<Vec<_>>(),
            ["current", "name", "access", "description", "target"]
        );
        assert!(lines[2].starts_with('*'));
        assert!(lines[2].contains("read-only"));
        assert!(lines[2].contains("Orders"));
        assert!(lines[2].contains("mongodb://prod.example.com"));
        assert!(lines[3].contains("read-write"));
    }

    #[test]
    fn skill_install_results_show_agent_skill_status_then_path() {
        let data = json!([{
            "agent": "codex",
            "skill": "mongodb-cli",
            "path": "/home/tester/.agents/skills/mongodb-cli",
            "status": "installed"
        }]);
        let (output, _) = render("skills.install", &data, 120);
        assert_eq!(
            output
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .collect::<Vec<_>>(),
            ["agent", "skill", "status", "path"]
        );
    }

    #[test]
    fn narrow_terminals_switch_to_record_details_and_report_shortening() {
        let data = json!([{"_id": 1, "description": "a long value that cannot fit in a narrow terminal", "other": "x"}]);
        let (output, shortened) = render("documents.find", &data, 15);
        assert!(output.contains("[1]:"));
        assert!(shortened);
        assert!(output.lines().all(|line| measure_text_width(line) <= 15));
    }

    #[test]
    fn wide_characters_and_control_characters_are_aligned_and_escaped() {
        let data =
            json!([{"name": "客户", "note": "line\nnext"}, {"name": "ab", "note": "\u{202e}x"}]);
        let (output, _) = render("documents.find", &data, 80);
        let lines: Vec<_> = output.lines().collect();
        assert_eq!(
            measure_text_width(lines[2].split("line").next().unwrap()),
            measure_text_width(lines[3].split("\\u{202e}").next().unwrap())
        );
        assert!(output.contains("line\\nnext"));
        assert!(!output.contains('\u{202e}'));
    }

    #[test]
    fn ejson_scalars_cover_common_wrappers() {
        assert_eq!(
            ejson_scalar(&json!({"$timestamp": {"t": 1, "i": 2}})).as_deref(),
            Some("Timestamp(1, 2)")
        );
        assert_eq!(
            ejson_scalar(&json!({"$regularExpression": {"pattern": "^a", "options": "i"}}))
                .as_deref(),
            Some("/^a/i")
        );
        assert_eq!(
            ejson_scalar(&json!({"$date": {"$numberLong": "-1"}})).as_deref(),
            Some("Date(-1)")
        );
        assert_eq!(ejson_scalar(&json!({"city": "x"})), None);
        assert_eq!(ejson_scalar(&json!({"$oid": "a", "extra": 1})), None);
    }

    #[test]
    fn scalars_and_empty_results_have_explicit_placeholders() {
        assert_eq!(render("documents.find", &json!([]), 80).0, "(no rows)\n");
        assert_eq!(
            render("documents.count", &json!({}), 80).0,
            "(empty object)\n"
        );
        assert_eq!(
            render("documents.count", &json!({"count": 3}), 80).0,
            "count: 3\n"
        );
    }
}
