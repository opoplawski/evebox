// Copyright (C) 2020 Jason Ish
//
// Permission is hereby granted, free of charge, to any person obtaining
// a copy of this software and associated documentation files (the
// "Software"), to deal in the Software without restriction, including
// without limitation the rights to use, copy, modify, merge, publish,
// distribute, sublicense, and/or sell copies of the Software, and to
// permit persons to whom the Software is furnished to do so, subject to
// the following conditions:
//
// The above copyright notice and this permission notice shall be
// included in all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
// EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
// MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
// NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE
// LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
// OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION
// WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

use crate::prelude::*;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use warp::filters::BoxedFilter;
use warp::{self, Filter, Future};

use crate::bookmark;
use crate::datastore::Datastore;
use crate::elastic;
use crate::eve::filters::{AddRuleFilter, EveBoxMetadataFilter};
use crate::eve::processor::Processor;
use crate::eve::EveReader;
use crate::server::session::Session;
use crate::server::AuthenticationType;
use crate::settings::Settings;
use crate::sqlite;
use crate::sqlite::configrepo::ConfigRepo;

use super::{ServerConfig, ServerContext};
use warp::filters::log::{Info, Log};

fn load_event_services(filename: &str) -> anyhow::Result<serde_json::Value> {
    let finput = std::fs::File::open(filename)?;
    let yaml_value: serde_yaml::Value = serde_yaml::from_reader(finput)?;
    let json_value = serde_json::to_value(&yaml_value["event-services"])?;
    Ok(json_value)
}

pub async fn main(args: &clap::ArgMatches<'static>) -> Result<()> {
    crate::version::log_version();

    let config_filename = args.value_of("config");

    let mut settings = Settings::new(args);
    let mut config = ServerConfig::default();
    config.port = settings.get("http.port")?;
    config.host = settings.get("http.host")?;
    config.tls_enabled = settings.get_bool("http.tls.enabled")?;
    config.tls_cert_filename = settings.get_or_none("http.tls.certificate")?;
    config.tls_key_filename = settings.get_or_none("http.tls.key")?;
    config.datastore = settings.get("database.type")?;
    config.elastic_url = settings.get("database.elasticsearch.url")?;
    config.elastic_index = settings.get("database.elasticsearch.index")?;
    config.elastic_no_index_suffix = settings.get_bool("database.elasticsearch.no-index-suffix")?;
    config.elastic_ecs = settings.get_bool("database.elasticsearch.ecs")?;
    config.elastic_username = settings.get_or_none("database.elasticsearch.username")?;
    config.elastic_password = settings.get_or_none("database.elasticsearch.password")?;
    config.data_directory = settings.get_or_none("data-directory")?;
    config.database_retention_period = settings.get_or_none("database.retention-period")?;
    if let Ok(val) = settings.get_bool("database.elasticsearch.disable-certificate-check") {
        if val {
            config.no_check_certificate = true;
        } else {
            config.no_check_certificate = settings.get_bool("no-check-certificate")?;
        }
    }
    config.http_request_logging = settings.get_bool("http.request-logging")?;
    config.http_reverse_proxy = settings.get_bool("http.reverse-proxy")?;

    debug!(
        "Certificate checks disabled: {}",
        config.no_check_certificate,
    );

    config.authentication_required = settings.get_bool("authentication.required")?;
    if config.authentication_required {
        if let Some(auth_type) = settings.get_or_none::<String>("authentication.type")? {
            config.authentication_type = match auth_type.as_ref() {
                "username" => AuthenticationType::Username,
                "usernamepassword" => AuthenticationType::UsernamePassword,
                _ => {
                    return Err(anyhow!("Bad authentication type: {}", auth_type));
                }
            };
        }
    }

    // Do we need a data-directory? If so, make sure its set.
    let data_directory_required = config.datastore == "sqlite";

    if data_directory_required && config.data_directory.is_none() {
        error!("A data-directory is required");
        std::process::exit(1);
    }

    tokio::spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to register CTRL-C handler");
        std::process::exit(0);
    });

    let mut context = build_context(config.clone(), None).await?;

    if let Some(filename) = config_filename {
        match load_event_services(&filename) {
            Err(err) => {
                error!("Failed to load event-services: {}", err);
            }
            Ok(event_services) => {
                context.event_services = Some(event_services);
            }
        }
    }

    let input_enabled = {
        if settings.args.occurrences_of("input.filename") > 0 {
            true
        } else {
            settings.get_bool("input.enabled")?
        }
    };

    // This needs some cleanup. We load the input file names here, but configure
    // it later down.  Also, the filters (rules) are unlikely required if we
    // don't have an input enabled.
    let input_filenames = if input_enabled {
        let input_filename: Option<String> = settings.get_or_none("input.filename")?;
        let mut input_filenames = Vec::new();
        if let Some(input_filename) = &input_filename {
            for path in crate::path::expand(&input_filename)? {
                let path = path.display().to_string();
                input_filenames.push(path);
            }
        }
        input_filenames
    } else {
        Vec::new()
    };

    let mut shared_filters = Vec::new();

    match settings.get::<Vec<String>>("input.rules") {
        Ok(rules) => {
            let rulemap = crate::rules::load_rules(&rules);
            let rulemap = Arc::new(rulemap);
            shared_filters.push(crate::eve::filters::EveFilter::AddRuleFilter(
                AddRuleFilter {
                    map: rulemap.clone(),
                },
            ));
            crate::rules::watch_rules(rulemap);
        }
        Err(err) => match err {
            config::ConfigError::NotFound(_) => {}
            _ => {
                error!("Failed to read input.rules configuration: {}", err);
            }
        },
    }

    let shared_filters = Arc::new(shared_filters);

    for input_filename in &input_filenames {
        let end = settings.get_bool("end")?;
        let bookmark_directory: Option<String> =
            settings.get_or_none("input.bookmark-directory")?;
        let bookmark_filename = get_bookmark_filename(
            input_filename,
            bookmark_directory.as_deref(),
            config.data_directory.as_deref(),
        );
        info!(
            "Using bookmark filename {:?} for input {:?}",
            bookmark_filename, input_filename
        );

        let importer = if let Some(importer) = context.datastore.get_importer() {
            importer
        } else {
            error!("No importer implementation for this database.");
            std::process::exit(1);
        };

        let filters = vec![
            crate::eve::filters::EveFilter::Filters(shared_filters.clone()),
            EveBoxMetadataFilter {
                filename: Some(input_filename.clone()),
            }
            .into(),
        ];

        let reader = EveReader::new(input_filename);
        let mut processor = Processor::new(reader, importer.clone());
        processor.report_interval = Duration::from_secs(60);
        processor.filters = Arc::new(filters);
        processor.end = end;
        processor.bookmark_filename = bookmark_filename;
        info!("Starting reader for {}", input_filename);
        tokio::spawn(async move {
            processor.run().await;
        });
    }

    let context = Arc::new(context);
    let addr: SocketAddr = format!("{}:{}", config.host, config.port).parse()?;
    let server = build_server(&config, context.clone());

    info!(
        "Starting server on {}:{}, tls={}",
        config.host, config.port, config.tls_enabled
    );
    if config.tls_enabled {
        debug!("TLS key filename: {:?}", config.tls_key_filename);
        debug!("TLS cert filename: {:?}", config.tls_cert_filename);
        let cert_path = if let Some(filename) = config.tls_cert_filename {
            filename
        } else {
            error!("TLS requested but no certificate filename provided");
            std::process::exit(1);
        };
        let key_path = if let Some(filename) = config.tls_key_filename {
            filename
        } else {
            cert_path.clone()
        };
        server
            .tls()
            .cert_path(cert_path)
            .key_path(key_path)
            .run(addr)
            .await;
    } else {
        match server.try_bind_ephemeral(addr) {
            Err(err) => {
                error!("Failed to start server: {}", err);
                std::process::exit(1);
            }
            Ok((_, bound)) => {
                bound.await;
            }
        }
    }
    Ok(())
}

pub async fn build_server_try_bind(
    config: &ServerConfig,
    context: Arc<ServerContext>,
) -> Result<
    impl Future<Output = ()> + Send + Sync + 'static,
    Box<dyn std::error::Error + Sync + Send>,
> {
    let server = build_server(config, context);
    let addr: SocketAddr = format!("{}:{}", config.host, config.port).parse()?;
    let server = server.try_bind_ephemeral(addr)?.1;
    Ok(server)
}

pub fn build_server(
    config: &ServerConfig,
    context: Arc<ServerContext>,
) -> warp::Server<BoxedFilter<(impl warp::Reply,)>> {
    let session_filter = build_session_filter(context.clone()).boxed();
    let routes = super::filters::api_routes(context, session_filter)
        .or(resource_filters())
        .recover(super::rejection::rejection_handler);
    let mut headers = warp::http::header::HeaderMap::new();
    headers.insert(
        "X-EveBox-Git-Revision",
        warp::http::header::HeaderValue::from_static(crate::version::build_rev()),
    );

    let routes = routes.with(warp::reply::with::headers(headers));
    let http_log =
        build_http_request_logger(config.http_request_logging, config.http_reverse_proxy);

    let routes = routes.with(http_log);
    warp::serve(routes.boxed())
}

fn build_http_request_logger(enabled: bool, reverse_proxy: bool) -> Log<impl Fn(Info) + Clone> {
    warp::log::custom(move |info| {
        if enabled {
            http_request_logger(info, reverse_proxy);
        }
    })
}

fn http_request_logger(info: Info, reverse_proxy: bool) {
    let mut xff = None;
    if reverse_proxy {
        for (header, value) in info.request_headers() {
            if header == "x-forwarded-for" {
                // First convert value to a string, which could fail, then take the first
                // address from the comma seaparated list of addresses.
                if let Ok(val) = value.to_str() {
                    if let Some(val) = val.split(',').next() {
                        xff = Some(val.trim().to_string());
                        break;
                    }
                }
            }
        }
    }

    let remote_addr = get_remote_addr(info.remote_addr(), xff, reverse_proxy);

    info!(
        "{} {:?} {} {} {}",
        remote_addr,
        info.version(),
        info.status(),
        info.method(),
        info.path()
    );
}

pub async fn build_context(
    config: ServerConfig,
    datastore: Option<Datastore>,
) -> anyhow::Result<ServerContext> {
    let config_repo = if let Some(directory) = &config.data_directory {
        let filename = PathBuf::from(directory).join("config.sqlite");
        info!("Configuration database filename: {:?}", filename);
        ConfigRepo::new(Some(&filename))?
    } else {
        info!("Using temporary in-memory configuration database");
        ConfigRepo::new(None)?
    };
    let mut context = ServerContext::new(config, Arc::new(config_repo));
    if let Some(datastore) = datastore {
        context.datastore = datastore;
    } else {
        configure_datastore(&mut context).await?;
    }
    Ok(context)
}

async fn configure_datastore(context: &mut ServerContext) -> anyhow::Result<()> {
    let config = &context.config;
    match config.datastore.as_ref() {
        "elasticsearch" => {
            let mut client = elastic::ClientBuilder::new(&config.elastic_url);
            if let Some(username) = &config.elastic_username {
                client.with_username(username);
            }
            if let Some(password) = &config.elastic_password {
                client.with_password(password);
            }
            client.disable_certificate_validation(config.no_check_certificate);

            let client = client.build();

            match client.get_version().await {
                Err(err) => {
                    error!(
                        "Failed to get Elasticsearch version, things may not work right: error={}",
                        err
                    );
                }
                Ok(version) => {
                    if version.major < 6 {
                        return Err(anyhow!(
                            "Elasticsearch versions less than 6 are not supported"
                        ));
                    }
                    info!(
                        "Found Elasticsearch version {} at {}",
                        version.version, &config.elastic_url
                    );
                }
            }

            let index_pattern = if config.elastic_no_index_suffix {
                config.elastic_index.clone()
            } else {
                format!("{}-*", config.elastic_index)
            };

            let eventstore = elastic::EventStore {
                base_index: config.elastic_index.clone(),
                index_pattern: index_pattern,
                client: client,
                ecs: config.elastic_ecs,
                no_index_suffix: config.elastic_no_index_suffix,
            };
            debug!("Elasticsearch base index: {}", &eventstore.base_index);
            debug!(
                "Elasticsearch search index pattern: {}",
                &eventstore.index_pattern
            );
            debug!("Elasticsearch ECS mode: {}", eventstore.ecs);
            context.features.reporting = true;
            context.features.comments = true;
            context.elastic = Some(eventstore.clone());
            context.datastore = Datastore::Elastic(eventstore);
        }
        "sqlite" => {
            let db_filename = if let Some(dir) = &config.data_directory {
                std::path::PathBuf::from(dir).join("events.sqlite")
            } else if let Some(filename) = &config.sqlite_filename {
                std::path::PathBuf::from(filename)
            } else {
                panic!("data-directory required");
            };
            let connection_builder = sqlite::ConnectionBuilder {
                filename: Some(db_filename),
            };
            let mut connection = connection_builder.open().unwrap();
            sqlite::init_event_db(&mut connection).unwrap();
            let connection = Arc::new(Mutex::new(connection));

            let eventstore = sqlite::eventstore::SQLiteEventStore {
                connection: connection.clone(),
                importer: sqlite::importer::Importer::new(connection.clone()),
            };
            context.datastore = Datastore::SQLite(eventstore);

            // Setup retention job.
            if let Some(period) = config.database_retention_period {
                if period > 0 {
                    info!("Setting data retention period to {} days", period);
                    let retention_config = sqlite::retention::RetentionConfig { days: period };
                    let connection = connection;
                    tokio::task::spawn_blocking(|| {
                        sqlite::retention::retention_task(retention_config, connection);
                    });
                }
            }
        }
        _ => panic!("unsupported datastore"),
    }
    Ok(())
}

pub fn resource_filters(
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    let index = warp::get()
        .and(warp::path::end())
        .map(|| super::asset::new_static_or_404("index.html"));
    let favicon = warp::get()
        .and(warp::path("favicon.ico"))
        .and(warp::path::end())
        .map(|| super::asset::new_static_or_404("favicon.ico"));
    let public = warp::get()
        .and(warp::path("public"))
        .and(warp::path::tail())
        .map(|path: warp::filters::path::Tail| super::asset::new_static_or_404(path.as_str()));
    return index.or(favicon).or(public);
}

#[derive(Debug)]
pub enum GenericError {
    NotFound,
    AuthenticationRequired,
}

impl warp::reject::Reject for GenericError {}

pub fn get_remote_addr(
    socket_addr: Option<SocketAddr>,
    xff: Option<String>,
    enable_xff: bool,
) -> String {
    if enable_xff {
        if let Some(xff) = xff {
            return xff.split(',').next().unwrap().trim().to_string();
        }
    }
    if let Some(addr) = socket_addr {
        return addr.ip().to_string();
    }
    return "<unknown-remote>".to_string();
}

pub fn build_session_filter(
    context: Arc<ServerContext>,
) -> impl Filter<Extract = (Arc<Session>,), Error = warp::Rejection> + Clone {
    let enable_reverse_proxy = context.config.http_reverse_proxy;
    let context = warp::any().map(move || context.clone());

    let session_id = warp::header("x-evebox-session-id")
        .map(Some)
        .or(warp::any().map(|| None))
        .unify();

    let remote_user = warp::header("REMOTE_USER")
        .map(Some)
        .or(warp::any().map(|| None))
        .unify();

    let remote_addr = warp::filters::addr::remote()
        .and(warp::filters::header::optional("x-forwarded-for"))
        .map(move |a, b| get_remote_addr(a, b, enable_reverse_proxy));

    warp::any()
        .and(session_id)
        .and(context)
        .and(remote_user)
        .and(remote_addr)
        .and_then(
            move |session_id: Option<String>,
                  context: Arc<ServerContext>,
                  remote_user: Option<String>,
                  remote_addr: String| async move {
                if let Some(session_id) = session_id {
                    let session = context.session_store.get(&session_id);
                    if let Some(session) = session {
                        return Ok(session);
                    }
                }

                match context.config.authentication_type {
                    AuthenticationType::Anonymous => {
                        let username = if let Some(username) = remote_user {
                            username
                        } else {
                            "<anonymous>".to_string()
                        };
                        info!(
                            "Creating anonymous session for user from {} with name {}",
                            remote_addr, username
                        );
                        let mut session = Session::new();
                        session.username = Some(username);
                        let session = Arc::new(session);
                        context.session_store.put(session.clone()).unwrap();
                        Ok::<_, warp::Rejection>(session)
                    }
                    _ => Err::<_, warp::Rejection>(warp::reject::custom(
                        GenericError::AuthenticationRequired,
                    )),
                }
            },
        )
}

fn get_bookmark_filename(
    input_filename: &str,
    input_bookmark_dir: Option<&str>,
    data_directory: Option<&str>,
) -> Option<PathBuf> {
    // First priority is the input_bookmark_directory.
    if let Some(directory) = input_bookmark_dir {
        return Some(bookmark::bookmark_filename(input_filename, directory));
    }

    // Otherwise see if there is a file with the same name as the input filename but
    // suffixed with ".bookmark".
    let legacy_filename = format!("{}.bookmark", input_filename);
    if let Ok(_meta) = std::fs::metadata(&legacy_filename) {
        warn!(
            "Found legacy bookmark file, checking if writable: {}",
            &legacy_filename
        );
        match test_writable(&legacy_filename) {
            Ok(_) => {
                warn!("Using legacy bookmark filename: {}", &legacy_filename);
                return Some(PathBuf::from(&legacy_filename));
            }
            Err(err) => {
                error!(
                    "Legacy bookmark filename not writable, will not use: filename={}, error={}",
                    legacy_filename, err
                );
            }
        }
    }

    // Do we have a global data-directory, and is it writable?
    if let Some(directory) = data_directory {
        let bookmark_filename = bookmark::bookmark_filename(input_filename, directory);
        debug!("Checking {:?} for writability", &bookmark_filename);
        if let Err(err) = test_writable(&bookmark_filename) {
            error!("{:?} not writable: {}", &bookmark_filename, err);
        } else {
            return Some(bookmark_filename);
        }
    }

    // All that failed, check the current directory.
    let bookmark_filename = bookmark::bookmark_filename(input_filename, ".");
    if test_writable(&bookmark_filename).is_ok() {
        return Some(bookmark_filename);
    }

    None
}

fn test_writable<T: AsRef<Path>>(filename: T) -> anyhow::Result<()> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(filename)?;
    Ok(())
}
