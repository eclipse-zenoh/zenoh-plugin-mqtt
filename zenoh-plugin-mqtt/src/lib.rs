//
// Copyright (c) 2017, 2024 ZettaScale Technology
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ZettaScale Zenoh Team, <zenoh@zettascale.tech>
//
use std::{
    collections::HashMap,
    env,
    future::Future,
    io::BufReader,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use ntex::{
    io::IoBoxed,
    service::{chain_factory, fn_factory_with_config, fn_service},
    time::Deadline,
    util::{ByteString, Bytes, Ready},
    ServiceFactory,
};
use ntex_mqtt::{v3, v5, MqttError, MqttServer};
use ntex_tls::rustls::Acceptor;
use rustls::{
    server::AllowAnyAuthenticatedClient, Certificate, PrivateKey, RootCertStore, ServerConfig,
};
use secrecy::ExposeSecret;
use serde_json::Value;
use tokio::task::JoinHandle;
use zenoh::{
    bytes::{Encoding, ZBytes},
    internal::{
        plugins::{RunningPluginTrait, ZenohPlugin},
        runtime::Runtime,
        zerror,
    },
    key_expr::keyexpr,
    prelude::*,
    query::Query,
    try_init_log_from_env, Result as ZResult, Session,
};
use zenoh_plugin_trait::{plugin_long_version, plugin_version, Plugin, PluginControl};

pub mod config;
mod mqtt_helpers;
mod mqtt_session_state;
use config::{AuthConfig, Config, TLSConfig};
use mqtt_session_state::MqttSessionState;

lazy_static::lazy_static! {
    static ref WORK_THREAD_NUM: AtomicUsize = AtomicUsize::new(config::DEFAULT_WORK_THREAD_NUM);
    static ref MAX_BLOCK_THREAD_NUM: AtomicUsize = AtomicUsize::new(config::DEFAULT_MAX_BLOCK_THREAD_NUM);
    // The global runtime is used in the dynamic plugins, which we can't get the current runtime
    static ref TOKIO_RUNTIME: tokio::runtime::Runtime = tokio::runtime::Builder::new_multi_thread()
               .worker_threads(WORK_THREAD_NUM.load(Ordering::SeqCst))
               .max_blocking_threads(MAX_BLOCK_THREAD_NUM.load(Ordering::SeqCst))
               .enable_all()
               .build()
               .expect("Unable to create runtime");
}
#[inline(always)]
pub(crate) fn spawn_runtime<F>(task: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    // Check whether able to get the current runtime
    match tokio::runtime::Handle::try_current() {
        Ok(rt) => {
            // Able to get the current runtime (standalone binary), spawn on the current runtime
            rt.spawn(task)
        }
        Err(_) => {
            // Unable to get the current runtime (dynamic plugins), spawn on the global runtime
            TOKIO_RUNTIME.spawn(task)
        }
    }
}

lazy_static::lazy_static! {
    static ref KE_PREFIX_ADMIN_SPACE: &'static keyexpr = unsafe { keyexpr::from_str_unchecked("@") };
    static ref KE_PREFIX_MQTT: &'static keyexpr = unsafe { keyexpr::from_str_unchecked("mqtt") };
    static ref ADMIN_SPACE_KE_VERSION: &'static keyexpr = unsafe { keyexpr::from_str_unchecked("version") };
    static ref ADMIN_SPACE_KE_CONFIG: &'static keyexpr = unsafe { keyexpr::from_str_unchecked("config") };
}

#[cfg(feature = "dynamic_plugin")]
zenoh_plugin_trait::declare_plugin!(MqttPlugin);

pub struct MqttPlugin;

// Authentication types
type User = Vec<u8>;
type Password = Vec<u8>;

impl ZenohPlugin for MqttPlugin {}
impl Plugin for MqttPlugin {
    type StartArgs = Runtime;
    type Instance = zenoh::internal::plugins::RunningPlugin;

    const DEFAULT_NAME: &'static str = "mqtt";
    const PLUGIN_LONG_VERSION: &'static str = plugin_long_version!();
    const PLUGIN_VERSION: &'static str = plugin_version!();

    fn start(
        name: &str,
        runtime: &Self::StartArgs,
    ) -> ZResult<zenoh::internal::plugins::RunningPlugin> {
        // Try to initiate login.
        // Required in case of dynamic lib, otherwise no logs.
        // But cannot be done twice in case of static link.
        try_init_log_from_env();

        let runtime_conf = runtime.config().lock();
        let plugin_conf = runtime_conf
            .plugin(name)
            .ok_or_else(|| zerror!("Plugin `{}`: missing config", name))?;
        let config: Config = serde_json::from_value(plugin_conf.clone())
            .map_err(|e| zerror!("Plugin `{}` configuration error: {}", name, e))?;

        let tls_config = match config.tls.as_ref() {
            Some(tls) => Some(create_tls_config(tls)?),
            None => None,
        };

        let auth_dictionary = match config.auth.as_ref() {
            Some(auth) => Some(create_auth_dictionary(auth)?),
            None => None,
        };
        WORK_THREAD_NUM.store(config.work_thread_num, Ordering::SeqCst);
        MAX_BLOCK_THREAD_NUM.store(config.max_block_thread_num, Ordering::SeqCst);

        spawn_runtime(run(runtime.clone(), config, tls_config, auth_dictionary));

        Ok(Box::new(MqttPlugin))
    }
}

impl PluginControl for MqttPlugin {}
impl RunningPluginTrait for MqttPlugin {}

async fn run(
    runtime: Runtime,
    config: Config,
    tls_config: Option<Arc<ServerConfig>>,
    auth_dictionary: Option<HashMap<User, Password>>,
) {
    // Try to initiate login.
    // Required in case of dynamic lib, otherwise no logs.
    // But cannot be done twice in case of static link.
    try_init_log_from_env();
    tracing::debug!("MQTT plugin {}", MqttPlugin::PLUGIN_LONG_VERSION);
    tracing::info!("MQTT plugin {:?}", config);

    // init Zenoh Session with provided Runtime
    let zsession = match zenoh::session::init(runtime)
        .aggregated_subscribers(config.generalise_subs.clone())
        .aggregated_publishers(config.generalise_pubs.clone())
        .await
    {
        Ok(session) => Arc::new(session),
        Err(e) => {
            tracing::error!("Unable to init zenoh session for MQTT plugin : {:?}", e);
            return;
        }
    };

    // declare admin space queryable
    let admin_keyexpr_prefix =
        *KE_PREFIX_ADMIN_SPACE / &zsession.zid().into_keyexpr() / *KE_PREFIX_MQTT;
    let admin_keyexpr_expr = (&admin_keyexpr_prefix) / unsafe { keyexpr::from_str_unchecked("**") };
    tracing::debug!("Declare admin space on {}", admin_keyexpr_expr);
    let config2 = config.clone();
    let _admin_queryable = zsession
        .declare_queryable(admin_keyexpr_expr)
        .callback(move |query| treat_admin_query(query, &admin_keyexpr_prefix, &config2))
        .await
        .expect("Failed to create AdminSpace queryable");

    if auth_dictionary.is_some() && tls_config.is_none() {
        tracing::warn!(
            "Warning: MQTT client username/password authentication enabled without TLS!"
        );
    }

    // Start MQTT Server task
    let config = Arc::new(config);
    let auth_dictionary = Arc::new(auth_dictionary);

    // The future inside the `run_local` is !SEND, so we can't spawn it directly in tokio runtime.
    // Therefore, we dedicate a blocking thread to `block_on` ntex server.
    // Note that we can't use the ntex `block_on` because it doesn't support multi-threaded runtime, which is necessary for Zenoh.
    tokio::task::spawn_blocking(|| {
        let rt = tokio::runtime::Handle::try_current()
            .expect("Unable to get the current runtime, which should not happen.");
        rt.block_on(
            ntex::rt::System::new(MqttPlugin::DEFAULT_NAME).run_local(async move {
                let server = match tls_config {
                    Some(tls) => ntex::server::Server::build().bind(
                        "mqtt",
                        config.port.clone(),
                        move |_| {
                            chain_factory(Acceptor::new(tls.clone()))
                                .map_err(|err| MqttError::Service(MqttPluginError::from(err)))
                                .and_then(create_mqtt_server(
                                    zsession.clone(),
                                    config.clone(),
                                    auth_dictionary.clone(),
                                ))
                        },
                    )?,
                    None => ntex::server::Server::build().bind(
                        "mqtt",
                        config.port.clone(),
                        move |_| {
                            create_mqtt_server(
                                zsession.clone(),
                                config.clone(),
                                auth_dictionary.clone(),
                            )
                        },
                    )?,
                };
                // Disable catching the signal inside the ntex, or we can't stop the plugin.
                server.workers(1).disable_signals().run().await
            }),
        )
        .unwrap();
    });
}

fn create_tls_config(config: &TLSConfig) -> ZResult<Arc<ServerConfig>> {
    let key_bytes = match (
        config.server_private_key.as_ref(),
        config.server_private_key_base64.as_ref(),
    ) {
        (Some(file), None) => {
            std::fs::read(file).map_err(|e| zerror!("Invalid private key file: {e:?}"))?
        }
        (None, Some(base64)) => base64_decode(base64.expose_secret())?,
        (None, None) => {
            return Err(zerror!(
                "Either 'server_private_key' or 'server_private_key_base64' must be present!"
            )
            .into());
        }
        _ => {
            return Err(zerror!(
                "Only one of 'server_private_key' and 'server_private_key_base64' can be present!"
            )
            .into());
        }
    };
    let key = load_private_key(key_bytes)?;

    let certs_bytes = match (
        config.server_certificate.as_ref(),
        config.server_certificate_base64.as_ref(),
    ) {
        (Some(file), None) => {
            std::fs::read(file).map_err(|e| zerror!("Invalid certificate file: {e:?}"))?
        }
        (None, Some(base64)) => base64_decode(base64.expose_secret())?,
        (None, None) => {
            return Err(zerror!(
                "Either 'server_certificate' or 'server_certificate_base64' must be present!"
            )
            .into());
        }
        _ => {
            return Err(zerror!(
                "Only one of 'server_certificate' and 'server_certificate_base64' can be present!"
            )
            .into());
        }
    };
    let certs = load_certs(certs_bytes)?;

    // Providing a root CA certificate is optional - when provided clients will be verified
    let rootca_bytes = match (
        config.root_ca_certificate.as_ref(),
        config.root_ca_certificate_base64.as_ref(),
    ) {
        (Some(file), None) => {
            Some(std::fs::read(file).map_err(|e| zerror!("Invalid root certificate file: {e:?}"))?)
        }
        (None, Some(base64)) => Some(base64_decode(base64.expose_secret())?),
        (None, None) => None,
        _ => {
            return Err(zerror!("Only one of 'root_ca_certificate' and 'root_ca_certificate_base64' can be present!").into());
        }
    };

    let tls_config = match rootca_bytes {
        Some(bytes) => {
            let root_cert_store = load_trust_anchors(bytes)?;

            ServerConfig::builder()
                .with_safe_defaults()
                .with_client_cert_verifier(Arc::new(AllowAnyAuthenticatedClient::new(
                    root_cert_store,
                )))
                .with_single_cert(certs, key)?
        }
        None => ServerConfig::builder()
            .with_safe_defaults()
            .with_no_client_auth()
            .with_single_cert(certs, key)?,
    };
    Ok(Arc::new(tls_config))
}

pub fn base64_decode(data: &str) -> ZResult<Vec<u8>> {
    use base64::{engine::general_purpose, Engine};
    Ok(general_purpose::STANDARD
        .decode(data)
        .map_err(|e| zerror!("Unable to perform base64 decoding: {e:?}"))?)
}

fn load_private_key(bytes: Vec<u8>) -> ZResult<PrivateKey> {
    let mut reader = BufReader::new(bytes.as_slice());

    loop {
        match rustls_pemfile::read_one(&mut reader) {
            Ok(item) => match item {
                Some(rustls_pemfile::Item::Pkcs1Key(key)) => {
                    return Ok(rustls::PrivateKey(key.secret_pkcs1_der().to_vec()))
                }
                Some(rustls_pemfile::Item::Pkcs8Key(key)) => {
                    return Ok(rustls::PrivateKey(key.secret_pkcs8_der().to_vec()))
                }
                Some(rustls_pemfile::Item::Sec1Key(key)) => {
                    return Ok(rustls::PrivateKey(key.secret_sec1_der().to_vec()))
                }
                None => break,
                _ => continue,
            },
            Err(e) => return Err(zerror!("Cannot parse private key: {e:?}").into()),
        }
    }
    Err(zerror!("No supported private keys found").into())
}

fn load_certs(bytes: Vec<u8>) -> ZResult<Vec<Certificate>> {
    let mut reader = BufReader::new(bytes.as_slice());

    let certs: Vec<Certificate> = rustls_pemfile::certs(&mut reader)
        .map(|c| c.map(|c| Certificate(c.to_vec())))
        .collect::<Result<_, _>>()
        .map_err(|err| zerror!("Error processing client certificate: {err}."))?;

    match certs.is_empty() {
        true => Err(zerror!("No certificates found").into()),
        false => Ok(certs),
    }
}

fn load_trust_anchors(bytes: Vec<u8>) -> ZResult<RootCertStore> {
    let mut root_cert_store = RootCertStore::empty();
    let roots = load_certs(bytes)?;
    for root in roots {
        root_cert_store.add(&root)?;
    }
    Ok(root_cert_store)
}

fn create_auth_dictionary(config: &AuthConfig) -> ZResult<HashMap<User, Password>> {
    let mut dictionary: HashMap<User, Password> = HashMap::new();
    let content = std::fs::read_to_string(config.dictionary_file.as_str())
        .map_err(|e| zerror!("Invalid user/password dictionary file: {}", e))?;

    // Populate the user/password dictionary
    // The dictionary file is expected to be in the form of:
    //      usr1:pwd1
    //      usr2:pwd2
    //      usr3:pwd3
    for line in content.lines() {
        let idx = line
            .find(':')
            .ok_or_else(|| zerror!("Invalid user/password dictionary file: invalid format"))?;
        let user = line[..idx].as_bytes().to_owned();
        if user.is_empty() {
            return Err(zerror!("Invalid user/password dictionary file: empty user").into());
        }
        let password = line[idx + 1..].as_bytes().to_owned();
        if password.is_empty() {
            return Err(zerror!("Invalid user/password dictionary file: empty password").into());
        }
        dictionary.insert(user, password);
    }
    Ok(dictionary)
}

fn is_authorized(
    dictionary: Option<&HashMap<User, Password>>,
    usr: Option<&ByteString>,
    pwd: Option<&Bytes>,
) -> Result<(), String> {
    match (dictionary, usr, pwd) {
        // No user/password dictionary - all clients authorized to connect
        (None, _, _) => Ok(()),
        // User/password dictionary provided - clients must provide credentials to connect
        (Some(dictionary), Some(usr), Some(pwd)) => {
            match dictionary.get(&usr.as_bytes().to_vec()) {
                Some(expected_pwd) => {
                    if pwd == expected_pwd {
                        Ok(())
                    } else {
                        Err(format!("Incorrect password for user {usr:?}"))
                    }
                }
                None => Err(format!("Unknown user {usr:?}")),
            }
        }
        (Some(_), Some(usr), None) => Err(format!("Missing password for user {usr:?}")),
        (Some(_), None, Some(_)) => Err(("Missing user name").to_string()),
        (Some(_), None, None) => Err(("Missing user credentials").to_string()),
    }
}

fn create_mqtt_server(
    session: Arc<Session>,
    config: Arc<Config>,
    auth_dictionary: Arc<Option<HashMap<User, Password>>>,
) -> MqttServer<
    impl ServiceFactory<
        (IoBoxed, Deadline),
        (),
        Response = (),
        Error = MqttError<MqttPluginError>,
        InitError = (),
    >,
    impl ServiceFactory<
        (IoBoxed, Deadline),
        (),
        Response = (),
        Error = MqttError<MqttPluginError>,
        InitError = (),
    >,
    MqttPluginError,
    (),
> {
    let zs_v3 = session.clone();
    let zs_v5 = session.clone();
    let config_v3 = config.clone();
    let config_v5 = config.clone();
    let auth_dictionary_v3 = auth_dictionary.clone();
    let auth_dictionary_v5 = auth_dictionary.clone();

    MqttServer::new()
        .v3(v3::MqttServer::new(fn_factory_with_config(move |_| {
            let zs = zs_v3.clone();
            let config = config_v3.clone();
            let auth_dictionary = auth_dictionary_v3.clone();
            Ready::Ok::<_, ()>(fn_service(move |h| {
                handshake_v3(h, zs.clone(), config.clone(), auth_dictionary.clone())
            }))
        }))
        .publish(fn_factory_with_config(
            |session: v3::Session<MqttSessionState>| {
                Ready::Ok::<_, MqttPluginError>(fn_service(move |req| {
                    publish_v3(session.clone(), req)
                }))
            },
        ))
        .control(fn_factory_with_config(
            |session: v3::Session<MqttSessionState>| {
                Ready::Ok::<_, MqttPluginError>(fn_service(move |req| {
                    control_v3(session.clone(), req)
                }))
            },
        )))
        .v5(v5::MqttServer::new(fn_factory_with_config(move |_| {
            let zs = zs_v5.clone();
            let config = config_v5.clone();
            let auth_dictionary = auth_dictionary_v5.clone();
            Ready::Ok::<_, ()>(fn_service(move |h| {
                handshake_v5(h, zs.clone(), config.clone(), auth_dictionary.clone())
            }))
        }))
        .publish(fn_factory_with_config(
            |session: v5::Session<MqttSessionState>| {
                Ready::Ok::<_, MqttPluginError>(fn_service(move |req| {
                    publish_v5(session.clone(), req)
                }))
            },
        ))
        .control(fn_factory_with_config(
            |session: v5::Session<MqttSessionState>| {
                Ready::Ok::<_, MqttPluginError>(fn_service(move |req| {
                    control_v5(session.clone(), req)
                }))
            },
        )))
}

fn treat_admin_query(query: Query, admin_keyexpr_prefix: &keyexpr, config: &Config) {
    let selector = query.selector();
    tracing::debug!("Query on admin space: {:?}", selector);

    // get the list of sub-key expressions that will match the same stored keys than
    // the selector, if those keys had the admin_keyexpr_prefix.
    let sub_kes = selector.key_expr.strip_prefix(admin_keyexpr_prefix);
    if sub_kes.is_empty() {
        tracing::error!("Received query for admin space: '{}' - but it's not prefixed by admin_keyexpr_prefix='{}'", selector, admin_keyexpr_prefix);
        return;
    }

    // Get all matching keys/values
    let mut kvs: Vec<(&keyexpr, Value)> = Vec::with_capacity(sub_kes.len());
    for sub_ke in sub_kes {
        if sub_ke.intersects(&ADMIN_SPACE_KE_VERSION) {
            kvs.push((
                &ADMIN_SPACE_KE_VERSION,
                Value::String(MqttPlugin::PLUGIN_LONG_VERSION.to_string()),
            ));
        }
        if sub_ke.intersects(&ADMIN_SPACE_KE_CONFIG) {
            kvs.push((
                &ADMIN_SPACE_KE_CONFIG,
                serde_json::to_value(config).unwrap(),
            ));
        }
    }

    // send replies
    for (ke, v) in kvs.drain(..) {
        let admin_keyexpr = admin_keyexpr_prefix / ke;
        match ZBytes::try_from(v) {
            Ok(bytes) => {
                if let Err(e) = query
                    .reply(admin_keyexpr, bytes)
                    .encoding(Encoding::APPLICATION_JSON)
                    .wait()
                {
                    tracing::warn!("Error replying to admin query {:?}: {}", query, e);
                }
            }
            Err(err) => {
                tracing::error!("Could not Serialize serde_json::Value to ZBytes {}", err)
            }
        }
    }
}

// NOTE: this types exists just because we can't implement TryFrom<Box<dyn std::error::Error + Send + Sync + 'static>> for v5::PublishAck
// (required for MQTT V5 negative acks)
#[derive(Debug)]
struct MqttPluginError {
    err: Box<dyn std::error::Error + Send + Sync + 'static>,
}

impl From<ZError> for MqttPluginError {
    fn from(e: ZError) -> Self {
        MqttPluginError { err: e }
    }
}

impl From<std::io::Error> for MqttPluginError {
    fn from(e: std::io::Error) -> Self {
        MqttPluginError { err: e.into() }
    }
}

impl From<rustls::Error> for MqttPluginError {
    fn from(e: rustls::Error) -> Self {
        MqttPluginError { err: e.into() }
    }
}

impl From<String> for MqttPluginError {
    fn from(e: String) -> Self {
        MqttPluginError { err: e.into() }
    }
}

// mqtt5 supports negative acks, so service error could be converted to PublishAck
// (weird way to do it, but that's how it's done in ntex-mqtt examples...)
impl std::convert::TryFrom<MqttPluginError> for v5::PublishAck {
    type Error = MqttPluginError;
    fn try_from(err: MqttPluginError) -> Result<Self, Self::Error> {
        Err(err)
    }
}

async fn handshake_v3<'a>(
    handshake: v3::Handshake,
    zsession: Arc<Session>,
    config: Arc<Config>,
    auth_dictionary: Arc<Option<HashMap<User, Password>>>,
) -> Result<v3::HandshakeAck<MqttSessionState<'a>>, MqttPluginError> {
    let client_id = handshake.packet().client_id.to_string();

    match is_authorized(
        (*auth_dictionary).as_ref(),
        handshake.packet().username.as_ref(),
        handshake.packet().password.as_ref(),
    ) {
        Ok(_) => {
            tracing::info!("MQTT client {} connects using v3", client_id);
            let session =
                MqttSessionState::new(client_id, zsession, config, handshake.sink().into());
            Ok(handshake.ack(session, false))
        }
        Err(err) => {
            tracing::warn!(
                "MQTT client {} connect using v3 rejected: {}",
                client_id,
                err
            );
            Ok(handshake.not_authorized())
        }
    }
}

async fn publish_v3(
    session: v3::Session<MqttSessionState<'_>>,
    publish: v3::Publish,
) -> Result<(), MqttPluginError> {
    session
        .state()
        .route_mqtt_to_zenoh(publish.topic(), publish.payload())
        .await
        .map_err(MqttPluginError::from)
}

async fn control_v3(
    session: v3::Session<MqttSessionState<'_>>,
    control: v3::ControlMessage<MqttPluginError>,
) -> Result<v3::ControlResult, MqttPluginError> {
    tracing::trace!(
        "MQTT client {} sent control: {:?}",
        session.client_id,
        control
    );

    match control {
        v3::ControlMessage::Ping(ref msg) => Ok(msg.ack()),
        v3::ControlMessage::Disconnect(msg) => {
            tracing::debug!("MQTT client {} disconnected", session.client_id);
            session.sink().close();
            Ok(msg.ack())
        }
        v3::ControlMessage::Subscribe(mut msg) => {
            for mut s in msg.iter_mut() {
                let topic = s.topic().as_str();
                tracing::debug!(
                    "MQTT client {} subscribes to '{}'",
                    session.client_id,
                    topic
                );
                match session.state().map_mqtt_subscription(topic).await {
                    Ok(()) => s.confirm(v3::QoS::AtMostOnce),
                    Err(e) => {
                        tracing::error!("Subscription to '{}' failed: {}", topic, e);
                        s.fail()
                    }
                }
            }
            Ok(msg.ack())
        }
        v3::ControlMessage::Unsubscribe(msg) => {
            for topic in msg.iter() {
                tracing::debug!(
                    "MQTT client {} unsubscribes from '{}'",
                    session.client_id,
                    topic.as_str()
                );
            }
            Ok(msg.ack())
        }
        v3::ControlMessage::Closed(msg) => {
            tracing::debug!("MQTT client {} closed connection", session.client_id);
            session.sink().force_close();
            Ok(msg.ack())
        }
        v3::ControlMessage::Error(msg) => {
            tracing::warn!(
                "MQTT client {} Error received: {}",
                session.client_id,
                msg.get_ref().err
            );
            Ok(msg.ack())
        }
        v3::ControlMessage::ProtocolError(ref msg) => {
            tracing::warn!(
                "MQTT client {}: ProtocolError received: {} => disconnect it",
                session.client_id,
                msg.get_ref()
            );
            Ok(control.disconnect())
        }
        v3::ControlMessage::PeerGone(msg) => {
            tracing::debug!(
                "MQTT client {}: PeerGone => close connection",
                session.client_id
            );
            session.sink().close();
            Ok(msg.ack())
        }
    }
}

async fn handshake_v5<'a>(
    handshake: v5::Handshake,
    zsession: Arc<Session>,
    config: Arc<Config>,
    auth_dictionary: Arc<Option<HashMap<User, Password>>>,
) -> Result<v5::HandshakeAck<MqttSessionState<'a>>, MqttPluginError> {
    let client_id = handshake.packet().client_id.to_string();

    match is_authorized(
        (*auth_dictionary).as_ref(),
        handshake.packet().username.as_ref(),
        handshake.packet().password.as_ref(),
    ) {
        Ok(_) => {
            tracing::info!("MQTT client {} connects using v5", client_id);
            let session =
                MqttSessionState::new(client_id, zsession, config, handshake.sink().into());
            Ok(handshake.ack(session))
        }
        Err(err) => {
            tracing::warn!(
                "MQTT client {} connect using v5 rejected: {}",
                client_id,
                err
            );
            Ok(handshake.failed(ntex_mqtt::v5::codec::ConnectAckReason::NotAuthorized))
        }
    }
}

async fn publish_v5(
    session: v5::Session<MqttSessionState<'_>>,
    publish: v5::Publish,
) -> Result<v5::PublishAck, MqttPluginError> {
    session
        .state()
        .route_mqtt_to_zenoh(publish.topic(), publish.payload())
        .await
        .map(|()| publish.ack())
        .map_err(MqttPluginError::from)
}

async fn control_v5(
    session: v5::Session<MqttSessionState<'_>>,
    control: v5::ControlMessage<MqttPluginError>,
) -> Result<v5::ControlResult, MqttPluginError> {
    tracing::trace!(
        "MQTT client {} sent control: {:?}",
        session.client_id,
        control
    );

    use v5::codec::{Disconnect, DisconnectReasonCode};
    match control {
        v5::ControlMessage::Auth(_) => {
            tracing::debug!(
                "MQTT client {} wants to authenticate... not yet supported!",
                session.client_id
            );
            Ok(control.disconnect_with(Disconnect::new(
                DisconnectReasonCode::ImplementationSpecificError,
            )))
        }
        v5::ControlMessage::Ping(msg) => Ok(msg.ack()),
        v5::ControlMessage::Disconnect(msg) => {
            tracing::debug!("MQTT client {} disconnected", session.client_id);
            session.sink().close();
            Ok(msg.ack())
        }
        v5::ControlMessage::Subscribe(mut msg) => {
            for mut s in msg.iter_mut() {
                let topic = s.topic().as_str();
                tracing::debug!(
                    "MQTT client {} subscribes to '{}'",
                    session.client_id,
                    topic
                );
                match session.state().map_mqtt_subscription(topic).await {
                    Ok(()) => s.confirm(v5::QoS::AtMostOnce),
                    Err(e) => {
                        tracing::error!("Subscription to '{}' failed: {}", topic, e);
                        s.fail(v5::codec::SubscribeAckReason::ImplementationSpecificError)
                    }
                }
            }
            Ok(msg.ack())
        }
        v5::ControlMessage::Unsubscribe(msg) => {
            for topic in msg.iter() {
                tracing::debug!(
                    "MQTT client {} unsubscribes from '{}'",
                    session.client_id,
                    topic.as_str()
                );
            }
            Ok(msg.ack())
        }
        v5::ControlMessage::Closed(msg) => {
            tracing::debug!("MQTT client {} closed connection", session.client_id);
            session.sink().close();
            Ok(msg.ack())
        }
        v5::ControlMessage::Error(msg) => {
            tracing::warn!(
                "MQTT client {} Error received: {}",
                session.client_id,
                msg.get_ref().err
            );
            Ok(msg.ack(DisconnectReasonCode::UnspecifiedError))
        }
        v5::ControlMessage::ProtocolError(msg) => {
            tracing::warn!(
                "MQTT client {}: ProtocolError received: {}",
                session.client_id,
                msg.get_ref()
            );
            session.sink().close();
            Ok(msg.reason_code(DisconnectReasonCode::ProtocolError).ack())
        }
        v5::ControlMessage::PeerGone(msg) => {
            tracing::debug!(
                "MQTT client {}: PeerGone => close connection",
                session.client_id
            );
            session.sink().close();
            Ok(msg.ack())
        }
    }
}
