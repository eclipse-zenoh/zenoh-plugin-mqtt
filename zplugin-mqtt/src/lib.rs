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
use log::debug;
use std::env;
use std::sync::Arc;
use zenoh::plugins::{Plugin, RunningPluginTrait, Runtime, ZenohPlugin};
use zenoh::prelude::r#async::AsyncResolve;
use zenoh::prelude::*;
use zenoh::Result as ZResult;
use zenoh::Session;
use zenoh_core::zresult::ZError;
use zenoh_core::{bail, zerror};

use ntex::service::{fn_factory_with_config, fn_service};
use ntex::util::Ready;
use ntex_mqtt::{v3, v5, MqttServer};

pub mod config;
use config::Config;

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
    debug!("MQTT plugin {}", LONG_VERSION.as_str());
    debug!("MQTT plugin {:?}", config);

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
                            |session: v3::Session<MqttSession>| {
                                Ready::Ok::<_, MqttPluginError>(fn_service(move |req| {
                                    publish_v3(session.clone(), req)
                                }))
                            },
                        )))
                        .v5(v5::MqttServer::new(fn_factory_with_config(move |_| {
                            let zs = zs_v5.clone();
                            Ready::Ok::<_, ()>(fn_service(move |h| handshake_v5(h, zs.clone())))
                        }))
                        .publish(fn_factory_with_config(
                            |session: v5::Session<MqttSession>| {
                                Ready::Ok::<_, MqttPluginError>(fn_service(move |req| {
                                    publish_v5(session.clone(), req)
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

#[derive(Clone, Debug)]
struct MqttSession {
    zsession: Arc<Session>,
    _client_id: String,
}

// pub type MqttPluginError = Box<dyn std::error::Error + Send + Sync + 'static>;

#[derive(Debug)]
struct MqttPluginError {
    _err: Box<dyn std::error::Error + Send + Sync + 'static>,
}

impl From<ZError> for MqttPluginError {
    fn from(e: ZError) -> Self {
        MqttPluginError { _err: e.into() }
    }
}

impl From<Box<dyn std::error::Error + Send + Sync + 'static>> for MqttPluginError {
    fn from(e: Box<dyn std::error::Error + Send + Sync + 'static>) -> Self {
        MqttPluginError { _err: e.into() }
    }
}

/// mqtt5 supports negative acks, so service error could be converted to PublishAck
impl std::convert::TryFrom<MqttPluginError> for v5::PublishAck {
    type Error = MqttPluginError;

    fn try_from(err: MqttPluginError) -> Result<Self, Self::Error> {
        Err(err)
    }
}

async fn handshake_v3(
    handshake: v3::Handshake,
    zsession: Arc<Session>,
) -> Result<v3::HandshakeAck<MqttSession>, MqttPluginError> {
    log::info!("v3 new connection: {:?}", handshake);

    let session = MqttSession {
        zsession,
        _client_id: handshake.packet().client_id.to_string(),
    };

    Ok(handshake.ack(session, false))
}

async fn publish_v3(
    session: v3::Session<MqttSession>,
    publish: v3::Publish,
) -> Result<(), MqttPluginError> {
    log::info!(
        "v3 incoming publish ({:?}): {:?} -> {:?}",
        session.state(),
        publish.id(),
        publish.topic()
    );

    session
        .state()
        .zsession
        .put(publish.topic().path(), publish.payload().deref())
        .res()
        .await
        .map_err(|e| MqttPluginError::from(e))
}

async fn handshake_v5(
    handshake: v5::Handshake,
    zsession: Arc<Session>,
) -> Result<v5::HandshakeAck<MqttSession>, MqttPluginError> {
    log::info!("v5 new connection: {:?}", handshake);

    let session = MqttSession {
        zsession,
        _client_id: handshake.packet().client_id.to_string(),
    };

    Ok(handshake.ack(session))
}

async fn publish_v5(
    session: v5::Session<MqttSession>,
    publish: v5::Publish,
) -> Result<v5::PublishAck, MqttPluginError> {
    log::info!(
        "v5 incoming publish ({:?}) : {:?} -> {:?}",
        session.state(),
        publish.id(),
        publish.topic()
    );

    session
        .state()
        .zsession
        .put(publish.topic().path(), publish.payload().deref())
        .res()
        .await
        .map(|_| publish.ack())
        .map_err(|e| MqttPluginError::from(e))
}
