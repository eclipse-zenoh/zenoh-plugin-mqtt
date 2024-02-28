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
use regex::Regex;
use serde::de::{Unexpected, Visitor};
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use zenoh::config::SecretValue;
use zenoh::prelude::*;

const DEFAULT_MQTT_INTERFACE: &str = "0.0.0.0";
const DEFAULT_MQTT_PORT: &str = "1883";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(
        default = "default_mqtt_port",
        deserialize_with = "deserialize_mqtt_port"
    )]
    pub port: String,
    #[serde(default)]
    pub scope: Option<OwnedKeyExpr>,
    #[serde(
        default,
        deserialize_with = "deserialize_regex",
        serialize_with = "serialize_allow"
    )]
    pub allow: Option<Regex>,
    #[serde(
        default,
        deserialize_with = "deserialize_regex",
        serialize_with = "serialize_deny"
    )]
    pub deny: Option<Regex>,
    #[serde(default)]
    pub generalise_subs: Vec<OwnedKeyExpr>,
    #[serde(default)]
    pub generalise_pubs: Vec<OwnedKeyExpr>,
    #[serde(default)]
    pub tls: Option<TLSConfig>,
    __required__: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_path")]
    __path__: Option<Vec<String>>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct TLSConfig {
    pub server_private_key: Option<String>,
    #[serde(skip_serializing)]
    pub server_private_key_base64: Option<SecretValue>,
    pub server_certificate: Option<String>,
    #[serde(skip_serializing)]
    pub server_certificate_base64: Option<SecretValue>,
    pub root_ca_certificate: Option<String>,
    #[serde(skip_serializing)]
    pub root_ca_certificate_base64: Option<SecretValue>,
}

fn default_mqtt_port() -> String {
    format!("{DEFAULT_MQTT_INTERFACE}:{DEFAULT_MQTT_PORT}")
}

fn deserialize_mqtt_port<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_any(MqttPortVisitor)
}

fn deserialize_path<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_option(OptPathVisitor)
}

struct OptPathVisitor;

impl<'de> serde::de::Visitor<'de> for OptPathVisitor {
    type Value = Option<Vec<String>>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(formatter, "none or a string or an array of strings")
    }

    fn visit_none<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(None)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(PathVisitor).map(Some)
    }
}

struct PathVisitor;

impl<'de> serde::de::Visitor<'de> for PathVisitor {
    type Value = Vec<String>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(formatter, "a string or an array of strings")
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(vec![v.into()])
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: de::SeqAccess<'de>,
    {
        let mut v = if let Some(l) = seq.size_hint() {
            Vec::with_capacity(l)
        } else {
            Vec::new()
        };
        while let Some(s) = seq.next_element()? {
            v.push(s);
        }
        Ok(v)
    }
}

fn deserialize_regex<'de, D>(deserializer: D) -> Result<Option<Regex>, D::Error>
where
    D: Deserializer<'de>,
{
    let s: Option<String> = Deserialize::deserialize(deserializer)?;

    match s {
        Some(s) => Regex::new(&s).map(Some).map_err(|e| {
            de::Error::custom(format!(
                r#"Invalid regex for 'allow' or 'deny': "{s}" - {e}"#
            ))
        }),

        None => Ok(None),
    }
}

fn serialize_allow<S>(v: &Option<Regex>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(
        &v.as_ref()
            .map_or_else(|| ".*".to_string(), |re| re.to_string()),
    )
}

fn serialize_deny<S>(v: &Option<Regex>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(
        &v.as_ref()
            .map_or_else(|| "".to_string(), |re| re.to_string()),
    )
}

struct MqttPortVisitor;

impl<'de> Visitor<'de> for MqttPortVisitor {
    type Value = String;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str(r#"either a port number as an integer or a string, either a string with format "<local_ip>:<port_number>""#)
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(format!("{DEFAULT_MQTT_INTERFACE}:{value}"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        let parts: Vec<&str> = value.split(':').collect();
        if parts.len() > 2 {
            return Err(E::invalid_value(Unexpected::Str(value), &self));
        }
        let (interface, port) = if parts.len() == 1 {
            (DEFAULT_MQTT_INTERFACE, parts[0])
        } else {
            (parts[0], parts[1])
        };
        if port.parse::<u32>().is_err() {
            return Err(E::invalid_value(Unexpected::Str(port), &self));
        }
        Ok(format!("{interface}:{port}"))
    }
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn test_path_field() {
        // See: https://github.com/eclipse-zenoh/zenoh-plugin-webserver/issues/19
        let config = serde_json::from_str::<Config>(r#"{"__path__": "/example/path"}"#);

        assert!(config.is_ok());
        let Config {
            __required__,
            __path__,
            ..
        } = config.unwrap();

        assert_eq!(__path__, Some(vec![String::from("/example/path")]));
        assert_eq!(__required__, None);
    }

    #[test]
    fn test_required_field() {
        // See: https://github.com/eclipse-zenoh/zenoh-plugin-webserver/issues/19
        let config = serde_json::from_str::<Config>(r#"{"__required__": true}"#);
        assert!(config.is_ok());
        let Config {
            __required__,
            __path__,
            ..
        } = config.unwrap();

        assert_eq!(__path__, None);
        assert_eq!(__required__, Some(true));
    }

    #[test]
    fn test_path_field_and_required_field() {
        // See: https://github.com/eclipse-zenoh/zenoh-plugin-webserver/issues/19
        let config = serde_json::from_str::<Config>(
            r#"{"__path__": "/example/path", "__required__": true}"#,
        );

        assert!(config.is_ok());
        let Config {
            __required__,
            __path__,
            ..
        } = config.unwrap();

        assert_eq!(__path__, Some(vec![String::from("/example/path")]));
        assert_eq!(__required__, Some(true));
    }

    #[test]
    fn test_no_path_field_and_no_required_field() {
        // See: https://github.com/eclipse-zenoh/zenoh-plugin-webserver/issues/19
        let config = serde_json::from_str::<Config>("{}");

        assert!(config.is_ok());
        let Config {
            __required__,
            __path__,
            ..
        } = config.unwrap();

        assert_eq!(__path__, None);
        assert_eq!(__required__, None);
    }
}
