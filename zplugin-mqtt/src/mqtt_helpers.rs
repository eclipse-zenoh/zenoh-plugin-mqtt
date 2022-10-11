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

use ntex::util::{ByteString, Bytes};
use ntex_mqtt::{error::SendPacketError, v3, v5};
use std::convert::TryInto;
use std::sync::{Arc, Mutex};
use zenoh::plugins::ZResult;
use zenoh::prelude::*;
use zenoh_core::zlock;

const MQTT_SEPARATOR: char = '/';
const MQTT_EMPTY_LEVEL: &str = "//";
const MQTT_SINGLE_WILD: char = '+';
const MQTT_MULTI_WILD: char = '#';

pub(crate) fn mqtt_topic_to_ke<'a>(topic: &'a str, scope: &Option<OwnedKeyExpr>) -> ZResult<KeyExpr<'a>> {
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

    let ke: KeyExpr = if !topic.contains(|c| c == MQTT_SINGLE_WILD || c == MQTT_MULTI_WILD) {
        topic.try_into()?
    } else {
        topic
            .replace(MQTT_SINGLE_WILD, "*")
            .replace(MQTT_MULTI_WILD, "**")
            .try_into()?
    };

    match scope {
        Some(scope) => Ok((scope / &ke).into()),
        None => Ok(ke)
    }
}

pub(crate) fn ke_to_mqtt_topic_publish(ke: &KeyExpr<'_>) -> ZResult<ByteString> {
    if ke.is_wild() {
        bail!("Zenoh KeyExpr '{}' contains wildcards and cannot be converted to MQTT topic for publications", ke);
    }
    Ok(ke.as_str().into())
}

#[derive(Clone, Debug)]
pub(crate) enum MqttSink {
    V3(Arc<Mutex<v3::MqttSink>>),
    V5(Arc<Mutex<v5::MqttSink>>),
}

unsafe impl Send for MqttSink {}
unsafe impl Sync for MqttSink {}

impl MqttSink {
    pub(crate) fn publish_at_most_once<U>(
        &self,
        topic: U,
        payload: Bytes,
    ) -> Result<(), SendPacketError>
    where
        ByteString: From<U>,
    {
        match self {
            MqttSink::V3(s) => {
                let guard = zlock!(s);
                guard.publish(topic, payload).send_at_most_once()
            }
            MqttSink::V5(s) => {
                let guard = zlock!(s);
                guard.publish(topic, payload).send_at_most_once()
            }
        }
    }
}

impl From<v3::MqttSink> for MqttSink {
    fn from(s: v3::MqttSink) -> Self {
        MqttSink::V3(Arc::new(Mutex::new(s)))
    }
}

impl From<v5::MqttSink> for MqttSink {
    fn from(s: v5::MqttSink) -> Self {
        MqttSink::V5(Arc::new(Mutex::new(s)))
    }
}
