use crate::executable::Opt;
use crate::plugin::DynPlugin;
use crate::router_factory::SupergraphServiceConfigurator;
use crate::spec::Schema;
use crate::Configuration;
use async_trait::async_trait;
use clap::CommandFactory;
use http::header::USER_AGENT;
use lazy_static::lazy_static;
use serde::Serialize;
use serde_json::{Map, Value};
use std::sync::Arc;
use std::{env, thread};
use tower::BoxError;
use uuid::Uuid;

lazy_static! {
    /// This session id is created once when the router starts. It persists between config reloads.
    static ref SESSION_ID: Uuid = Uuid::new_v4();
}

/// Platform represents the platform the CLI is being run from
#[derive(Debug, Serialize)]
struct Platform {
    /// the platform from which the command was run (i.e. linux, macOS, windows or even wsl)
    os: String,

    /// if we think this command is being run in CI
    continuous_integration: Option<ci_info::types::Vendor>,
}

/// Platform represents the platform the CLI is being run from
#[derive(Debug, Serialize)]
struct UsageReport {
    /// A random ID that is generated on first startup of the Router. It is persistent between restarts. This is best effort and not guaranteed to be populated
    machine_id: Uuid,
    /// A random ID that is generated on first startup of the Router. It is not persistent between restarts of the Router, but will be persistent for hot reloads
    session_id: Uuid,
    /// The version of the Router
    version: String,
    /// Information about the current architecture/platform
    platform: Platform,
    /// Information about what was being used
    usage: Map<String, serde_json::Value>,
}

/// A service factory that will report some anonymous telemetry to Apollo. It can be disabled by users, but the data is useful for helping us to decide where to spend our efforts.
/// In future we should try and move this towards otel metrics, this will allow us to send the information direct to something that ingests OTLP.
/// The data sent looks something like this:
/// ```json
/// {
///   "machine_id": "57c2a779-2ae3-476b-a712-122b9f0f19b6",
///   "session_id": "fbe09da3-ebdb-4863-8086-feb97464b8d7",
///   "version": "1.4.0",
///   "platform": {
///     "os": "linux",
///     "continuous_integration": null
///   },
///   "usage": {
///     "configuration.headers.all.request.propagate.named": "set",
///     "configuration.headers.all.request.insert.name": "set",
///     "configuration.headers.all.request.insert.value": "set",
///     "configuration.headers.all.request.len": 4,
///     "args.config-path": "set",
///     "args.apollo-key": "set",
///     "args.apollo-graph-ref": "set"
///   }
/// }
/// ```
#[derive(Default)]
pub(crate) struct OrbiterAbstractRouterServiceFactory<T: SupergraphServiceConfigurator> {
    delegate: T,
}

impl<T: SupergraphServiceConfigurator> OrbiterAbstractRouterServiceFactory<T> {
    pub(crate) fn new(delegate: T) -> OrbiterAbstractRouterServiceFactory<T> {
        OrbiterAbstractRouterServiceFactory { delegate }
    }
}

#[async_trait]
impl<T: SupergraphServiceConfigurator> SupergraphServiceConfigurator
    for OrbiterAbstractRouterServiceFactory<T>
{
    type SupergraphServiceFactory = T::SupergraphServiceFactory;

    async fn create<'a>(
        &'a mut self,
        configuration: Arc<Configuration>,
        schema: Arc<Schema>,
        previous_router: Option<&'a Self::SupergraphServiceFactory>,
        extra_plugins: Option<Vec<(String, Box<dyn DynPlugin>)>>,
    ) -> Result<Self::SupergraphServiceFactory, BoxError> {
        self.delegate
            .create(
                configuration.clone(),
                schema.clone(),
                previous_router,
                extra_plugins,
            )
            .await
            .map(|factory| {
                if env::var("APOLLO_TELEMETRY_DISABLED").unwrap_or_default() != "true" {
                    thread::spawn(|| {
                        tracing::debug!("sending anonymous usage data to Apollo");
                        send_anonymous_metrics_to_orbiter(configuration, schema);
                    });
                }
                factory
            })
    }
}

fn send_anonymous_metrics_to_orbiter(configuration: Arc<Configuration>, _schema: Arc<Schema>) {
    let machine_id = get_machine_id();
    let os = get_os();
    let mut usage = serde_json::Map::new();
    // We only report apollo plugins. This way we don't risk leaking sensitive data if the user has customized the router and added their own plugins.
    // In addition, we only report the shape of the configuration
    for (name, config) in &configuration.apollo_plugins.plugins {
        visit_config(&mut usage, name, config);
    }

    // Check the command line options. This encapsulates both env and command line functionality
    let matches = Opt::command().get_matches();
    Opt::command().get_arguments().for_each(|a| {
        // This logic took a lot of trial and error to figure out.
        // If there are no defaults then the setting of the arg itself is enough for us to record it.
        // If there are defaults then only record if the arg differed from the default
        let defaults = a.get_default_values().to_vec();
        if let Some(values) = matches.get_raw(a.get_id()) {
            let values = values.collect::<Vec<_>>();
            if defaults != values {
                usage.insert(
                    format!("args.{}", a.get_id()),
                    Value::String("set".to_string()),
                );
            } else if defaults.is_empty() {
                usage.insert(format!("args.{}", a.get_id()), Value::Bool(true));
            }
        }
    });

    let body = UsageReport {
        machine_id,
        session_id: *SESSION_ID,
        version: std::env!("CARGO_PKG_VERSION").to_string(),
        platform: Platform {
            os,
            continuous_integration: ci_info::get().vendor,
        },
        usage,
    };

    if let Err(e) = send(body) {
        tracing::debug!("failed to send anonymous usage: {}", e);
    }
}

fn send(body: UsageReport) -> Result<String, BoxError> {
    tracing::debug!("anonymous usage: {}", serde_json::to_string_pretty(&body)?);

    Ok(reqwest::blocking::Client::new()
        .post("http://localhost:8888/telemetry")
        .header(USER_AGENT, "router")
        .json(&serde_json::to_value(body)?)
        .send()?
        .text()?)
}

fn get_os() -> String {
    if wsl::is_wsl() {
        "wsl"
    } else {
        std::env::consts::OS
    }
    .to_string()
}

fn get_machine_id() -> Uuid {
    let mut file = std::env::temp_dir();
    file.push(".apollo_router_machine_id");
    if file.exists() {
        std::fs::read_to_string(&file)
            .unwrap_or_default()
            .parse()
            .unwrap_or_else(|_| Uuid::new_v4())
    } else {
        let id = Uuid::new_v4();
        if let Err(err) = std::fs::write(&file, &id.to_string()) {
            tracing::debug!("unable to write anonymous machine id {}", err);
        }
        id
    }
}

fn visit_config(usage: &mut Map<String, Value>, path: &str, value: &Value) {
    match value {
        Value::Null => {}
        Value::Bool(value) => {
            usage.insert(format!("configuration.{}", path), Value::Bool(*value));
        }
        Value::Number(value) => {
            usage.insert(
                format!("configuration.{}", path),
                Value::Number(value.clone()),
            );
        }
        Value::String(_) => {
            usage.insert(
                format!("configuration.{}", path),
                Value::String("set".to_string()),
            );
        }
        Value::Array(a) => {
            // We don't care about arrays really, but it's useful to output the length.
            for v in a {
                visit_config(usage, path, v);
            }
            usage.insert(format!("configuration.{}.len", path), a.len().into());
        }
        Value::Object(o) => {
            for (k, v) in o {
                visit_config(usage, &format!("{}.{}", path, k), v);
            }
        }
    }
}

#[cfg(test)]
mod test {
    use crate::orbiter::visit_config;
    use insta::assert_yaml_snapshot;
    use serde_json::json;

    #[test]
    fn test_visit_config() {
        let source = json!({
            "string_value": "b",
            "bool_value": true,
            "numeric_value": 4,
            "nested_obj": {
                "string_value": "b",
                "bool_value": true,
                "numeric_value": 4
            },
            "nested_arr": [
                {
                    "string_value": "b",
                    "bool_value": true,
                    "numeric_value": 4
                },
                {
                    "string_value": "b",
                    "bool_value": true,
                    "numeric_value": 4
                }
            ]
        });
        let mut usage = serde_json::Map::new();
        visit_config(&mut usage, "root", &source);
        insta::with_settings!({sort_maps => true}, {
            assert_yaml_snapshot!(usage);
        });
    }
}
