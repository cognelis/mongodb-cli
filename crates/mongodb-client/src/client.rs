//! Bounded MongoDB operations used by the CLI.

use std::future::{Future, IntoFuture};
use std::io;
use std::time::Duration;

use mongodb::bson::{Bson, Document, doc};
use mongodb::options::{ClientOptions, UpdateModifications};
use mongodb::{Client, Collection, Cursor};
use serde_json::{Map, Value, json};

use crate::ejson::{document_to_json, to_json};
use crate::uri::ConnectionUri;
use crate::{ClientError, ErrorCode, schema};

/// Upper bound on the BSON bytes of one metadata response, the MongoDB MCP server's `maxBytesPerQuery`.
pub const MAX_RESULT_BYTES: usize = 16 * 1024 * 1024;

pub struct MongoClient {
    client: Client,
    timeout: Duration,
}

/// One page of a find or aggregation and the bounds that apply to it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageRequest {
    pub limit: u32,
    pub offset: u32,
    /// Stop before the BSON bytes of the page would exceed this; `usize::MAX` disables it.
    pub max_bytes: usize,
    /// Upper bound for the best-effort total count that accompanies the page.
    pub count_time_cap: Duration,
}

#[derive(Debug, Default)]
pub struct Page {
    pub documents: Vec<Value>,
    pub has_more: bool,
    /// The page stopped early because of `PageRequest::max_bytes`.
    pub truncated: bool,
    pub total: Option<u64>,
    pub warnings: Vec<String>,
}

/// What `MongoClient::export` streams.
#[derive(Clone, Debug)]
pub enum ExportSource {
    Find {
        query: Box<FindQuery>,
        limit: Option<u32>,
    },
    Aggregate {
        pipeline: Vec<Document>,
    },
}

/// An operation whose plan `MongoClient::check_index_usage` inspects.
#[derive(Clone, Debug)]
pub enum IndexCheckTarget {
    Find {
        query: FindQuery,
        page: PageRequest,
    },
    Count {
        filter: Document,
    },
    Aggregate {
        pipeline: Vec<Document>,
    },
    Update {
        filter: Document,
        update: Bson,
        upsert: bool,
    },
    Delete {
        filter: Document,
    },
}

#[derive(Clone, Debug, Default)]
pub struct FindQuery {
    pub filter: Document,
    pub projection: Option<Document>,
    pub sort: Option<Document>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Verbosity {
    QueryPlanner,
    QueryPlannerExtended,
    ExecutionStats,
    AllPlansExecution,
}

impl Verbosity {
    fn as_str(self) -> &'static str {
        match self {
            Self::QueryPlanner => "queryPlanner",
            Self::QueryPlannerExtended => "queryPlannerExtended",
            Self::ExecutionStats => "executionStats",
            Self::AllPlansExecution => "allPlansExecution",
        }
    }
}

#[derive(Clone, Debug)]
pub enum ExplainTarget {
    Find {
        query: FindQuery,
        limit: Option<u32>,
    },
    Aggregate {
        pipeline: Vec<Document>,
    },
    Count {
        filter: Document,
    },
}

#[derive(Clone, Debug)]
pub enum NewIndex {
    Classic {
        keys: Document,
        name: Option<String>,
        options: Document,
    },
    Search {
        name: Option<String>,
        definition: Document,
        vector: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogType {
    Global,
    StartupWarnings,
}

impl LogType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::StartupWarnings => "startupWarnings",
        }
    }
}

impl MongoClient {
    /// Configure a client. Server selection and authentication happen on the first operation.
    pub async fn connect(uri: &ConnectionUri, timeout: Duration) -> Result<Self, ClientError> {
        // Parsing resolves mongodb+srv records, so it shares the operation deadline.
        let mut options = tokio::time::timeout(timeout, ClientOptions::parse(uri.expose()))
            .await
            .map_err(|_| timed_out(timeout))??;
        prefer_direct_connection(&mut options, uri.expose().starts_with("mongodb+srv://"));
        options
            .app_name
            .get_or_insert_with(|| concat!("mongodb-cli/", env!("CARGO_PKG_VERSION")).into());
        options
            .server_selection_timeout
            .get_or_insert(timeout.min(Duration::from_secs(30)));
        options
            .connect_timeout
            .get_or_insert(timeout.min(Duration::from_secs(10)));
        Ok(Self {
            client: Client::with_options(options)?,
            timeout,
        })
    }

    pub async fn ping(&self) -> Result<(), ClientError> {
        self.command("admin", doc! {"ping": 1}).await.map(|_| ())
    }

    pub async fn server_info(&self) -> Result<Value, ClientError> {
        let build = self.command("admin", doc! {"buildInfo": 1}).await?;
        let hello = self.command("admin", doc! {"hello": 1}).await?;
        let topology = if hello
            .get_str("msg")
            .is_ok_and(|message| message == "isdbgrid")
        {
            "sharded"
        } else if hello.contains_key("setName") {
            "replicaSet"
        } else {
            "standalone"
        };
        let mut info = Map::new();
        info.insert(
            "version".into(),
            json!(build.get_str("version").unwrap_or("unknown")),
        );
        info.insert("topology".into(), json!(topology));
        if let Ok(set_name) = hello.get_str("setName") {
            info.insert("setName".into(), json!(set_name));
        }
        if let Ok(primary) = hello.get_bool("isWritablePrimary") {
            info.insert("writablePrimary".into(), json!(primary));
        }
        Ok(Value::Object(info))
    }

    /// The authenticated users and their roles for this connection.
    pub async fn connection_status(&self) -> Result<Value, ClientError> {
        let status = self.command("admin", doc! {"connectionStatus": 1}).await?;
        let auth = status.get_document("authInfo").cloned().unwrap_or_default();
        let list = |key: &str| to_json(auth.get(key).cloned().unwrap_or(Bson::Array(Vec::new())));
        Ok(json!({
            "users": list("authenticatedUsers"),
            "roles": list("authenticatedUserRoles"),
        }))
    }

    pub async fn server_logs(&self, log_type: LogType, limit: u32) -> Result<Value, ClientError> {
        let result = self
            .command("admin", doc! {"getLog": log_type.as_str()})
            .await?;
        let lines: Vec<&str> = result
            .get_array("log")
            .map_err(|error| incompatible(format!("getLog returned no log array: {error}")))?
            .iter()
            .filter_map(Bson::as_str)
            .collect();
        // getLog lists entries oldest first; keep the most recent ones.
        let start = lines.len().saturating_sub(limit as usize);
        let logs: Vec<Value> = lines[start..]
            .iter()
            .map(|line| {
                serde_json::from_str(line).unwrap_or_else(|_| Value::String(line.trim_end().into()))
            })
            .collect();
        Ok(json!({
            "totalLinesWritten": number(&result, "totalLinesWritten"),
            "shownCount": logs.len(),
            "logs": logs,
        }))
    }

    pub async fn list_databases(&self) -> Result<Vec<Value>, ClientError> {
        self.deadline(async {
            let databases = self
                .client
                .list_databases()
                .authorized_databases(true)
                .await?;
            Ok(databases
                .into_iter()
                .map(|database| {
                    json!({
                        "name": database.name,
                        "sizeOnDisk": database.size_on_disk,
                        "empty": database.empty,
                    })
                })
                .collect())
        })
        .await
    }

    pub async fn database_stats(&self, database: &str) -> Result<Value, ClientError> {
        self.command(database, doc! {"dbStats": 1, "scale": 1})
            .await
            .map(document_to_json)
    }

    pub async fn drop_database(&self, database: &str) -> Result<(), ClientError> {
        self.write_deadline(async { Ok(self.client.database(database).drop().await?) })
            .await
    }

    pub async fn list_collections(&self, database: &str) -> Result<Vec<Value>, ClientError> {
        self.deadline(async {
            let cursor = self
                .client
                .database(database)
                .run_cursor_command(doc! {
                    "listCollections": 1,
                    "nameOnly": true,
                    "authorizedCollections": true,
                })
                .await?;
            let mut collections = drain(cursor).await?;
            collections.sort_by(|left, right| {
                left.get_str("name")
                    .unwrap_or_default()
                    .cmp(right.get_str("name").unwrap_or_default())
            });
            Ok(collections
                .iter()
                .map(|collection| {
                    json!({
                        "name": collection.get_str("name").unwrap_or_default(),
                        "type": collection.get_str("type").unwrap_or("collection"),
                    })
                })
                .collect())
        })
        .await
    }

    pub async fn create_collection(
        &self,
        database: &str,
        collection: &str,
        options: Document,
    ) -> Result<(), ClientError> {
        let mut command = doc! {"create": collection};
        command.extend(options);
        self.write_command(database, command).await.map(|_| ())
    }

    pub async fn drop_collection(
        &self,
        database: &str,
        collection: &str,
    ) -> Result<(), ClientError> {
        self.write_deadline(async { Ok(self.collection(database, collection).drop().await?) })
            .await
    }

    pub async fn rename_collection(
        &self,
        database: &str,
        collection: &str,
        new_name: &str,
        drop_target: bool,
    ) -> Result<(), ClientError> {
        self.write_command(
            "admin",
            doc! {
                "renameCollection": format!("{database}.{collection}"),
                "to": format!("{database}.{new_name}"),
                "dropTarget": drop_target,
            },
        )
        .await
        .map(|_| ())
    }

    pub async fn collection_stats(
        &self,
        database: &str,
        collection: &str,
    ) -> Result<Value, ClientError> {
        self.deadline(async {
            let cursor = self
                .collection(database, collection)
                .aggregate(vec![doc! {"$collStats": {"storageStats": {}}}])
                .max_time(self.timeout)
                .await?;
            // A sharded collection returns one document per shard.
            let shards = drain(cursor).await?;
            let (mut count, mut size, mut storage_size, mut index_size, mut indexes) =
                (0, 0, 0, 0, 0);
            for shard in &shards {
                let stats = shard.get_document("storageStats").map_err(|error| {
                    incompatible(format!("$collStats returned no storageStats: {error}"))
                })?;
                count += number(stats, "count");
                size += number(stats, "size");
                storage_size += number(stats, "storageSize");
                index_size += number(stats, "totalIndexSize");
                indexes = indexes.max(number(stats, "nindexes"));
            }
            let mut summary = json!({
                "count": count,
                "size": size,
                "avgObjSize": if count > 0 { size / count } else { 0 },
                "storageSize": storage_size,
                "totalIndexSize": index_size,
                "nindexes": indexes,
            });
            if shards.len() > 1 {
                summary["shards"] = json!(shards.len());
            }
            Ok(summary)
        })
        .await
    }

    pub async fn sample_schema(
        &self,
        database: &str,
        collection: &str,
        sample_size: u32,
        max_bytes: usize,
    ) -> Result<Value, ClientError> {
        self.deadline(async {
            let cursor = self
                .collection(database, collection)
                .aggregate(vec![doc! {"$sample": {"size": i64::from(sample_size)}}])
                .max_time(self.timeout)
                .await?;
            let collected = collect(cursor, sample_size as usize, max_bytes).await?;
            let fields = schema::infer(&collected.documents);
            let top_level = fields
                .iter()
                .filter(|field| !field.path.contains(['.', '[']))
                .count();
            Ok(json!({
                "sampled": collected.documents.len(),
                "truncated": collected.truncated,
                "fieldsCount": top_level,
                "fields": fields,
            }))
        })
        .await
    }

    pub async fn list_indexes(
        &self,
        database: &str,
        collection: &str,
    ) -> Result<Vec<Value>, ClientError> {
        self.deadline(async {
            let cursor = self
                .client
                .database(database)
                .run_cursor_command(doc! {"listIndexes": collection})
                .await?;
            Ok(drain(cursor)
                .await?
                .into_iter()
                .map(document_to_json)
                .collect())
        })
        .await
    }

    pub async fn list_search_indexes(
        &self,
        database: &str,
        collection: &str,
    ) -> Result<Vec<Value>, ClientError> {
        self.deadline(async {
            let cursor = self
                .collection(database, collection)
                .aggregate(vec![doc! {"$listSearchIndexes": {}}])
                .max_time(self.timeout)
                .await?;
            Ok(drain(cursor)
                .await?
                .into_iter()
                .map(|mut index| {
                    if !index.contains_key("type") {
                        index.insert("type", search_index_type(&index));
                    }
                    document_to_json(index)
                })
                .collect())
        })
        .await
    }

    pub async fn create_index(
        &self,
        database: &str,
        collection: &str,
        index: NewIndex,
    ) -> Result<Value, ClientError> {
        match index {
            NewIndex::Classic {
                keys,
                name,
                options,
            } => {
                let name = name
                    .or_else(|| options.get_str("name").ok().map(str::to_owned))
                    .unwrap_or_else(|| default_index_name(&keys));
                // The server validates every option, so a misspelled option fails loudly.
                let mut spec = doc! {"key": keys, "name": &name};
                spec.extend(
                    options
                        .into_iter()
                        .filter(|(key, _)| key != "name" && key != "key"),
                );
                let result = self
                    .write_command(
                        database,
                        doc! {"createIndexes": collection, "indexes": [spec]},
                    )
                    .await?;
                Ok(json!({
                    "database": database,
                    "collection": collection,
                    "name": name,
                    "type": "classic",
                    "created": number(&result, "numIndexesAfter") > number(&result, "numIndexesBefore"),
                }))
            }
            NewIndex::Search {
                name,
                definition,
                vector,
            } => {
                let index_type = if vector { "vectorSearch" } else { "search" };
                let mut spec = doc! {"type": index_type, "definition": definition};
                if let Some(name) = &name {
                    spec.insert("name", name);
                }
                let result = self
                    .write_command(
                        database,
                        doc! {"createSearchIndexes": collection, "indexes": [spec]},
                    )
                    .await?;
                let created_name = result
                    .get_array("indexesCreated")
                    .ok()
                    .and_then(|created| created.first())
                    .and_then(Bson::as_document)
                    .and_then(|created| created.get_str("name").ok())
                    .map(str::to_owned)
                    .or(name)
                    .unwrap_or_else(|| "default".into());
                Ok(json!({
                    "database": database,
                    "collection": collection,
                    "name": created_name,
                    "type": index_type,
                    "created": true,
                }))
            }
        }
    }

    pub async fn drop_index(
        &self,
        database: &str,
        collection: &str,
        name: &str,
        search: bool,
    ) -> Result<(), ClientError> {
        let command = if search {
            doc! {"dropSearchIndex": collection, "name": name}
        } else {
            doc! {"dropIndexes": collection, "index": name}
        };
        self.write_command(database, command).await.map(|_| ())
    }

    pub async fn find(
        &self,
        database: &str,
        collection: &str,
        query: FindQuery,
        page: PageRequest,
    ) -> Result<Page, ClientError> {
        self.deadline(async {
            let collection = self.collection(database, collection);
            // Requesting one extra document reveals whether another page exists.
            let mut find = collection
                .find(query.filter.clone())
                .limit(i64::from(page.limit) + 1)
                .skip(u64::from(page.offset))
                .max_time(self.timeout);
            if let Some(projection) = query.projection {
                find = find.projection(projection);
            }
            if let Some(sort) = query.sort {
                find = find.sort(sort);
            }
            let count = collection
                .count_documents(query.filter)
                .max_time(self.timeout.min(page.count_time_cap));
            let (cursor, total) = tokio::join!(find.into_future(), count.into_future());
            let mut result = collect(cursor?, page.limit as usize, page.max_bytes)
                .await?
                .into_page(page.max_bytes);
            record_total(&mut result, total.map_err(ClientError::from));
            Ok(result)
        })
        .await
    }

    pub async fn count(
        &self,
        database: &str,
        collection: &str,
        filter: Document,
    ) -> Result<u64, ClientError> {
        self.deadline(async {
            Ok(self
                .collection(database, collection)
                .count_documents(filter)
                .max_time(self.timeout)
                .await?)
        })
        .await
    }

    /// Aggregate a collection, or the database when `collection` is `None`.
    ///
    /// With a page, `$skip`/`$limit` stages are appended and a bounded `$count` runs alongside,
    /// as the MCP server does; write pipelines must not be paged.
    pub async fn aggregate(
        &self,
        database: &str,
        collection: Option<&str>,
        pipeline: Vec<Document>,
        page: Option<PageRequest>,
    ) -> Result<Page, ClientError> {
        let run = |pipeline: Vec<Document>, max_time: Duration| async move {
            let database = self.client.database(database);
            match collection {
                Some(collection) => {
                    database
                        .collection::<Document>(collection)
                        .aggregate(pipeline)
                        .max_time(max_time)
                        .await
                }
                None => database.aggregate(pipeline).max_time(max_time).await,
            }
        };
        let Some(page) = page else {
            return self
                .write_deadline(async {
                    let cursor = run(pipeline, self.timeout).await?;
                    Ok(collect(cursor, usize::MAX, usize::MAX)
                        .await?
                        .into_page(usize::MAX))
                })
                .await;
        };
        self.deadline(async {
            let mut counted = pipeline.clone();
            counted.push(doc! {"$count": "totalDocuments"});
            let total = async {
                let cursor = run(counted, self.timeout.min(page.count_time_cap)).await?;
                let documents = drain(cursor).await?;
                Ok::<_, ClientError>(documents.first().map_or(0, |document| {
                    number(document, "totalDocuments").max(0) as u64
                }))
            };
            let mut paged = pipeline;
            if page.offset > 0 {
                paged.push(doc! {"$skip": i64::from(page.offset)});
            }
            paged.push(doc! {"$limit": i64::from(page.limit) + 1});
            let (cursor, total) = tokio::join!(run(paged, self.timeout), total);
            let mut result = collect(cursor?, page.limit as usize, page.max_bytes)
                .await?
                .into_page(page.max_bytes);
            record_total(&mut result, total);
            Ok(result)
        })
        .await
    }

    /// Stream every matching document as an Extended JSON array, without page or byte limits.
    ///
    /// The format matches the MCP server's export files: `[`, documents joined by `,\n`, `]`.
    pub async fn export(
        &self,
        database: &str,
        collection: &str,
        source: ExportSource,
        canonical: bool,
        output: &mut (dyn io::Write + Send),
    ) -> Result<u64, ClientError> {
        self.deadline(async {
            let collection = self.collection(database, collection);
            let mut cursor = match source {
                ExportSource::Find { query, limit } => {
                    let mut find = collection.find(query.filter).max_time(self.timeout);
                    if let Some(projection) = query.projection {
                        find = find.projection(projection);
                    }
                    if let Some(sort) = query.sort {
                        find = find.sort(sort);
                    }
                    if let Some(limit) = limit {
                        find = find.limit(i64::from(limit));
                    }
                    find.await?
                }
                ExportSource::Aggregate { pipeline } => {
                    collection
                        .aggregate(pipeline)
                        .allow_disk_use(true)
                        .max_time(self.timeout)
                        .await?
                }
            };
            let mut exported = 0_u64;
            // A consumer such as `head` may close stdout early; stop cleanly like other output.
            macro_rules! emit {
                ($result:expr) => {
                    match $result {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                            return Ok(exported);
                        }
                        Err(error) => {
                            return Err(ClientError::new(
                                ErrorCode::IncompatibleResponse,
                                format!("failed to write the export: {error}"),
                            ));
                        }
                    }
                };
            }
            emit!(output.write_all(b"["));
            while cursor.advance().await? {
                let document = Bson::Document(cursor.deserialize_current()?);
                let value = if canonical {
                    document.into_canonical_extjson()
                } else {
                    document.into_relaxed_extjson()
                };
                if exported > 0 {
                    emit!(output.write_all(b",\n"));
                }
                emit!(serde_json::to_writer(&mut *output, &value).map_err(io::Error::from));
                exported += 1;
            }
            emit!(output.write_all(b"]\n"));
            emit!(output.flush());
            Ok(exported)
        })
        .await
    }

    /// Fail with `forbidden` when the operation would scan the whole collection, like the MCP
    /// server's `indexCheck`. Returns a warning when the plan cannot be inspected.
    pub async fn check_index_usage(
        &self,
        database: &str,
        collection: &str,
        target: IndexCheckTarget,
    ) -> Result<Option<String>, ClientError> {
        let (operation, explained) = match target {
            IndexCheckTarget::Find { query, page } => {
                let mut command = doc! {
                    "find": collection,
                    "filter": query.filter,
                    "limit": i64::from(page.limit) + 1,
                    "skip": i64::from(page.offset),
                };
                if let Some(projection) = query.projection {
                    command.insert("projection", projection);
                }
                if let Some(sort) = query.sort {
                    command.insert("sort", sort);
                }
                ("find", command)
            }
            IndexCheckTarget::Count { filter } => {
                ("count", doc! {"count": collection, "query": filter})
            }
            IndexCheckTarget::Aggregate { pipeline } => {
                if let Some(index) = vector_search_index(&pipeline) {
                    return self
                        .check_vector_search_index(database, collection, &index)
                        .await;
                }
                (
                    "aggregate",
                    doc! {"aggregate": collection, "pipeline": pipeline, "cursor": {}},
                )
            }
            IndexCheckTarget::Update {
                filter,
                update,
                upsert,
            } => (
                "updateMany",
                doc! {"update": collection, "updates": [{"q": filter, "u": update, "upsert": upsert, "multi": true}]},
            ),
            IndexCheckTarget::Delete { filter } => (
                "deleteMany",
                doc! {"delete": collection, "deletes": [{"q": filter, "limit": 0}]},
            ),
        };
        let plan = match self
            .command(
                database,
                doc! {"explain": explained, "verbosity": "queryPlanner"},
            )
            .await
        {
            Ok(plan) => plan,
            Err(error) => {
                return Ok(Some(format!(
                    "index check skipped: the query plan could not be inspected ({error})"
                )));
            }
        };
        if uses_index(&plan) {
            Ok(None)
        } else {
            Err(ClientError::new(
                ErrorCode::Forbidden,
                format!(
                    "index check failed: the {operation} operation on \"{database}.{collection}\" performs a collection scan instead of using an index"
                ),
            )
            .with_hint("inspect the plan with explain, list indexes, or ask the user to create an index"))
        }
    }

    async fn check_vector_search_index(
        &self,
        database: &str,
        collection: &str,
        index: &str,
    ) -> Result<Option<String>, ClientError> {
        let found = self
            .deadline(async {
                let cursor = self
                    .collection(database, collection)
                    .aggregate(vec![doc! {"$listSearchIndexes": {"name": index}}])
                    .max_time(self.timeout)
                    .await?;
                Ok(!drain(cursor).await?.is_empty())
            })
            .await;
        match found {
            Ok(true) => Ok(None),
            Ok(false) => Err(ClientError::new(
                ErrorCode::NotFound,
                format!("could not find an index named \"{index}\" in namespace \"{database}.{collection}\""),
            )
            .with_hint("list search indexes with `indexes list --type search`")),
            // The MCP server treats a failed probe as "index exists".
            Err(error) => Ok(Some(format!(
                "index check skipped: search indexes could not be listed ({error})"
            ))),
        }
    }

    pub async fn explain(
        &self,
        database: &str,
        collection: &str,
        target: ExplainTarget,
        verbosity: Verbosity,
    ) -> Result<Value, ClientError> {
        // executionStats runs the plan, so the server must stop it at the same deadline.
        let max_time = i64::try_from(self.timeout.as_millis()).unwrap_or(i64::MAX);
        let explained = match target {
            ExplainTarget::Find { query, limit } => {
                let mut command =
                    doc! {"find": collection, "filter": query.filter, "maxTimeMS": max_time};
                if let Some(projection) = query.projection {
                    command.insert("projection", projection);
                }
                if let Some(sort) = query.sort {
                    command.insert("sort", sort);
                }
                if let Some(limit) = limit {
                    command.insert("limit", i64::from(limit));
                }
                command
            }
            ExplainTarget::Aggregate { pipeline } => doc! {
                "aggregate": collection,
                "pipeline": pipeline,
                "cursor": {},
                "maxTimeMS": max_time,
            },
            ExplainTarget::Count { filter } => {
                doc! {"count": collection, "query": filter, "maxTimeMS": max_time}
            }
        };
        self.command(
            database,
            doc! {"explain": explained, "verbosity": verbosity.as_str()},
        )
        .await
        .map(document_to_json)
    }

    pub async fn insert_many(
        &self,
        database: &str,
        collection: &str,
        documents: Vec<Document>,
    ) -> Result<Value, ClientError> {
        self.write_deadline(async {
            let result = self
                .collection(database, collection)
                .insert_many(documents)
                .await?;
            let mut ids: Vec<_> = result.inserted_ids.into_iter().collect();
            ids.sort_by_key(|(index, _)| *index);
            Ok(json!({
                "insertedCount": ids.len(),
                "insertedIds": ids.into_iter().map(|(_, id)| to_json(id)).collect::<Vec<_>>(),
            }))
        })
        .await
    }

    /// Apply an update document or an aggregation-pipeline update to every matching document.
    pub async fn update_many(
        &self,
        database: &str,
        collection: &str,
        filter: Document,
        update: Bson,
        upsert: bool,
    ) -> Result<Value, ClientError> {
        let update = match update {
            Bson::Document(document) => UpdateModifications::Document(document),
            Bson::Array(stages) => UpdateModifications::Pipeline(
                stages
                    .into_iter()
                    .map(|stage| match stage {
                        Bson::Document(stage) => Ok(stage),
                        _ => Err(ClientError::new(
                            ErrorCode::Validation,
                            "--update: pipeline stages must be JSON objects",
                        )),
                    })
                    .collect::<Result<_, _>>()?,
            ),
            _ => {
                return Err(ClientError::new(
                    ErrorCode::Validation,
                    "--update: expected an update document or an array of pipeline stages",
                ));
            }
        };
        self.write_deadline(async {
            let result = self
                .collection(database, collection)
                .update_many(filter, update)
                .upsert(upsert)
                .await?;
            Ok(json!({
                "matchedCount": result.matched_count,
                "modifiedCount": result.modified_count,
                "upsertedCount": u64::from(result.upserted_id.is_some()),
                "upsertedId": result.upserted_id.map(to_json),
            }))
        })
        .await
    }

    pub async fn delete_many(
        &self,
        database: &str,
        collection: &str,
        filter: Document,
    ) -> Result<Value, ClientError> {
        self.write_deadline(async {
            let result = self
                .collection(database, collection)
                .delete_many(filter)
                .await?;
            Ok(json!({"deletedCount": result.deleted_count}))
        })
        .await
    }

    fn collection(&self, database: &str, collection: &str) -> Collection<Document> {
        self.client.database(database).collection(collection)
    }

    async fn command(&self, database: &str, command: Document) -> Result<Document, ClientError> {
        self.deadline(async { Ok(self.client.database(database).run_command(command).await?) })
            .await
    }

    async fn write_command(
        &self,
        database: &str,
        command: Document,
    ) -> Result<Document, ClientError> {
        self.write_deadline(async {
            Ok(self.client.database(database).run_command(command).await?)
        })
        .await
    }

    async fn deadline<T>(
        &self,
        operation: impl Future<Output = Result<T, ClientError>>,
    ) -> Result<T, ClientError> {
        tokio::time::timeout(self.timeout, operation)
            .await
            .map_err(|_| timed_out(self.timeout))?
    }

    async fn write_deadline<T>(
        &self,
        operation: impl Future<Output = Result<T, ClientError>>,
    ) -> Result<T, ClientError> {
        self.deadline(operation).await.map_err(|error| {
            if error.code() == ErrorCode::Timeout {
                error.with_hint(
                    "the write may have partially completed; verify the data before retrying",
                )
            } else {
                error
            }
        })
    }
}

struct Collected {
    documents: Vec<Document>,
    has_more: bool,
    truncated: bool,
}

impl Collected {
    fn into_page(self, max_bytes: usize) -> Page {
        let mut warnings = Vec::new();
        if self.truncated {
            warnings.push(format!(
                "the page stopped at the {max_bytes}-byte response limit; continue with --offset, narrow the projection, or raise --response-bytes-limit"
            ));
        }
        Page {
            documents: self.documents.into_iter().map(document_to_json).collect(),
            has_more: self.has_more,
            truncated: self.truncated,
            total: None,
            warnings,
        }
    }
}

/// Collect up to `limit` documents, stopping before their BSON size would reach `max_bytes`
/// as the MCP server does. A first document that alone exceeds the limit is an error, so
/// offset pagination always advances.
async fn collect(
    mut cursor: Cursor<Document>,
    limit: usize,
    max_bytes: usize,
) -> Result<Collected, ClientError> {
    let mut documents = Vec::new();
    let mut bytes: usize = 0;
    let mut has_more = false;
    let mut truncated = false;
    while cursor.advance().await? {
        if documents.len() == limit {
            has_more = true;
            break;
        }
        let size = cursor.current().as_bytes().len();
        if bytes.saturating_add(size) >= max_bytes {
            if documents.is_empty() {
                return Err(ClientError::new(
                    ErrorCode::ResponseTooLarge,
                    format!("a {size}-byte document exceeds the {max_bytes}-byte response limit"),
                )
                .with_hint("raise --response-bytes-limit or narrow the projection"));
            }
            has_more = true;
            truncated = true;
            break;
        }
        bytes += size;
        documents.push(cursor.deserialize_current()?);
    }
    Ok(Collected {
        documents,
        has_more,
        truncated,
    })
}

/// Collect a metadata cursor completely; an oversized metadata response is an error.
async fn drain(cursor: Cursor<Document>) -> Result<Vec<Document>, ClientError> {
    let collected = collect(cursor, usize::MAX, MAX_RESULT_BYTES).await?;
    if collected.truncated {
        return Err(ClientError::new(
            ErrorCode::ResponseTooLarge,
            format!(
                "the server response exceeded {} MiB",
                MAX_RESULT_BYTES / (1024 * 1024)
            ),
        ));
    }
    Ok(collected.documents)
}

fn record_total(page: &mut Page, total: Result<u64, ClientError>) {
    match total {
        Ok(total) => page.total = Some(total),
        Err(error) => page
            .warnings
            .push(format!("total count unavailable: {error}")),
    }
}

/// Infer a search index's type when the server omits it, in the MCP server's order.
fn search_index_type(index: &Document) -> String {
    let Ok(definition) = index.get_document("latestDefinition") else {
        return "UNKNOWN".into();
    };
    if let Ok(kind) = definition.get_str("type") {
        return kind.to_owned();
    }
    if let Ok(fields) = definition.get_array("fields") {
        let auto_embed = fields
            .iter()
            .filter_map(Bson::as_document)
            .any(|field| field.get_str("type").is_ok_and(|kind| kind == "autoEmbed"));
        return if auto_embed {
            "autoEmbed"
        } else {
            "vectorSearch"
        }
        .into();
    }
    if definition.contains_key("mappings") {
        "search".into()
    } else {
        "UNKNOWN".into()
    }
}

fn vector_search_index(pipeline: &[Document]) -> Option<String> {
    let stage = pipeline
        .iter()
        .find_map(|stage| stage.get_document("$vectorSearch").ok())?;
    Some(stage.get_str("index").unwrap_or("default").to_owned())
}

/// Plan stages that the MCP server's index check accepts as index use.
const INDEX_STAGES: &[&str] = &[
    "IXSCAN",
    "COUNT_SCAN",
    "EXPRESS_IXSCAN",
    "EXPRESS_CLUSTERED_IXSCAN",
    "EXPRESS_UPDATE",
    "EXPRESS_DELETE",
    "IDHACK",
];

/// Walk the winning plan's `inputStage` chain looking for an index stage. Aggregation plans
/// pushed into `$cursor` and SBE `queryPlan` wrappers are also understood.
fn uses_index(plan: &Document) -> bool {
    let planner = plan.get_document("queryPlanner").ok().or_else(|| {
        plan.get_array("stages")
            .ok()?
            .first()?
            .as_document()?
            .get_document("$cursor")
            .ok()?
            .get_document("queryPlanner")
            .ok()
    });
    let Some(mut stage) = planner.and_then(|planner| planner.get_document("winningPlan").ok())
    else {
        return false;
    };
    if let Ok(query_plan) = stage.get_document("queryPlan") {
        stage = query_plan;
    }
    loop {
        if stage
            .get_str("stage")
            .is_ok_and(|name| INDEX_STAGES.contains(&name))
        {
            return true;
        }
        match stage.get_document("inputStage") {
            Ok(next) => stage = next,
            Err(_) => return false,
        }
    }
}

/// Connect to a single host directly unless the connection string asks for discovery, as
/// mongosh and the MongoDB MCP server do. Replica-set discovery would otherwise switch to the
/// member names the set advertises, which are often unreachable behind Docker or NAT.
fn prefer_direct_connection(options: &mut ClientOptions, srv: bool) {
    if !srv
        && options.hosts.len() == 1
        && options.repl_set_name.is_none()
        && options.direct_connection.is_none()
        && options.load_balanced.is_none()
    {
        options.direct_connection = Some(true);
    }
}

/// Name an index the way MongoDB drivers and the shell do: `field_direction` pairs.
fn default_index_name(keys: &Document) -> String {
    keys.iter()
        .map(|(field, direction)| {
            let direction = match direction {
                Bson::String(value) => value.clone(),
                Bson::Int32(value) => value.to_string(),
                Bson::Int64(value) => value.to_string(),
                Bson::Double(value) => value.to_string(),
                other => other.to_string(),
            };
            format!("{field}_{direction}")
        })
        .collect::<Vec<_>>()
        .join("_")
}

fn number(document: &Document, key: &str) -> i64 {
    match document.get(key) {
        Some(Bson::Int32(value)) => i64::from(*value),
        Some(Bson::Int64(value)) => *value,
        // Storage statistics are reported as doubles by some server versions.
        Some(Bson::Double(value)) => *value as i64,
        _ => 0,
    }
}

fn timed_out(timeout: Duration) -> ClientError {
    ClientError::new(
        ErrorCode::Timeout,
        format!(
            "MongoDB did not respond within the {}-second timeout",
            timeout.as_secs()
        ),
    )
    .with_hint("raise --timeout or narrow the operation")
}

fn incompatible(message: String) -> ClientError {
    ClientError::new(ErrorCode::IncompatibleResponse, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn single_hosts_connect_directly_unless_discovery_is_requested() {
        for (uri, expected) in [
            ("mongodb://db.example.com:27017", Some(true)),
            (
                "mongodb://db.example.com/?directConnection=false",
                Some(false),
            ),
            ("mongodb://db.example.com/?replicaSet=rs0", None),
            ("mongodb://a.example.com,b.example.com", None),
            ("mongodb://db.example.com/?loadBalanced=true", None),
        ] {
            let mut options = ClientOptions::parse(uri).await.unwrap();
            prefer_direct_connection(&mut options, false);
            assert_eq!(options.direct_connection, expected, "{uri}");
        }
        let mut options = ClientOptions::parse("mongodb://db.example.com")
            .await
            .unwrap();
        prefer_direct_connection(&mut options, true);
        assert_eq!(
            options.direct_connection, None,
            "SRV connections always discover"
        );
    }

    #[test]
    fn index_check_accepts_index_stages_through_wrappers() {
        let ixscan = doc! {"stage": "FETCH", "inputStage": {"stage": "IXSCAN"}};
        for (plan, expected) in [
            (doc! {"queryPlanner": {"winningPlan": ixscan.clone()}}, true),
            (
                doc! {"queryPlanner": {"winningPlan": {"queryPlan": ixscan.clone()}}},
                true,
            ),
            (
                doc! {"stages": [{"$cursor": {"queryPlanner": {"winningPlan": ixscan}}}]},
                true,
            ),
            (
                doc! {"queryPlanner": {"winningPlan": {"stage": "IDHACK"}}},
                true,
            ),
            (
                doc! {"queryPlanner": {"winningPlan": {"stage": "COLLSCAN"}}},
                false,
            ),
            (
                doc! {"queryPlanner": {"winningPlan": {"stage": "SORT", "inputStage": {"stage": "COLLSCAN"}}}},
                false,
            ),
            (doc! {"shards": []}, false),
        ] {
            assert_eq!(uses_index(&plan), expected, "{plan}");
        }
    }

    #[test]
    fn search_index_types_are_inferred_in_mcp_order() {
        for (index, expected) in [
            (doc! {"latestDefinition": {"type": "custom"}}, "custom"),
            (
                doc! {"latestDefinition": {"fields": [{"type": "autoEmbed"}]}},
                "autoEmbed",
            ),
            (
                doc! {"latestDefinition": {"fields": [{"type": "vector"}]}},
                "vectorSearch",
            ),
            (
                doc! {"latestDefinition": {"mappings": {"dynamic": true}}},
                "search",
            ),
            (doc! {"latestDefinition": {}}, "UNKNOWN"),
            (doc! {}, "UNKNOWN"),
        ] {
            assert_eq!(search_index_type(&index), expected, "{index}");
        }
        assert_eq!(
            vector_search_index(&[doc! {"$vectorSearch": {"index": "v", "path": "p"}}]).as_deref(),
            Some("v")
        );
        assert_eq!(vector_search_index(&[doc! {"$match": {}}]), None);
    }

    #[test]
    fn default_index_names_follow_driver_convention() {
        assert_eq!(default_index_name(&doc! {"a": 1, "b": -1}), "a_1_b_-1");
        assert_eq!(default_index_name(&doc! {"body": "text"}), "body_text");
        assert_eq!(default_index_name(&doc! {"n": 1.0}), "n_1");
    }

    #[test]
    fn numbers_accept_every_numeric_bson_type() {
        let document = doc! {"a": 1_i32, "b": 2_i64, "c": 3.9, "d": "x"};
        assert_eq!(number(&document, "a"), 1);
        assert_eq!(number(&document, "b"), 2);
        assert_eq!(number(&document, "c"), 3);
        assert_eq!(number(&document, "d"), 0);
        assert_eq!(number(&document, "missing"), 0);
    }
}
