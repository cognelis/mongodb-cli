//! Pipeline checks that run before an operation reaches the server.

use mongodb::bson::{Bson, Document};

use crate::{ClientError, ErrorCode};

/// Operators that run JavaScript on the server; refused unless the connection allows them.
pub const SERVER_SIDE_JS: &[&str] = &["$where", "$function", "$accumulator"];
/// Nesting deeper than this is not scanned further; MongoDB itself limits documents to 100 levels.
const MAX_SCAN_DEPTH: usize = 128;

/// Return the first server-side JavaScript operator found at any depth, as the MCP server does.
pub fn server_side_js(value: &Bson) -> Option<&'static str> {
    fn scan(value: &Bson, depth: usize) -> Option<&'static str> {
        if depth > MAX_SCAN_DEPTH {
            return None;
        }
        match value {
            Bson::Document(document) => document.iter().find_map(|(key, nested)| {
                SERVER_SIDE_JS
                    .iter()
                    .copied()
                    .find(|operator| key == operator)
                    .or_else(|| scan(nested, depth + 1))
            }),
            Bson::Array(items) => items.iter().find_map(|item| scan(item, depth + 1)),
            _ => None,
        }
    }
    scan(value, 0)
}

/// Describe what each `$out`/`$merge` stage writes, qualifying bare collection names with `database`.
pub fn write_targets(pipeline: &[Document], database: &str) -> Vec<String> {
    let namespace = |target: Option<&Bson>| match target {
        Some(Bson::String(collection)) => format!("{database}.{collection}"),
        Some(Bson::Document(target)) => match (target.get_str("db"), target.get_str("coll")) {
            (Ok(db), Ok(collection)) => format!("{db}.{collection}"),
            (Err(_), Ok(collection)) => format!("{database}.{collection}"),
            _ => "its target collection".into(),
        },
        _ => "its target collection".into(),
    };
    pipeline
        .iter()
        .filter_map(|stage| {
            if let Some(target) = stage.get("$out") {
                return Some(format!(
                    "$out replaces every document in {}",
                    namespace(Some(target))
                ));
            }
            let merge = stage.get("$merge")?;
            let (into, modes) = match merge {
                Bson::Document(spec) => {
                    let modes: Vec<String> = ["whenMatched", "whenNotMatched"]
                        .into_iter()
                        .filter_map(|key| Some(format!("{key}: {}", spec.get_str(key).ok()?)))
                        .collect();
                    (spec.get("into"), modes)
                }
                other => (Some(other), Vec::new()),
            };
            let mut description = format!("$merge writes into {}", namespace(into));
            if !modes.is_empty() {
                description.push_str(&format!(" ({})", modes.join(", ")));
            }
            Some(description)
        })
        .collect()
}

/// First stages of a database-level aggregation that finish on their own.
pub const DATABASE_STAGES: &[&str] = &[
    "$currentOp",
    "$documents",
    "$listLocalSessions",
    "$queryStats",
];

/// Return the first write stage (`$out` or `$merge`) of a pipeline, if any.
///
/// MongoDB accepts these stages only at the top level, so sub-pipelines need no scan.
pub fn write_stage(pipeline: &[Document]) -> Option<&'static str> {
    pipeline.iter().find_map(|stage| {
        if stage.contains_key("$out") {
            Some("$out")
        } else if stage.contains_key("$merge") {
            Some("$merge")
        } else {
            None
        }
    })
}

pub fn validate_database_pipeline(pipeline: &[Document]) -> Result<(), ClientError> {
    let Some(first) = pipeline.first() else {
        return Err(ClientError::new(
            ErrorCode::Validation,
            "a database aggregation needs at least one stage",
        ));
    };
    if first.contains_key("$changeStream") {
        return Err(ClientError::new(
            ErrorCode::Validation,
            "$changeStream cursors never finish, so database aggregations cannot start with them",
        )
        .with_hint(format!("start with one of {}", DATABASE_STAGES.join(", "))));
    }
    if DATABASE_STAGES
        .iter()
        .any(|stage| first.contains_key(stage))
    {
        Ok(())
    } else {
        Err(ClientError::new(
            ErrorCode::Validation,
            format!(
                "the first stage of a database aggregation must be one of {}",
                DATABASE_STAGES.join(", ")
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use mongodb::bson::doc;

    use super::*;

    #[test]
    fn detects_top_level_write_stages() {
        assert_eq!(write_stage(&[doc! {"$match": {}}]), None);
        assert_eq!(
            write_stage(&[doc! {"$match": {}}, doc! {"$out": "copy"}]),
            Some("$out")
        );
        assert_eq!(
            write_stage(&[doc! {"$merge": {"into": "copy"}}]),
            Some("$merge")
        );
        assert_eq!(
            write_stage(&[doc! {"$facet": {"a": [{"$match": {"$out": 1}}]}}]),
            None
        );
    }

    #[test]
    fn finds_server_side_javascript_at_any_depth() {
        use mongodb::bson::Bson;
        for (value, expected) in [
            (doc! {"$where": "this.a > 1"}, Some("$where")),
            (doc! {"$or": [{"a": 1}, {"$where": "true"}]}, Some("$where")),
            (
                doc! {"$group": {"_id": null, "x": {"$accumulator": {}}}},
                Some("$accumulator"),
            ),
            (
                doc! {"$expr": {"$function": {"body": "f", "args": [], "lang": "js"}}},
                Some("$function"),
            ),
            (doc! {"where": "$where", "a": {"$gt": 1}}, None),
        ] {
            assert_eq!(
                server_side_js(&Bson::Document(value.clone())),
                expected,
                "{value}"
            );
        }
        let pipeline = Bson::Array(vec![
            Bson::Document(doc! {"$match": {}}),
            Bson::Document(doc! {"$project": {"y": {"$function": {}}}}),
        ]);
        assert_eq!(server_side_js(&pipeline), Some("$function"));
    }

    #[test]
    fn describes_write_targets_like_the_mcp_server() {
        assert_eq!(
            write_targets(&[doc! {"$out": "copy"}], "shop"),
            ["$out replaces every document in shop.copy"]
        );
        assert_eq!(
            write_targets(
                &[doc! {"$merge": {"into": {"db": "other", "coll": "c"}, "whenMatched": "merge"}}],
                "shop"
            ),
            ["$merge writes into other.c (whenMatched: merge)"]
        );
        assert_eq!(
            write_targets(&[doc! {"$out": {"s3": {}}}], "shop"),
            ["$out replaces every document in its target collection"]
        );
        assert!(write_targets(&[doc! {"$match": {}}], "shop").is_empty());
    }

    #[test]
    fn database_pipelines_need_a_finite_database_stage() {
        assert!(validate_database_pipeline(&[doc! {"$currentOp": {}}]).is_ok());
        assert!(validate_database_pipeline(&[doc! {"$documents": [{"a": 1}]}]).is_ok());
        for pipeline in [
            vec![],
            vec![doc! {"$match": {}}],
            vec![doc! {"$changeStream": {}}],
        ] {
            assert_eq!(
                validate_database_pipeline(&pipeline).unwrap_err().code(),
                ErrorCode::Validation
            );
        }
    }
}
