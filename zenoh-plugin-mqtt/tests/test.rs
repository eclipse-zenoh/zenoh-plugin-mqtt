//
// Copyright (c) 2024 ZettaScale Technology
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
    sync::mpsc::{channel, Sender},
    time::Duration,
};

use ntex::{
    service::fn_service,
    time::{sleep, Millis},
    util::Ready,
};
use ntex_mqtt::v5;
use zenoh::{
    config::Config,
    internal::{plugins::PluginsManager, runtime::RuntimeBuilder},
    Wait,
};
use zenoh_config::ModeDependentValue;

// The test topic
const TEST_TOPIC: &str = "test-topic";
// The test payload
const TEST_PAYLOAD: &str = "Hello World";

#[derive(Debug)]
struct Error;

impl std::convert::TryFrom<Error> for v5::PublishAck {
    type Error = Error;

    fn try_from(err: Error) -> Result<Self, Self::Error> {
        Err(err)
    }
}

async fn create_mqtt_server() {
    let mut plugins_mgr = PluginsManager::static_plugins_only();
    plugins_mgr.declare_static_plugin::<zenoh_plugin_mqtt::MqttPlugin, &str>("mqtt", true);
    let mut config = Config::default();
    config.insert_json5("plugins/mqtt", "{}").unwrap();
    config
        .timestamping
        .set_enabled(Some(ModeDependentValue::Unique(true)))
        .unwrap();
    config.adminspace.set_enabled(true).unwrap();
    config.plugins_loading.set_enabled(true).unwrap();
    let mut runtime = RuntimeBuilder::new(config)
        .plugins_manager(plugins_mgr)
        .build()
        .await
        .unwrap();
    runtime.start().await.unwrap();
}

async fn create_mqtt_subscriber(tx: Sender<String>) {
    let client = v5::client::MqttConnector::new("127.0.0.1:1883")
        .client_id("mqtt-sub-id")
        .connect()
        .await
        .unwrap();

    let sink = client.sink();

    // handle incoming publishes
    ntex::rt::spawn(client.start(fn_service(
        move |control: v5::client::Control<Error>| match control {
            v5::client::Control::Publish(publish) => {
                println!(
                    "incoming publish: {:?} -> {:?} payload {:?}",
                    publish.packet().packet_id,
                    publish.packet().topic,
                    publish.packet().payload
                );
                let payload = std::str::from_utf8(&publish.packet().payload.to_vec())
                    .unwrap()
                    .to_owned();
                tx.send(payload).unwrap();
                Ready::Ok(publish.ack(v5::codec::PublishAckReason::Success))
            }
            v5::client::Control::Disconnect(msg) => {
                println!("Server disconnecting: {:?}", msg);
                Ready::Ok(msg.ack())
            }
            v5::client::Control::Error(msg) => {
                println!("Codec error: {:?}", msg);
                Ready::Ok(msg.ack(v5::codec::DisconnectReasonCode::UnspecifiedError))
            }
            v5::client::Control::ProtocolError(msg) => {
                println!("Protocol error: {:?}", msg);
                Ready::Ok(msg.ack())
            }
            v5::client::Control::PeerGone(msg) => {
                println!("Peer closed connection: {:?}", msg.error());
                Ready::Ok(msg.ack())
            }
            v5::client::Control::Closed(msg) => {
                println!("Server closed connection: {:?}", msg);
                Ready::Ok(msg.ack())
            }
        },
    )));

    // subscribe to topic
    sink.subscribe(None)
        .topic_filter(
            TEST_TOPIC.into(),
            v5::codec::SubscriptionOptions {
                qos: v5::codec::QoS::AtLeastOnce,
                no_local: false,
                retain_as_published: false,
                retain_handling: v5::codec::RetainHandling::AtSubscribe,
            },
        )
        .send()
        .await
        .unwrap();
    // Ensure the data is received
    sleep(Millis(3_000)).await;
}

async fn create_mqtt_publisher() {
    let client = v5::client::MqttConnector::new("127.0.0.1:1883")
        .client_id("mqtt-pub-id")
        .connect()
        .await
        .unwrap();

    let sink = client.sink();

    // handle incoming publishes
    ntex::rt::spawn(client.start(fn_service(
        |control: v5::client::Control<Error>| match control {
            v5::client::Control::Publish(publish) => {
                println!(
                    "incoming publish: {:?} -> {:?} payload {:?}",
                    publish.packet().packet_id,
                    publish.packet().topic,
                    publish.packet().payload
                );
                Ready::Ok(publish.ack(v5::codec::PublishAckReason::Success))
            }
            v5::client::Control::Disconnect(msg) => {
                println!("Server disconnecting: {:?}", msg);
                Ready::Ok(msg.ack())
            }
            v5::client::Control::Error(msg) => {
                println!("Codec error: {:?}", msg);
                Ready::Ok(msg.ack(v5::codec::DisconnectReasonCode::UnspecifiedError))
            }
            v5::client::Control::ProtocolError(msg) => {
                println!("Protocol error: {:?}", msg);
                Ready::Ok(msg.ack())
            }
            v5::client::Control::PeerGone(msg) => {
                println!("Peer closed connection: {:?}", msg.error());
                Ready::Ok(msg.ack())
            }
            v5::client::Control::Closed(msg) => {
                println!("Server closed connection: {:?}", msg);
                Ready::Ok(msg.ack())
            }
        },
    )));

    // send client publish
    let ack = sink
        .publish(TEST_TOPIC, TEST_PAYLOAD.into())
        .send_at_least_once()
        .await
        .unwrap();
    // Ensure the data is sent
    println!("ack received: {:?}", ack);
}

#[test]
fn test_mqtt_pub_mqtt_sub() {
    // Run the bridge for MQTT and Zenoh
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.spawn(create_mqtt_server());
    // Wait for the bridge to be ready
    std::thread::sleep(Duration::from_secs(2));

    // MQTT subscriber
    let (tx, rx) = channel();
    rt.spawn_blocking(move || {
        ntex::rt::System::new("mqtt_sub").block_on(create_mqtt_subscriber(tx))
    });
    std::thread::sleep(Duration::from_secs(1));

    // MQTT publisher
    rt.spawn_blocking(|| ntex::rt::System::new("mqtt_pub").block_on(create_mqtt_publisher()));

    // Wait for the test to complete
    let result = rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(result, TEST_PAYLOAD);

    // Stop the tokio runtime
    // Since ntex server is running in blocking thread, we need to force shutdown the runtime while completing the test
    rt.shutdown_background();
}

#[test]
fn test_mqtt_pub_zenoh_sub() {
    // Run the bridge for MQTT and Zenoh
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.spawn(create_mqtt_server());
    // Wait for the bridge to be ready
    std::thread::sleep(Duration::from_secs(2));

    // Zenoh subscriber
    let (tx, rx) = channel();
    let session = zenoh::open(zenoh::Config::default()).wait().unwrap();
    let _subscriber = session
        .declare_subscriber(TEST_TOPIC)
        .callback_mut(move |sample| {
            let data = sample
                .payload()
                .try_to_string()
                .to_owned()
                .unwrap()
                .into_owned();
            tx.send(data).unwrap();
        })
        .wait()
        .unwrap();
    std::thread::sleep(Duration::from_secs(1));

    // MQTT publisher
    rt.spawn_blocking(|| ntex::rt::System::new("mqtt_pub").block_on(create_mqtt_publisher()));

    // Wait for the test to complete
    let result = rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(result, TEST_PAYLOAD);

    // Stop the tokio runtime
    // Since ntex server is running in blocking thread, we need to force shutdown the runtime while completing the test
    rt.shutdown_background();
}

#[test]
fn test_zenoh_pub_mqtt_sub() {
    // Run the bridge for MQTT and Zenoh
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.spawn(create_mqtt_server());
    // Wait for the bridge to be ready
    std::thread::sleep(Duration::from_secs(2));

    // MQTT subscriber
    let (tx, rx) = channel();
    rt.spawn_blocking(move || {
        ntex::rt::System::new("mqtt_sub").block_on(create_mqtt_subscriber(tx))
    });
    std::thread::sleep(Duration::from_secs(1));

    // Zenoh publisher
    let session = zenoh::open(zenoh::Config::default()).wait().unwrap();
    let publisher = session.declare_publisher(TEST_TOPIC).wait().unwrap();
    publisher.put(TEST_PAYLOAD).wait().unwrap();

    // Wait for the test to complete
    let result = rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(result, TEST_PAYLOAD);

    // Stop the tokio runtime
    // Since ntex server is running in blocking thread, we need to force shutdown the runtime while completing the test
    rt.shutdown_background();
}
