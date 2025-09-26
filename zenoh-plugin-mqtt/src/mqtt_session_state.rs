//
// Copyright (c) 2022 ZettaScale Technology
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
use std::{collections::HashMap, convert::TryInto, sync::Arc};

use flume::{Receiver, Sender};
use lazy_static::__Deref;
use ntex::util::{ByteString, Bytes};
use tokio::sync::RwLock;
use zenoh::{
    internal::zerror,
    key_expr::KeyExpr,
    pubsub::Subscriber,
    sample::{Locality, Sample},
    Result as ZResult, Session,
};

use crate::{config::Config, mqtt_helpers::*};

#[derive(Debug)]
pub(crate) struct MqttSessionState {
    pub(crate) client_id: String,
    pub(crate) zsession: Arc<Session>,
    pub(crate) config: Arc<Config>,
    pub(crate) subs: RwLock<HashMap<String, Subscriber<()>>>,
    pub(crate) tx: Sender<(ByteString, Bytes)>,
}

impl MqttSessionState {
    pub(crate) fn new(
        client_id: String,
        zsession: Arc<Session>,
        config: Arc<Config>,
        sink: MqttSink,
    ) -> MqttSessionState {
        let (tx, rx) = flume::bounded::<(ByteString, Bytes)>(config.tx_channel_size);
        spawn_mqtt_publisher(client_id.clone(), rx, sink);

        MqttSessionState {
            client_id,
            zsession,
            config,
            subs: RwLock::new(HashMap::new()),
            tx,
        }
    }

    pub(crate) async fn map_mqtt_subscription(&self, topic: &str) -> ZResult<()> {
        let sub_origin = if is_allowed(topic, &self.config) {
            // if topic is allowed, subscribe to publications coming from anywhere
            Locality::Any
        } else {
            // if topic is NOT allowed, subscribe to publications coming only from this plugin (for MQTT-to-MQTT routing only)
            tracing::debug!(
                "MQTT Client {}: topic '{}' is not allowed to be routed over Zenoh (see your 'allow' or 'deny' configuration) - re-publish only from MQTT publishers",
                self.client_id,
                topic
            );
            Locality::SessionLocal
        };

        let mut subs = self.subs.write().await;
        if !subs.contains_key(topic) {
            let ke = mqtt_topic_to_ke(topic, &self.config.scope)?;
            let client_id = self.client_id.clone();
            let config = self.config.clone();
            let tx = self.tx.clone();
            let sub = self
                .zsession
                .declare_subscriber(ke)
                .callback(move |sample| {
                    if let Err(e) = route_zenoh_to_mqtt(sample, &client_id, &config, &tx) {
                        tracing::warn!("{}", e);
                    }
                })
                .allowed_origin(sub_origin)
                .await?;
            subs.insert(topic.into(), sub);
            Ok(())
        } else {
            tracing::debug!(
                "MQTT Client {} already subscribes to {} => ignore",
                self.client_id,
                topic
            );
            Ok(())
        }
    }

    pub(crate) async fn route_mqtt_to_zenoh(
        &self,
        mqtt_topic: &ntex::router::Path<ByteString>,
        payload: &Bytes,
    ) -> ZResult<()> {
        let topic = mqtt_topic.get_ref().as_str();
        let destination = if is_allowed(topic, &self.config) {
            // if topic is allowed, publish to anywhere
            Locality::Any
        } else {
            // if topic is NOT allowed, publish only to this plugin (for MQTT-to-MQTT routing only)
            tracing::trace!(
                "MQTT Client {}: topic '{}' is not allowed to be routed over Zenoh (see your 'allow' or 'deny' configuration) - re-publish only to MQTT subscriber",
                self.client_id,
                topic
            );
            Locality::SessionLocal
        };

        let ke: KeyExpr = if let Some(scope) = &self.config.scope {
            (scope / topic.try_into()?).into()
        } else {
            topic.try_into()?
        };
        // TODO: check allow/deny
        tracing::trace!(
            "MQTT client {}: route from MQTT '{}' to Zenoh '{}'",
            self.client_id,
            topic,
            ke,
        );
        self.zsession
            .put(ke, payload.deref())
            .allowed_destination(destination)
            .await
    }

    pub(crate) async fn unsubscribe(&self, topic: &str) -> ZResult<()> {
        let mut subs = self.subs.write().await;
        if let Some(sub) = subs.remove(topic) {
            drop(sub);
            tracing::debug!(
                "MQTT Client {}: unsubscribed from Zenoh topic '{}'",
                self.client_id,
                topic
            );
        } else {
            tracing::error!(
                "MQTT Client {}: no subscription to unsubscribe for Zenoh topic '{}'",
                self.client_id,
                topic
            );
        }
        Ok(())
    }
}

fn route_zenoh_to_mqtt(
    sample: Sample,
    client_id: &str,
    config: &Config,
    tx: &Sender<(ByteString, Bytes)>,
) -> ZResult<()> {
    let topic = ke_to_mqtt_topic_publish(sample.key_expr(), &config.scope)?;
    tracing::trace!(
        "MQTT client {}: route from Zenoh '{}' to MQTT '{}'",
        client_id,
        sample.key_expr(),
        topic
    );
    let v: Vec<_> = sample.payload().to_bytes().to_vec();
    tx.try_send((topic, v.into())).map_err(|e| {
        zerror!(
            "MQTT client {}: error re-publishing on MQTT a Zenoh publication on {}: {}",
            client_id,
            sample.key_expr(),
            e
        )
        .into()
    })
}

fn spawn_mqtt_publisher(client_id: String, rx: Receiver<(ByteString, Bytes)>, sink: MqttSink) {
    ntex::rt::spawn(async move {
        loop {
            match rx.recv_async().await {
                Ok((topic, payload)) => {
                    if sink.is_open() {
                        if let Err(e) = sink.publish_at_most_once(topic, payload) {
                            tracing::trace!(
                                "Failed to send MQTT message for client {} - {}",
                                client_id,
                                e
                            );
                            sink.close();
                            break;
                        }
                    } else {
                        tracing::trace!("MQTT sink closed for client {}", client_id);
                        break;
                    }
                }
                Err(_) => {
                    tracing::trace!("MPSC Channel closed for client {}", client_id);
                    break;
                }
            }
        }
    });
}

