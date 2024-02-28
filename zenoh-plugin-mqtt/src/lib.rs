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
use ntex::io::IoBoxed;
use ntex::service::{chain_factory, fn_factory_with_config, fn_service};
use ntex::time::Deadline;
use ntex::util::Ready;
use ntex::ServiceFactory;
use ntex_mqtt::{v3, v5, MqttError, MqttServer};
use ntex_tls::rustls::Acceptor;
use rustls::server::AllowAnyAuthenticatedClient;
use rustls::{Certificate, PrivateKey, RootCertStore, ServerConfig};
use secrecy::ExposeSecret;
use serde_json::Value;
use std::env;
use std::io::BufReader;
use std::sync::Arc;
use zenoh::plugins::{RunningPluginTrait, ZenohPlugin};
use zenoh::prelude::r#async::*;
use zenoh::queryable::Query;
use zenoh::runtime::Runtime;
use zenoh::Result as ZResult;
use zenoh::Session;
use zenoh_core::zerror;
use zenoh_core::zresult::ZError;
use zenoh_plugin_trait::{plugin_long_version, plugin_version, Plugin, PluginControl};

#[macro_use]
extern crate zenoh_core;

pub mod config;
mod mqtt_helpers;
mod mqtt_session_state;
use config::{Config, TLSConfig};
use mqtt_session_state::MqttSessionState;

macro_rules! ke_for_sure {
    ($val:expr) => {
        unsafe { keyexpr::from_str_unchecked($val) }
    };
}

lazy_static::lazy_static! {
    static ref KE_PREFIX_ADMIN_SPACE: &'static keyexpr = ke_for_sure!("@/service");
    static ref ADMIN_SPACE_KE_VERSION: &'static keyexpr = ke_for_sure!("version");
    static ref ADMIN_SPACE_KE_CONFIG: &'static keyexpr = ke_for_sure!("config");
}

#[cfg(feature = "no_mangle")]
zenoh_plugin_trait::declare_plugin!(MqttPlugin);

pub struct MqttPlugin;

impl ZenohPlugin for MqttPlugin {}
impl Plugin for MqttPlugin {
    type StartArgs = Runtime;
    type Instance = zenoh::plugins::RunningPlugin;

    const DEFAULT_NAME: &'static str = "zenoh-plugin-mqtt";
    const PLUGIN_LONG_VERSION: &'static str = plugin_long_version!();
    const PLUGIN_VERSION: &'static str = plugin_version!();

    fn start(name: &str, runtime: &Self::StartArgs) -> ZResult<zenoh::plugins::RunningPlugin> {
        // Try to initiate login.
        // Required in case of dynamic lib, otherwise no logs.
        // But cannot be done twice in case of static link.
        let _ = env_logger::try_init();

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

        async_std::task::spawn(run(runtime.clone(), config, tls_config));
        Ok(Box::new(MqttPlugin))
    }
}

impl PluginControl for MqttPlugin {}
impl RunningPluginTrait for MqttPlugin {}

async fn run(runtime: Runtime, config: Config, tls_config: Option<Arc<ServerConfig>>) {
    // Try to initiate login.
    // Required in case of dynamic lib, otherwise no logs.
    // But cannot be done twice in case of static link.
    let _ = env_logger::try_init();
    log::debug!("MQTT plugin {}", MqttPlugin::PLUGIN_LONG_VERSION);
    log::debug!("MQTT plugin {:?}", config);

    // init Zenoh Session with provided Runtime
    let zsession = match zenoh::init(runtime)
        .aggregated_subscribers(config.generalise_subs.clone())
        .aggregated_publishers(config.generalise_pubs.clone())
        .res()
        .await
    {
        Ok(session) => Arc::new(session),
        Err(e) => {
            log::error!("Unable to init zenoh session for MQTT plugin : {:?}", e);
            return;
        }
    };

    // declare admin space queryable
    let admin_keyexpr_prefix =
        *KE_PREFIX_ADMIN_SPACE / &zsession.zid().into_keyexpr() / ke_for_sure!("mqtt");
    let admin_keyexpr_expr = (&admin_keyexpr_prefix) / ke_for_sure!("**");
    log::debug!("Declare admin space on {}", admin_keyexpr_expr);
    let config2 = config.clone();
    let _admin_queryable = zsession
        .declare_queryable(admin_keyexpr_expr)
        .callback(move |query| treat_admin_query(query, &admin_keyexpr_prefix, &config2))
        .res()
        .await
        .expect("Failed to create AdminSpace queryable");

    // Start MQTT Server task
    let config = Arc::new(config);
    ntex::rt::System::new(MqttPlugin::DEFAULT_NAME)
        .block_on(async move {
            let server = match tls_config {
                Some(tls) => {
                    ntex::server::Server::build().bind("mqtt", config.port.clone(), move |_| {
                        chain_factory(Acceptor::new(tls.clone()))
                            .map_err(|err| MqttError::Service(MqttPluginError::from(err)))
                            .and_then(create_mqtt_server(zsession.clone(), config.clone()))
                    })?
                }
                None => {
                    ntex::server::Server::build().bind("mqtt", config.port.clone(), move |_| {
                        create_mqtt_server(zsession.clone(), config.clone())
                    })?
                }
            };
            server.workers(1).run().await
        })
        .unwrap();
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
    use base64::engine::general_purpose;
    use base64::Engine;
    Ok(general_purpose::STANDARD
        .decode(data)
        .map_err(|e| zerror!("Unable to perform base64 decoding: {e:?}"))?)
}

fn load_private_key(bytes: Vec<u8>) -> ZResult<PrivateKey> {
    let mut reader = BufReader::new(bytes.as_slice());

    loop {
        match rustls_pemfile::read_one(&mut reader) {
            Ok(item) => match item {
                Some(rustls_pemfile::Item::RSAKey(key)) => return Ok(rustls::PrivateKey(key)),
                Some(rustls_pemfile::Item::PKCS8Key(key)) => return Ok(rustls::PrivateKey(key)),
                Some(rustls_pemfile::Item::ECKey(key)) => return Ok(rustls::PrivateKey(key)),
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

    let certs = match rustls_pemfile::certs(&mut reader) {
        Ok(certs) => certs,
        Err(e) => return Err(zerror!("Cannot parse certificate {e:?}").into()),
    };

    match certs.is_empty() {
        true => Err(zerror!("No certificates found").into()),
        false => Ok(certs.iter().map(|c| Certificate(c.to_vec())).collect()),
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

fn create_mqtt_server(
    session: Arc<Session>,
    config: Arc<Config>,
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

    MqttServer::new()
        .v3(v3::MqttServer::new(fn_factory_with_config(move |_| {
            let zs = zs_v3.clone();
            let config = config_v3.clone();
            Ready::Ok::<_, ()>(fn_service(move |h| {
                handshake_v3(h, zs.clone(), config.clone())
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
            Ready::Ok::<_, ()>(fn_service(move |h| {
                handshake_v5(h, zs.clone(), config.clone())
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
    log::debug!("Query on admin space: {:?}", selector);

    // get the list of sub-key expressions that will match the same stored keys than
    // the selector, if those keys had the admin_keyexpr_prefix.
    let sub_kes = selector.key_expr.strip_prefix(admin_keyexpr_prefix);
    if sub_kes.is_empty() {
        log::error!("Received query for admin space: '{}' - but it's not prefixed by admin_keyexpr_prefix='{}'", selector, admin_keyexpr_prefix);
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
        use zenoh::prelude::sync::SyncResolve;
        if let Err(e) = query.reply(Ok(Sample::new(admin_keyexpr, v))).res_sync() {
            log::warn!("Error replying to admin query {:?}: {}", query, e);
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
        MqttPluginError { err: e.into() }
    }
}

impl From<Box<dyn std::error::Error + Send + Sync + 'static>> for MqttPluginError {
    fn from(err: Box<dyn std::error::Error + Send + Sync + 'static>) -> Self {
        MqttPluginError { err }
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
) -> Result<v3::HandshakeAck<MqttSessionState<'a>>, MqttPluginError> {
    let client_id = handshake.packet().client_id.to_string();
    log::info!("MQTT client {} connects using v3", client_id);

    let session = MqttSessionState::new(client_id, zsession, config, handshake.sink().into());
    Ok(handshake.ack(session, false))
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
    log::trace!(
        "MQTT client {} sent control: {:?}",
        session.client_id,
        control
    );

    match control {
        v3::ControlMessage::Ping(ref msg) => Ok(msg.ack()),
        v3::ControlMessage::Disconnect(msg) => {
            log::debug!("MQTT client {} disconnected", session.client_id);
            session.sink().close();
            Ok(msg.ack())
        }
        v3::ControlMessage::Subscribe(mut msg) => {
            for mut s in msg.iter_mut() {
                let topic = s.topic().as_str();
                log::debug!(
                    "MQTT client {} subscribes to '{}'",
                    session.client_id,
                    topic
                );
                match session.state().map_mqtt_subscription(topic).await {
                    Ok(()) => s.confirm(v3::QoS::AtMostOnce),
                    Err(e) => {
                        log::error!("Subscription to '{}' failed: {}", topic, e);
                        s.fail()
                    }
                }
            }
            Ok(msg.ack())
        }
        v3::ControlMessage::Unsubscribe(msg) => {
            for topic in msg.iter() {
                log::debug!(
                    "MQTT client {} unsubscribes from '{}'",
                    session.client_id,
                    topic.as_str()
                );
            }
            Ok(msg.ack())
        }
        v3::ControlMessage::Closed(msg) => {
            log::debug!("MQTT client {} closed connection", session.client_id);
            session.sink().force_close();
            Ok(msg.ack())
        }
        v3::ControlMessage::Error(msg) => {
            log::warn!(
                "MQTT client {} Error received: {}",
                session.client_id,
                msg.get_ref().err
            );
            Ok(msg.ack())
        }
        v3::ControlMessage::ProtocolError(ref msg) => {
            log::warn!(
                "MQTT client {}: ProtocolError received: {} => disconnect it",
                session.client_id,
                msg.get_ref()
            );
            Ok(control.disconnect())
        }
        v3::ControlMessage::PeerGone(msg) => {
            log::debug!(
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
) -> Result<v5::HandshakeAck<MqttSessionState<'a>>, MqttPluginError> {
    let client_id = handshake.packet().client_id.to_string();
    log::info!("MQTT client {} connects using v5", client_id);

    let session = MqttSessionState::new(client_id, zsession, config, handshake.sink().into());
    Ok(handshake.ack(session))
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
    log::trace!(
        "MQTT client {} sent control: {:?}",
        session.client_id,
        control
    );

    use v5::codec::{Disconnect, DisconnectReasonCode};
    match control {
        v5::ControlMessage::Auth(_) => {
            log::debug!(
                "MQTT client {} wants to authenticate... not yet supported!",
                session.client_id
            );
            Ok(control.disconnect_with(Disconnect::new(
                DisconnectReasonCode::ImplementationSpecificError,
            )))
        }
        v5::ControlMessage::Ping(msg) => Ok(msg.ack()),
        v5::ControlMessage::Disconnect(msg) => {
            log::debug!("MQTT client {} disconnected", session.client_id);
            session.sink().close();
            Ok(msg.ack())
        }
        v5::ControlMessage::Subscribe(mut msg) => {
            for mut s in msg.iter_mut() {
                let topic = s.topic().as_str();
                log::debug!(
                    "MQTT client {} subscribes to '{}'",
                    session.client_id,
                    topic
                );
                match session.state().map_mqtt_subscription(topic).await {
                    Ok(()) => s.confirm(v5::QoS::AtMostOnce),
                    Err(e) => {
                        log::error!("Subscription to '{}' failed: {}", topic, e);
                        s.fail(v5::codec::SubscribeAckReason::ImplementationSpecificError)
                    }
                }
            }
            Ok(msg.ack())
        }
        v5::ControlMessage::Unsubscribe(msg) => {
            for topic in msg.iter() {
                log::debug!(
                    "MQTT client {} unsubscribes from '{}'",
                    session.client_id,
                    topic.as_str()
                );
            }
            Ok(msg.ack())
        }
        v5::ControlMessage::Closed(msg) => {
            log::debug!("MQTT client {} closed connection", session.client_id);
            session.sink().close();
            Ok(msg.ack())
        }
        v5::ControlMessage::Error(msg) => {
            log::warn!(
                "MQTT client {} Error received: {}",
                session.client_id,
                msg.get_ref().err
            );
            Ok(msg.ack(DisconnectReasonCode::UnspecifiedError))
        }
        v5::ControlMessage::ProtocolError(msg) => {
            log::warn!(
                "MQTT client {}: ProtocolError received: {}",
                session.client_id,
                msg.get_ref()
            );
            session.sink().close();
            Ok(msg.reason_code(DisconnectReasonCode::ProtocolError).ack())
        }
        v5::ControlMessage::PeerGone(msg) => {
            log::debug!(
                "MQTT client {}: PeerGone => close connection",
                session.client_id
            );
            session.sink().close();
            Ok(msg.ack())
        }
    }
}
