//! Extended JSON input parsing and relaxed Extended JSON output.

use mongodb::bson::{Bson, Document};
use serde_json::Value;

use crate::{ClientError, ErrorCode};

/// Parse one Extended JSON value supplied on the command line or in a file.
pub fn parse_value(text: &str, label: &str) -> Result<Bson, ClientError> {
    let value: Value = serde_json::from_str(text)
        .map_err(|error| invalid(label, format!("invalid JSON: {error}")))?;
    Bson::try_from(value).map_err(|error| invalid(label, format!("invalid Extended JSON: {error}")))
}

pub fn parse_document(text: &str, label: &str) -> Result<Document, ClientError> {
    match parse_value(text, label)? {
        Bson::Document(document) => Ok(document),
        _ => Err(invalid(label, "expected a JSON object".into())),
    }
}

/// Parse a JSON array of objects, as used by aggregation pipelines.
pub fn parse_document_array(text: &str, label: &str) -> Result<Vec<Document>, ClientError> {
    match parse_value(text, label)? {
        Bson::Array(items) => documents(items, label),
        _ => Err(invalid(label, "expected a JSON array of objects".into())),
    }
}

/// Parse either one JSON object or an array of objects, as used for inserts.
pub fn parse_documents(text: &str, label: &str) -> Result<Vec<Document>, ClientError> {
    match parse_value(text, label)? {
        Bson::Document(document) => Ok(vec![document]),
        Bson::Array(items) => documents(items, label),
        _ => Err(invalid(
            label,
            "expected a JSON object or an array of objects".into(),
        )),
    }
}

/// Convert BSON to relaxed Extended JSON, the format used by mongosh and the MongoDB MCP server.
pub fn to_json(value: Bson) -> Value {
    value.into_relaxed_extjson()
}

pub fn document_to_json(document: Document) -> Value {
    to_json(Bson::Document(document))
}

fn documents(items: Vec<Bson>, label: &str) -> Result<Vec<Document>, ClientError> {
    items
        .into_iter()
        .enumerate()
        .map(|(index, item)| match item {
            Bson::Document(document) => Ok(document),
            _ => Err(invalid(
                label,
                format!("element {index} must be a JSON object"),
            )),
        })
        .collect()
}

fn invalid(label: &str, detail: String) -> ClientError {
    ClientError::new(ErrorCode::Validation, format!("{label}: {detail}"))
}

#[cfg(test)]
mod tests {
    use mongodb::bson::{DateTime, doc, oid::ObjectId};

    use super::*;

    #[test]
    fn extended_json_input_preserves_types_and_key_order() {
        let document = parse_document(
            r#"{"_id": {"$oid": "65f1a2b3c4d5e6f708192a3b"}, "z": 1, "a": {"$date": "2026-10-10T00:00:00Z"}, "n": {"$numberLong": "9007199254740993"}}"#,
            "--filter",
        )
        .unwrap();
        assert_eq!(document.keys().collect::<Vec<_>>(), ["_id", "z", "a", "n"]);
        assert!(matches!(document.get("_id"), Some(Bson::ObjectId(_))));
        assert!(matches!(document.get("a"), Some(Bson::DateTime(_))));
        assert_eq!(document.get("n"), Some(&Bson::Int64(9_007_199_254_740_993)));
    }

    #[test]
    fn shapes_are_validated_with_the_option_label() {
        let error = parse_document("[1]", "--filter").unwrap_err();
        assert_eq!(error.to_string(), "--filter: expected a JSON object");
        assert_eq!(error.code(), ErrorCode::Validation);
        assert!(
            parse_document("{", "--filter")
                .unwrap_err()
                .to_string()
                .starts_with("--filter: invalid JSON")
        );
        assert!(
            parse_document_array("[{}, 2]", "--pipeline")
                .unwrap_err()
                .to_string()
                .contains("element 1")
        );
        assert!(parse_document_array("{}", "--pipeline").is_err());
        assert_eq!(
            parse_documents(r#"{"a": 1}"#, "--documents").unwrap().len(),
            1
        );
        assert_eq!(
            parse_documents(r#"[{"a": 1}, {"a": 2}]"#, "--documents")
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn output_is_relaxed_extended_json() {
        let id = ObjectId::parse_str("65f1a2b3c4d5e6f708192a3b").unwrap();
        let value =
            document_to_json(doc! {"_id": id, "count": 3_i64, "at": DateTime::from_millis(0)});
        assert_eq!(
            value["_id"],
            serde_json::json!({"$oid": "65f1a2b3c4d5e6f708192a3b"})
        );
        assert_eq!(value["count"], 3);
        assert!(
            value["at"]["$date"]
                .as_str()
                .unwrap()
                .starts_with("1970-01-01T00:00:00")
        );
    }
}
