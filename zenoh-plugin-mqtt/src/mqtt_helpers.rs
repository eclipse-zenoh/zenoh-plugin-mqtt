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

use std::convert::TryInto;

use ntex::util::{ByteString, Bytes};
use ntex_mqtt::{error::SendPacketError, v3, v5};
use zenoh::{
    internal::bail,
    key_expr::{KeyExpr, OwnedKeyExpr},
    Result as ZResult,
};

use crate::config::Config;

const MQTT_SEPARATOR: char = '/';
const MQTT_EMPTY_LEVEL: &str = "//";
const MQTT_SINGLE_WILD: char = '+';
const MQTT_MULTI_WILD: char = '#';

pub(crate) fn mqtt_topic_to_ke<'a>(
    topic: &'a str,
    scope: &Option<OwnedKeyExpr>,
) -> ZResult<KeyExpr<'a>> {
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

    let ke: KeyExpr = if !topic.contains([MQTT_SINGLE_WILD, MQTT_MULTI_WILD]) {
        topic.try_into()?
    } else {
        topic
            .replace(MQTT_SINGLE_WILD, "*")
            .replace(MQTT_MULTI_WILD, "**")
            .try_into()?
    };

    match scope {
        Some(scope) => Ok((scope / &ke).into()),
        None => Ok(ke),
    }
}

pub(crate) fn ke_to_mqtt_topic_publish(
    ke: &KeyExpr<'_>,
    scope: &Option<OwnedKeyExpr>,
) -> ZResult<ByteString> {
    if ke.is_wild() {
        bail!("Zenoh KeyExpr '{}' contains wildcards and cannot be converted to MQTT topic for publications", ke);
    }
    match scope {
        Some(scope) => {
            let after_scope_idx = scope.as_str().len();
            if ke.starts_with(scope.as_str()) && ke.chars().nth(after_scope_idx) == Some('/') {
                Ok(ke[after_scope_idx + 1..].into())
            } else {
                bail!(
                    "Zenoh KeyExpr '{}' doesn't start with the expected scope '{}'",
                    ke,
                    scope
                );
            }
        }
        None => Ok(ke.as_str().into()),
    }
}

pub(crate) fn is_allowed(mqtt_topic: &str, config: &Config) -> bool {
    match (&config.allow, &config.deny) {
        (Some(allow), None) => allow.is_match(mqtt_topic),
        (None, Some(deny)) => !deny.is_match(mqtt_topic),
        (Some(allow), Some(deny)) => allow.is_match(mqtt_topic) && !deny.is_match(mqtt_topic),
        (None, None) => true,
    }
}

#[derive(Clone, Debug)]
pub(crate) enum MqttSink {
    V3(v3::MqttSink),
    V5(v5::MqttSink),
}

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
            MqttSink::V3(sink) => sink.publish(topic, payload).send_at_most_once(),
            MqttSink::V5(sink) => sink.publish(topic, payload).send_at_most_once(),
        }
    }

    pub(crate) fn close(&self) {
        match self {
            MqttSink::V3(sink) => {
                sink.close();
            }
            MqttSink::V5(sink) => {
                sink.close();
            }
        }
    }

    pub(crate) fn is_open(&self) -> bool {
        match self {
            MqttSink::V3(sink) => sink.is_open(),
            MqttSink::V5(sink) => sink.is_open(),
        }
    }
}

impl From<v3::MqttSink> for MqttSink {
    fn from(s: v3::MqttSink) -> Self {
        MqttSink::V3(s)
    }
}

impl From<v5::MqttSink> for MqttSink {
    fn from(s: v5::MqttSink) -> Self {
        MqttSink::V5(s)
    }
}
