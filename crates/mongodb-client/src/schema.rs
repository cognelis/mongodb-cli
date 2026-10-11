//! Schema inference from sampled documents.

use std::collections::{BTreeMap, HashMap, HashSet};

use mongodb::bson::{Bson, Document};
use serde::Serialize;

/// Deeper values are counted at their parent path only, bounding work on pathological documents.
const MAX_DEPTH: usize = 32;

#[derive(Debug, PartialEq, Serialize)]
pub struct FieldSummary {
    /// Dotted field path; `[]` marks array elements, as in `items[].sku`.
    pub path: String,
    /// The number of sampled documents containing the path.
    pub count: u64,
    /// Occurrences per BSON type.
    pub types: BTreeMap<&'static str, u64>,
}

/// Summarize field paths in their first-seen order.
pub fn infer(documents: &[Document]) -> Vec<FieldSummary> {
    let mut collector = Collector::default();
    for document in documents {
        collector.seen.clear();
        for (key, value) in document {
            collector.walk(key, value, 1);
        }
    }
    let Collector {
        order, mut stats, ..
    } = collector;
    order
        .into_iter()
        .map(|path| {
            let (count, types) = stats
                .remove(&path)
                .expect("every ordered path has statistics");
            FieldSummary { path, count, types }
        })
        .collect()
}

#[derive(Default)]
struct Collector {
    order: Vec<String>,
    stats: HashMap<String, (u64, BTreeMap<&'static str, u64>)>,
    seen: HashSet<String>,
}

impl Collector {
    fn walk(&mut self, path: &str, value: &Bson, depth: usize) {
        self.record(path, value);
        if depth >= MAX_DEPTH {
            return;
        }
        match value {
            Bson::Document(document) => {
                for (key, nested) in document {
                    self.walk(&format!("{path}.{key}"), nested, depth + 1);
                }
            }
            Bson::Array(items) => {
                let element_path = format!("{path}[]");
                for item in items {
                    self.walk(&element_path, item, depth + 1);
                }
            }
            _ => {}
        }
    }

    fn record(&mut self, path: &str, value: &Bson) {
        if !self.stats.contains_key(path) {
            self.order.push(path.to_owned());
        }
        let entry = self.stats.entry(path.to_owned()).or_default();
        *entry.1.entry(type_name(value)).or_default() += 1;
        if self.seen.insert(path.to_owned()) {
            entry.0 += 1;
        }
    }
}

fn type_name(value: &Bson) -> &'static str {
    match value {
        Bson::Double(_) => "Double",
        Bson::String(_) => "String",
        Bson::Array(_) => "Array",
        Bson::Document(_) => "Document",
        Bson::Boolean(_) => "Boolean",
        Bson::Null => "Null",
        Bson::RegularExpression(_) => "RegExp",
        Bson::JavaScriptCode(_) => "JavaScript",
        Bson::JavaScriptCodeWithScope(_) => "JavaScriptWithScope",
        Bson::Int32(_) => "Int32",
        Bson::Int64(_) => "Int64",
        Bson::Timestamp(_) => "Timestamp",
        Bson::Binary(_) => "Binary",
        Bson::ObjectId(_) => "ObjectId",
        Bson::DateTime(_) => "Date",
        Bson::Symbol(_) => "Symbol",
        Bson::Decimal128(_) => "Decimal128",
        Bson::Undefined => "Undefined",
        Bson::MaxKey => "MaxKey",
        Bson::MinKey => "MinKey",
        Bson::DbPointer(_) => "DBPointer",
    }
}

#[cfg(test)]
mod tests {
    use mongodb::bson::{Bson, doc, oid::ObjectId};

    use super::*;

    #[test]
    fn summarizes_nested_paths_types_and_presence_in_first_seen_order() {
        let documents = vec![
            doc! {"_id": ObjectId::new(), "name": "a", "address": {"city": "x"}, "tags": ["t", 1]},
            doc! {"_id": ObjectId::new(), "name": Bson::Null, "items": [{"sku": "s1"}, {"sku": "s2"}]},
        ];

        let fields = infer(&documents);
        let paths: Vec<_> = fields.iter().map(|field| field.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "_id",
                "name",
                "address",
                "address.city",
                "tags",
                "tags[]",
                "items",
                "items[]",
                "items[].sku"
            ]
        );
        let name = &fields[1];
        assert_eq!(name.count, 2);
        assert_eq!(name.types, BTreeMap::from([("Null", 1), ("String", 1)]));
        let tag_elements = &fields[5];
        assert_eq!(tag_elements.count, 1);
        assert_eq!(
            tag_elements.types,
            BTreeMap::from([("Int32", 1), ("String", 1)])
        );
        let skus = fields.last().unwrap();
        assert_eq!(skus.count, 1, "presence counts documents, not elements");
        assert_eq!(skus.types, BTreeMap::from([("String", 2)]));
    }

    #[test]
    fn empty_samples_have_no_fields() {
        assert!(infer(&[]).is_empty());
    }
}
