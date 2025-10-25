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

            // Release lock before querying retained messages
            drop(subs);

            // Query and send retained messages for this subscription
            match self.query_retained_messages(topic).await {
                Ok(retained_messages) => {
                    for (mqtt_topic, payload) in retained_messages {
                        tracing::trace!(
                            "MQTT client {}: sending retained message on topic '{}' ({} bytes)",
                            self.client_id, mqtt_topic, payload.len()
                        );

                        if let Err(e) = self.tx.try_send((mqtt_topic.into(), payload)) {
                            tracing::warn!(
                                "MQTT client {}: failed to send retained message: {}",
                                self.client_id, e
                            );
                            // Continue sending other retained messages even if one fails
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "MQTT client {}: failed to query retained messages for '{}': {}",
                        self.client_id, topic, e
                    );
                    // Non-fatal: subscription still works, just no retained messages
                }
            }

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

    /// Build Zenoh key expression for retained message storage
    fn build_retained_key_expr(&self, mqtt_topic: &str) -> ZResult<KeyExpr> {
        use zenoh::key_expr::keyexpr;

        let topic_ke: KeyExpr = mqtt_topic.try_into()?;

        if let Some(scope) = &self.config.scope {
            Ok((scope / unsafe { keyexpr::from_str_unchecked("__retained__") } / topic_ke).into())
        } else {
            Ok((unsafe { keyexpr::from_str_unchecked("__retained__") } / topic_ke).into())
        }
    }

    /// Handle MQTT retained message by storing to or deleting from Zenoh storage
    pub(crate) async fn handle_retained_message(
        &self,
        mqtt_topic: &str,
        payload: &Bytes,
    ) -> ZResult<()> {
        if !self.config.retained_enabled {
            return Ok(());
        }

        let retained_ke = self.build_retained_key_expr(mqtt_topic)?;

        if payload.is_empty() {
            // Empty payload = delete retained message (per MQTT spec)
            tracing::trace!(
                "MQTT client {}: deleting retained message for topic '{}' (ke: '{}')",
                self.client_id, mqtt_topic, retained_ke
            );
            self.zsession.delete(retained_ke).await
        } else {
            // Store retained message to Zenoh storage
            tracing::trace!(
                "MQTT client {}: storing retained message for topic '{}' (ke: '{}', {} bytes)",
                self.client_id, mqtt_topic, retained_ke, payload.len()
            );
            self.zsession
                .put(retained_ke, payload.deref())
                .await
        }
    }

    /// Build Zenoh selector for querying retained messages matching MQTT topic filter
    fn build_retained_query_selector(&self, mqtt_topic_filter: &str) -> ZResult<String> {
        use zenoh::key_expr::keyexpr;

        // Convert MQTT wildcards to Zenoh: + -> *, # -> **
        let zenoh_pattern = mqtt_topic_to_ke(mqtt_topic_filter, &None)?;

        let selector_str = if let Some(scope) = &self.config.scope {
            format!("{}/__retained__/{}", scope, zenoh_pattern)
        } else {
            format!("__retained__/{}", zenoh_pattern)
        };

        Ok(selector_str)
    }

    /// Extract original MQTT topic from retained message key expression
    /// Key format: <scope>/__retained__/<mqtt_topic>
    fn extract_mqtt_topic_from_retained_ke(&self, ke: &KeyExpr) -> ZResult<String> {
        let ke_str = ke.as_str();

        let prefix = if let Some(scope) = &self.config.scope {
            format!("{}/__retained__/", scope)
        } else {
            "__retained__/".to_string()
        };

        ke_str
            .strip_prefix(&prefix)
            .map(|s| s.to_string())
            .ok_or_else(|| zerror!("Invalid retained message key: {}", ke_str).into())
    }

    /// Query Zenoh storage for retained messages matching the MQTT topic filter
    async fn query_retained_messages(
        &self,
        mqtt_topic_filter: &str,
    ) -> ZResult<Vec<(String, Bytes)>> {
        if !self.config.retained_enabled {
            return Ok(Vec::new());
        }

        let selector = self.build_retained_query_selector(mqtt_topic_filter)?;

        tracing::trace!(
            "MQTT client {}: querying retained messages for subscription '{}' (selector: '{}')",
            self.client_id, mqtt_topic_filter, selector
        );

        // Query Zenoh storage
        let replies = self.zsession.get(&selector).await?;

        // Collect matching retained messages
        let mut retained_messages = Vec::new();

        while let Ok(reply) = replies.recv_async().await {
            match reply.result() {
                Ok(sample) => {
                    // Extract original MQTT topic from key expression
                    match self.extract_mqtt_topic_from_retained_ke(sample.key_expr()) {
                        Ok(mqtt_topic) => {
                            let payload: Vec<u8> = sample.payload().to_bytes().to_vec();
                            retained_messages.push((mqtt_topic, payload.into()));
                        }
                        Err(e) => {
                            tracing::warn!(
                                "MQTT client {}: failed to extract topic from retained key '{}': {}",
                                self.client_id, sample.key_expr(), e
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!(
                        "MQTT client {}: query reply error: {}",
                        self.client_id, e
                    );
                }
            }
        }

        tracing::trace!(
            "MQTT client {}: found {} retained message(s) for subscription '{}'",
            self.client_id, retained_messages.len(), mqtt_topic_filter
        );

        Ok(retained_messages)
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
