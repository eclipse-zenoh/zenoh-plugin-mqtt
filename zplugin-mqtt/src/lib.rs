//
// Copyright (c) 2017, 2020 ADLINK Technology Inc.
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ADLINK zenoh team, <zenoh@adlink-labs.tech>
//
use git_version::git_version;
use lazy_static::__Deref;
use ntex::service::{fn_factory_with_config, fn_service};
use ntex::util::{ByteString, Ready};
use ntex_mqtt::{v3, v5, MqttServer};
use std::convert::TryInto;
use std::env;
use std::sync::Arc;
use zenoh::plugins::{Plugin, RunningPluginTrait, Runtime, ZenohPlugin};
use zenoh::prelude::r#async::*;
use zenoh::Result as ZResult;
use zenoh::Session;
use zenoh_core::zresult::ZError;
use zenoh_core::{bail, zerror};

#[macro_use]
extern crate zenoh_core;

pub mod config;
mod mqtt_helpers;
mod mqtt_session_state;
use config::Config;
use mqtt_session_state::MqttSessionState;

pub const GIT_VERSION: &str = git_version!(prefix = "v", cargo_prefix = "v");
lazy_static::lazy_static! {
    pub static ref LONG_VERSION: String = format!("{} built with {}", GIT_VERSION, env!("RUSTC_VERSION"));
}

macro_rules! ke_for_sure {
    ($val:expr) => {
        unsafe { keyexpr::from_str_unchecked($val) }
    };
}

zenoh_plugin_trait::declare_plugin!(MqttPlugin);

pub struct MqttPlugin;

impl ZenohPlugin for MqttPlugin {}
impl Plugin for MqttPlugin {
    type StartArgs = Runtime;
    type RunningPlugin = zenoh::plugins::RunningPlugin;

    const STATIC_NAME: &'static str = "zenoh-plugin-mqtt";

    fn start(name: &str, runtime: &Self::StartArgs) -> ZResult<zenoh::plugins::RunningPlugin> {
        // Try to initiate login.
        // Required in case of dynamic lib, otherwise no logs.
        // But cannot be done twice in case of static link.
        let _ = env_logger::try_init();

        let runtime_conf = runtime.config.lock();
        let plugin_conf = runtime_conf
            .plugin(name)
            .ok_or_else(|| zerror!("Plugin `{}`: missing config", name))?;
        let config: Config = serde_json::from_value(plugin_conf.clone())
            .map_err(|e| zerror!("Plugin `{}` configuration error: {}", name, e))?;
        async_std::task::spawn(run(runtime.clone(), config));
        Ok(Box::new(MqttPlugin))
    }
}

impl RunningPluginTrait for MqttPlugin {
    fn config_checker(&self) -> zenoh::plugins::ValidationFunction {
        Arc::new(|_, _, _| bail!("zenoh-plugin-mqtt does not support hot configuration changes."))
    }

    fn adminspace_getter<'a>(
        &'a self,
        selector: &'a Selector<'a>,
        plugin_status_key: &str,
    ) -> ZResult<Vec<zenoh::plugins::Response>> {
        let mut responses = Vec::new();
        let version_key = [plugin_status_key, "/__version__"].concat();
        if selector.key_expr.intersects(ke_for_sure!(&version_key)) {
            responses.push(zenoh::plugins::Response::new(
                version_key,
                GIT_VERSION.into(),
            ));
        }
        Ok(responses)
    }
}

async fn run(runtime: Runtime, config: Config) {
    // Try to initiate login.
    // Required in case of dynamic lib, otherwise no logs.
    // But cannot be done twice in case of static link.
    let _ = env_logger::try_init();
    log::debug!("MQTT plugin {}", LONG_VERSION.as_str());
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

    ntex::rt::System::new(MqttPlugin::STATIC_NAME)
        .block_on(async move {
            ntex::server::Server::build()
                .bind("mqtt", config.port, move |_| {
                    let zs_v3 = zsession.clone();
                    let zs_v5 = zsession.clone();
                    MqttServer::new()
                        .v3(v3::MqttServer::new(fn_factory_with_config(move |_| {
                            let zs = zs_v3.clone();
                            Ready::Ok::<_, ()>(fn_service(move |h| handshake_v3(h, zs.clone())))
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
                            Ready::Ok::<_, ()>(fn_service(move |h| handshake_v5(h, zs.clone())))
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
                })?
                .workers(1)
                .run()
                .await
        })
        .unwrap();
}

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
    fn from(e: Box<dyn std::error::Error + Send + Sync + 'static>) -> Self {
        MqttPluginError { err: e.into() }
    }
}

/// mqtt5 supports negative acks, so service error could be converted to PublishAck
impl std::convert::TryFrom<MqttPluginError> for v5::PublishAck {
    type Error = MqttPluginError;
    fn try_from(err: MqttPluginError) -> Result<Self, Self::Error> {
        Err(err)
    }
}

async fn handshake_v3<'a>(
    handshake: v3::Handshake,
    zsession: Arc<Session>,
) -> Result<v3::HandshakeAck<MqttSessionState<'a>>, MqttPluginError> {
    let client_id = handshake.packet().client_id.to_string();
    log::info!("MQTT client {} connects using v3", client_id);

    let session = MqttSessionState::new(zsession, client_id);
    Ok(handshake.ack(session, false))
}

async fn publish_v3<'a>(
    session: v3::Session<MqttSessionState<'a>>,
    publish: v3::Publish,
) -> Result<(), MqttPluginError> {
    log::debug!(
        "MQTT client {} publishes 'on' {}",
        session.client_id,
        publish.topic().path()
    );

    session
        .zsession
        .put(publish.topic().path(), publish.payload().deref())
        .res()
        .await
        .map_err(|e| MqttPluginError::from(e))
}

async fn control_v3<'a>(
    session: v3::Session<MqttSessionState<'a>>,
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
                match session
                    .state()
                    .map_mqtt_subscription(topic, session.sink().clone().into())
                    .await
                {
                    Ok(()) => s.confirm(v5::QoS::AtMostOnce),
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
) -> Result<v5::HandshakeAck<MqttSessionState<'a>>, MqttPluginError> {
    let client_id = handshake.packet().client_id.to_string();
    log::info!("MQTT client {} connects using v5", client_id);

    let session = MqttSessionState::new(zsession, client_id);
    Ok(handshake.ack(session))
}

async fn publish_v5<'a>(
    session: v5::Session<MqttSessionState<'a>>,
    publish: v5::Publish,
) -> Result<v5::PublishAck, MqttPluginError> {
    log::debug!(
        "MQTT client {} publishes on '{}'",
        session.client_id,
        publish.topic().path()
    );

    session
        .zsession
        .put(publish.topic().path(), publish.payload().deref())
        .res()
        .await
        .map(|_| publish.ack())
        .map_err(|e| MqttPluginError::from(e))
}

async fn control_v5<'a>(
    session: v5::Session<MqttSessionState<'a>>,
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
                    "MQTT client {} subscribes 'to' {}",
                    session.client_id,
                    topic
                );
                match session
                    .state()
                    .map_mqtt_subscription(topic, session.sink().clone().into())
                    .await
                {
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

const MQTT_SEPARATOR: char = '/';
const MQTT_EMPTY_LEVEL: &'static str = "//";
const MQTT_SINGLE_WILD: char = '+';
const MQTT_MULTI_WILD: char = '#';

pub(crate) fn mqtt_topic_to_ke<'a>(topic: &'a str) -> ZResult<KeyExpr> {
    if topic.starts_with(MQTT_SEPARATOR) {
        bail!(
            "MQTT topic with empty level not-supported: '{}' (starts with {})",
            topic,
            MQTT_SEPARATOR
        );
    }
    if topic.ends_with(MQTT_SEPARATOR) {
        bail!(
            "MQTT topic with empty level not-supported: '{}' (ends with {})",
            topic,
            MQTT_SEPARATOR
        );
    }
    if topic.contains(MQTT_EMPTY_LEVEL) {
        bail!(
            "MQTT topic with empty level not-supported: '{}' (contains {})",
            topic,
            MQTT_EMPTY_LEVEL
        );
    }

    if !topic.contains(|c| c == MQTT_SINGLE_WILD || c == MQTT_MULTI_WILD) {
        topic.try_into()
    } else {
        topic
            .replace(MQTT_SINGLE_WILD, "*")
            .replace(MQTT_MULTI_WILD, "**")
            .try_into()
    }
}

pub(crate) fn ke_to_mqtt_topic<'a>(ke: &KeyExpr<'a>) -> ByteString {
    ke.as_str().into()
}
