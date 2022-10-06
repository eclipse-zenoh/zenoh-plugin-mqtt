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

use std::sync::{Arc, Mutex};

use ntex::util::{ByteString, Bytes};
use ntex_mqtt::{error::SendPacketError, v3, v5};
use zenoh_core::zlock;

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
                let guard = zlock!(&s);
                guard.publish(topic, payload).send_at_most_once()
            }
            MqttSink::V5(s) => {
                let guard = zlock!(&s);
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
