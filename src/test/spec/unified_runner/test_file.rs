use std::{sync::Arc, time::Duration};

use semver::{Version, VersionReq};
use serde::{Deserialize, Deserializer};
use tokio::sync::oneshot;

use super::{results_match, ExpectedEvent, ObserveEvent, Operation};

use crate::{
    bson::{doc, Bson, Deserializer as BsonDeserializer, Document},
    client::options::{ServerApi, ServerApiVersion, SessionOptions},
    concern::{Acknowledgment, ReadConcernLevel},
    error::Error,
    options::{
        ClientOptions,
        CollectionOptions,
        DatabaseOptions,
        HedgedReadOptions,
        ReadConcern,
        ReadPreference,
        SelectionCriteria,
        WriteConcern,
    },
    test::{Serverless, TestClient, DEFAULT_URI},
};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TestFile {
    pub(crate) description: String,
    #[serde(deserialize_with = "deserialize_schema_version")]
    pub(crate) schema_version: Version,
    pub(crate) run_on_requirements: Option<Vec<RunOnRequirement>>,
    pub(crate) create_entities: Option<Vec<TestFileEntity>>,
    pub(crate) initial_data: Option<Vec<CollectionData>>,
    pub(crate) tests: Vec<TestCase>,
    // We don't need to use this field, but it needs to be included during deserialization so that
    // we can use the deny_unknown_fields tag.
    #[serde(rename = "_yamlAnchors")]
    _yaml_anchors: Option<Document>,
}

fn deserialize_schema_version<'de, D>(deserializer: D) -> std::result::Result<Version, D::Error>
where
    D: Deserializer<'de>,
{
    let mut schema_version = String::deserialize(deserializer)?;
    // If the schema version does not contain a minor or patch version, append as necessary to
    // ensure the String parses correctly into a semver::Version.
    let count = schema_version.split('.').count();
    if count == 1 {
        schema_version.push_str(".0.0");
    } else if count == 2 {
        schema_version.push_str(".0");
    }
    Version::parse(&schema_version).map_err(|e| serde::de::Error::custom(format!("{}", e)))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RunOnRequirement {
    min_server_version: Option<String>,
    max_server_version: Option<String>,
    topologies: Option<Vec<Topology>>,
    server_parameters: Option<Document>,
    serverless: Option<Serverless>,
    auth: Option<bool>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub(crate) enum Topology {
    Single,
    ReplicaSet,
    Sharded,
    #[serde(rename = "sharded-replicaset")]
    ShardedReplicaSet,
    #[serde(rename = "load-balanced")]
    LoadBalanced,
}

impl RunOnRequirement {
    pub(crate) async fn can_run_on(&self, client: &TestClient) -> bool {
        if let Some(ref min_version) = self.min_server_version {
            let req = VersionReq::parse(&format!(">= {}", &min_version)).unwrap();
            if !req.matches(&client.server_version) {
                return false;
            }
        }
        if let Some(ref max_version) = self.max_server_version {
            let req = VersionReq::parse(&format!("<= {}", &max_version)).unwrap();
            if !req.matches(&client.server_version) {
                return false;
            }
        }
        if let Some(ref topologies) = self.topologies {
            if !topologies.contains(&client.topology().await) {
                return false;
            }
        }
        if let Some(ref actual_server_parameters) = self.server_parameters {
            if results_match(
                Some(&Bson::Document(client.server_parameters.clone())),
                &Bson::Document(actual_server_parameters.clone()),
                false,
                None,
            )
            .is_err()
            {
                return false;
            }
        }
        if let Some(ref serverless) = self.serverless {
            if !serverless.can_run() {
                return false;
            }
        }
        if let Some(ref auth) = self.auth {
            if *auth != client.auth_enabled() {
                return false;
            }
        }
        true
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) enum TestFileEntity {
    Client(Client),
    Database(Database),
    Collection(Collection),
    Session(Session),
    Bucket(Bucket),
    Thread(Thread),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoreEventsAsEntity {
    pub id: String,
    pub events: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Client {
    pub(crate) id: String,
    pub(crate) uri_options: Option<Document>,
    pub(crate) use_multiple_mongoses: Option<bool>,
    pub(crate) observe_events: Option<Vec<ObserveEvent>>,
    pub(crate) ignore_command_monitoring_events: Option<Vec<String>>,
    #[serde(default)]
    pub(crate) observe_sensitive_commands: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_server_api_test_format")]
    pub(crate) server_api: Option<ServerApi>,
    pub(crate) store_events_as_entities: Option<Vec<StoreEventsAsEntity>>,
}

pub(crate) fn deserialize_server_api_test_format<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<ServerApi>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct ApiHelper {
        version: ServerApiVersion,
        strict: Option<bool>,
        deprecation_errors: Option<bool>,
    }

    let h = ApiHelper::deserialize(deserializer)?;
    Ok(Some(ServerApi {
        version: h.version,
        strict: h.strict,
        deprecation_errors: h.deprecation_errors,
    }))
}

pub(crate) fn merge_uri_options(given_uri: &str, uri_options: Option<&Document>) -> String {
    let uri_options = match uri_options {
        Some(opts) => opts,
        None => return given_uri.to_string(),
    };
    let mut given_uri_parts = given_uri.split('?');

    let mut uri = String::from(given_uri_parts.next().unwrap());
    // A connection string has two slashes before the host list and one slash before the auth db
    // name. If an auth db name is not provided the latter slash might not be present, so it needs
    // to be added manually.
    if uri.chars().filter(|c| *c == '/').count() < 3 {
        uri.push('/');
    }
    uri.push('?');

    if let Some(options) = given_uri_parts.next() {
        let options = options.split('&');
        for option in options {
            let key = option.split('=').next().unwrap();
            // The provided URI options should override any existing options in the connection
            // string.
            if !uri_options.contains_key(key) {
                uri.push_str(option);
                uri.push('&');
            }
        }
    }

    for (key, value) in uri_options {
        let value = value.to_string();
        // to_string() wraps quotations around Bson strings
        let value = value.trim_start_matches('\"').trim_end_matches('\"');
        uri.push_str(&format!("{}={}&", &key, value));
    }

    // remove the trailing '&' from the URI (or '?' if no options are present)
    uri.pop();

    uri
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Database {
    pub(crate) id: String,
    pub(crate) client: String,
    pub(crate) database_name: String,
    pub(crate) database_options: Option<CollectionOrDatabaseOptions>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Collection {
    pub(crate) id: String,
    pub(crate) database: String,
    pub(crate) collection_name: String,
    pub(crate) collection_options: Option<CollectionOrDatabaseOptions>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Session {
    pub(crate) id: String,
    pub(crate) client: String,
    pub(crate) session_options: Option<SessionOptions>,
}

// TODO: RUST-527 remove the unused annotation
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[allow(unused)]
pub(crate) struct Bucket {
    pub(crate) id: String,
    pub(crate) database: String,
    pub(crate) bucket_options: Option<Document>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Thread {
    pub(crate) id: String,
}

/// Messages used for communicating with test runner "threads".
#[derive(Debug)]
pub(crate) enum ThreadMessage {
    ExecuteOperation(Arc<Operation>),
    Stop(oneshot::Sender<()>),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CollectionOrDatabaseOptions {
    pub(crate) read_concern: Option<ReadConcern>,
    #[serde(rename = "readPreference")]
    pub(crate) selection_criteria: Option<SelectionCriteria>,
    pub(crate) write_concern: Option<WriteConcern>,
}

impl CollectionOrDatabaseOptions {
    pub(crate) fn as_database_options(&self) -> DatabaseOptions {
        DatabaseOptions {
            read_concern: self.read_concern.clone(),
            selection_criteria: self.selection_criteria.clone(),
            write_concern: self.write_concern.clone(),
        }
    }

    pub(crate) fn as_collection_options(&self) -> CollectionOptions {
        CollectionOptions {
            read_concern: self.read_concern.clone(),
            selection_criteria: self.selection_criteria.clone(),
            write_concern: self.write_concern.clone(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CollectionData {
    pub(crate) collection_name: String,
    pub(crate) database_name: String,
    pub(crate) documents: Vec<Document>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TestCase {
    pub(crate) description: String,
    pub(crate) run_on_requirements: Option<Vec<RunOnRequirement>>,
    pub(crate) skip_reason: Option<String>,
    pub(crate) operations: Vec<Operation>,
    pub(crate) expect_events: Option<Vec<ExpectedEvents>>,
    pub(crate) outcome: Option<Vec<CollectionData>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ExpectedEvents {
    pub(crate) client: String,
    pub(crate) events: Vec<ExpectedEvent>,
    pub(crate) event_type: Option<ExpectedEventType>,
    pub(crate) ignore_extra_events: Option<bool>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) enum ExpectedEventType {
    Command,
    Cmap,
    // TODO RUST-1055 Remove this when connection usage is serialized.
    #[serde(skip)]
    CmapWithoutConnectionReady,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) enum EventMatch {
    Exact,
    Prefix,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ExpectError {
    #[allow(unused)]
    pub(crate) is_error: Option<bool>,
    pub(crate) is_client_error: Option<bool>,
    pub(crate) error_contains: Option<String>,
    pub(crate) error_code: Option<i32>,
    pub(crate) error_code_name: Option<String>,
    pub(crate) error_labels_contain: Option<Vec<String>>,
    pub(crate) error_labels_omit: Option<Vec<String>>,
    pub(crate) expect_result: Option<Bson>,
}

impl ExpectError {
    pub(crate) fn verify_result(
        &self,
        error: &Error,
        description: impl AsRef<str>,
    ) -> std::result::Result<(), String> {
        let description = description.as_ref();

        if let Some(is_client_error) = self.is_client_error {
            if is_client_error != !error.is_server_error() {
                return Err(format!(
                    "{}: expected client error but got {:?}",
                    description, error
                ));
            }
        }
        if let Some(error_contains) = &self.error_contains {
            match &error.message() {
                Some(msg) if msg.contains(error_contains) => (),
                _ => {
                    return Err(format!(
                        "{}: \"{}\" should include message field",
                        description, error
                    ))
                }
            }
        }
        if let Some(error_code) = self.error_code {
            match &error.code() {
                Some(code) => {
                    if code != &error_code {
                        return Err(format!(
                            "{}: error code {} ({:?}) did not match expected error code {}",
                            description,
                            code,
                            error.code_name(),
                            error_code
                        ));
                    }
                }
                None => {
                    return Err(format!(
                        "{}: {:?} was expected to include code {} but had no code",
                        description, error, error_code
                    ))
                }
            }
        }

        if let Some(expected_code_name) = &self.error_code_name {
            match error.code_name() {
                Some(name) => {
                    if name != expected_code_name {
                        return Err(format!(
                            "{}: error code name \"{}\" did not match expected error code name \
                             \"{}\"",
                            description, name, expected_code_name,
                        ));
                    }
                }
                None => {
                    return Err(format!(
                        "{}: {:?} was expected to include code name \"{}\" but had no code name",
                        description, error, expected_code_name
                    ))
                }
            }
        }
        if let Some(error_labels_contain) = &self.error_labels_contain {
            for label in error_labels_contain {
                if !error.contains_label(label) {
                    return Err(format!(
                        "{}: expected {:?} to contain label \"{}\"",
                        description, error, label
                    ));
                }
            }
        }
        if let Some(error_labels_omit) = &self.error_labels_omit {
            for label in error_labels_omit {
                if error.contains_label(label) {
                    return Err(format!(
                        "{}: expected {:?} to omit label \"{}\"",
                        description, error, label
                    ));
                }
            }
        }
        if self.expect_result.is_some() {
            // TODO RUST-260: match against partial results
        }
        Ok(())
    }
}

#[cfg_attr(feature = "tokio-runtime", tokio::test)]
#[cfg_attr(feature = "async-std-runtime", async_std::test)]
async fn merged_uri_options() {
    let options = doc! {
        "ssl": true,
        "w": 2,
        "readconcernlevel": "local",
    };
    let uri = merge_uri_options(&DEFAULT_URI, Some(&options));
    let options = ClientOptions::parse_uri(&uri, None).await.unwrap();

    assert!(options.tls_options().is_some());

    let write_concern = WriteConcern::builder().w(Acknowledgment::from(2)).build();
    assert_eq!(options.write_concern.unwrap(), write_concern);

    let read_concern = ReadConcern::local();
    assert_eq!(options.read_concern.unwrap(), read_concern);
}

#[test]
fn deserialize_selection_criteria() {
    let read_preference = doc! {
        "mode": "SecondaryPreferred",
        "maxStalenessSeconds": 100,
        "hedge": { "enabled": true },
    };
    let d = BsonDeserializer::new(read_preference.into());
    let selection_criteria = SelectionCriteria::deserialize(d).unwrap();

    match selection_criteria {
        SelectionCriteria::ReadPreference(read_preference) => match read_preference {
            ReadPreference::SecondaryPreferred { options } => {
                assert_eq!(options.max_staleness, Some(Duration::from_secs(100)));
                assert_eq!(options.hedge, Some(HedgedReadOptions::with_enabled(true)));
            }
            other => panic!("Expected mode SecondaryPreferred, got {:?}", other),
        },
        SelectionCriteria::Predicate(_) => panic!("Expected read preference, got predicate"),
    }
}

#[test]
fn deserialize_read_concern() {
    let read_concern = doc! {
        "level": "local",
    };
    let d = BsonDeserializer::new(read_concern.into());
    let read_concern = ReadConcern::deserialize(d).unwrap();
    assert!(matches!(read_concern.level, ReadConcernLevel::Local));

    let read_concern = doc! {
        "level": "customlevel",
    };
    let d = BsonDeserializer::new(read_concern.into());
    let read_concern = ReadConcern::deserialize(d).unwrap();
    match read_concern.level {
        ReadConcernLevel::Custom(level) => assert_eq!(level.as_str(), "customlevel"),
        other => panic!("Expected custom read concern, got {:?}", other),
    };
}
