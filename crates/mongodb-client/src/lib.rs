//! Bounded MongoDB operations for the mongodb-cli executable.

pub mod atlas;
pub mod client;
pub mod ejson;
pub mod error;
pub mod guard;
pub mod schema;
pub mod uri;

pub use client::{
    ExplainTarget, ExportSource, FindQuery, IndexCheckTarget, LogType, MAX_RESULT_BYTES,
    MongoClient, NewIndex, Page, PageRequest, Verbosity,
};
pub use error::{ClientError, ErrorCode};
pub use mongodb::bson::{Bson, Document};
pub use uri::ConnectionUri;
