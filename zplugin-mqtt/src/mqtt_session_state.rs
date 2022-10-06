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
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};
use zenoh::plugins::ZResult;
use zenoh::prelude::r#async::*;
use zenoh::subscriber::Subscriber;

use crate::{ke_to_mqtt_topic, mqtt_helpers::MqttSink, mqtt_topic_to_ke};

#[derive(Debug)]
pub(crate) struct MqttSessionState<'a> {
    pub(crate) zsession: Arc<Session>,
    pub(crate) client_id: String,
    pub(crate) subs: RwLock<HashMap<String, Subscriber<'a, ()>>>,
}

impl MqttSessionState<'_> {
    pub(crate) fn new<'a>(zsession: Arc<Session>, client_id: String) -> MqttSessionState<'a> {
        MqttSessionState {
            zsession,
            client_id,
            subs: RwLock::new(HashMap::new()),
        }
    }

    pub(crate) async fn map_mqtt_subscription<'a>(
        &'a self,
        topic: &str,
        sink: MqttSink,
    ) -> ZResult<()> {
        let mut subs = zwrite!(self.subs);
        if !subs.contains_key(topic) {
            let ke = mqtt_topic_to_ke(&topic)?;
            let client_id = self.client_id.clone();
            let sub = self.zsession
                .declare_subscriber(ke)
                .callback(move |sample| {
                    let topic = ke_to_mqtt_topic(&sample.key_expr);
                    log::trace!("MQTT client {}: route publication on '{}' to Zenoh on '{}'", client_id, sample.key_expr, topic);
                    if let Err(e) = sink.publish_at_most_once(topic, sample.payload.contiguous().to_vec().into()) {
                        log::warn!("MQTT client {}: error re-publishing on MQTT a Zenoh publication on {}: {}", client_id, sample.key_expr, e);
                    }
                })
                .res()
                .await?;
            subs.insert(topic.into(), sub);
            Ok(())
        } else {
            log::debug!(
                "MQTT Client {} already subscribes to {} => ignore",
                self.client_id,
                topic
            );
            Ok(())
        }
    }
}
