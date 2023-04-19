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
use clap::{App, Arg};
use std::str::FromStr;
use zenoh::config::{Config, ModeDependentValue};
use zenoh::prelude::*;

macro_rules! insert_json5 {
    ($config: expr, $args: expr, $key: expr, if $name: expr) => {
        if $args.occurrences_of($name) > 0 {
            $config.insert_json5($key, "true").unwrap();
        }
    };
    ($config: expr, $args: expr, $key: expr, if $name: expr, $($t: tt)*) => {
        if $args.occurrences_of($name) > 0 {
            $config.insert_json5($key, &serde_json::to_string(&$args.value_of($name).unwrap()$($t)*).unwrap()).unwrap();
        }
    };
    ($config: expr, $args: expr, $key: expr, for $name: expr, $($t: tt)*) => {
        if let Some(value) = $args.values_of($name) {
            $config.insert_json5($key, &serde_json::to_string(&value$($t)*).unwrap()).unwrap();
        }
    };
}

fn parse_args() -> Config {
    let app = App::new("zenoh bridge for MQTT")
        .version(zplugin_mqtt::GIT_VERSION)
        .long_version(zplugin_mqtt::LONG_VERSION.as_str())
        //
        // zenoh related arguments:
        //
        .arg(Arg::from_usage(
r#"-i, --id=[HEX_STRING] \
'The identifier (as an hexadecimal string, with odd number of chars - e.g.: 0A0B23...) that zenohd must use.
WARNING: this identifier must be unique in the system and must be 16 bytes maximum (32 chars)!
If not set, a random UUIDv4 will be used.'"#,
            ))
        .arg(Arg::from_usage(
r#"-m, --mode=[MODE]  'The zenoh session mode.'"#)
            .possible_values(["peer", "client"])
            .default_value("peer")
        )
        .arg(Arg::from_usage(
r#"-c, --config=[FILE] \
'The configuration file. Currently, this file must be a valid JSON5 file.'"#,
            ))
        .arg(Arg::from_usage(
r#"-l, --listen=[ENDPOINT]... \
'A locator on which this router will listen for incoming sessions.
Repeat this option to open several listeners.'"#,
                ),
            )
        .arg(Arg::from_usage(
r#"-e, --connect=[ENDPOINT]... \
'A peer locator this router will try to connect to.
Repeat this option to connect to several peers.'"#,
            ))
        .arg(Arg::from_usage(
r#"--no-multicast-scouting \
'By default the zenoh bridge listens and replies to UDP multicast scouting messages for being discovered by peers and routers.
This option disables this feature.'"#
        ))
        .arg(Arg::from_usage(
r#"--rest-http-port=[PORT | IP:PORT] \
'Configures HTTP interface for the REST API (disabled by default, setting this option enables it). Accepted values:'
  - a port number
  - a string with format `<local_ip>:<port_number>` (to bind the HTTP server to a specific interface)."#
        ))
        //
        // MQTT related arguments:
        //
        .arg(Arg::from_usage(
r#"-p, --port=[PORT | IP:PORT] \
'The address to bind the MQTT server. Default: "0.0.0.0:1883". Accepted values:'
    - a port number ("0.0.0.0" will be used as IP to bind, meaning any interface of the host)
    - a string with format `<local_ip>:<port_number>` (to bind the MQTT server to a specific interface)."#
        ))
        .arg(Arg::from_usage(
r#"-s, --scope=[String]   'A string added as prefix to all routed MQTT topics when mapped to a zenoh key expression. This should be used to avoid conflicts when several distinct MQTT systems using the same topics names are routed via zenoh'"#
        ))
        .arg(Arg::from_usage(
r#"-a, --allow=[String]   'A regular expression matching the MQTT topic name that must be routed via zenoh. By default topics are allowed.
If both '--allow' and '--deny' are set a topic will be allowed if it matches only the 'allow' expression."#
        ))
        .arg(Arg::from_usage(
r#"--deny=[String]   'A regular expression matching the MQTT topic name that must not be routed via zenoh. By default no topics are denied.
If both '--allow' and '--deny' are set a topic will be allowed if it matches only the 'allow' expression."#
        ))
        .arg(Arg::from_usage(
r#"-r, --generalise-sub=[String]...   'A list of key expression to use for generalising subscriptions (usable multiple times).'"#
        ))
        .arg(Arg::from_usage(
r#"-w, --generalise-pub=[String]...   'A list of key expression to use for generalising publications (usable multiple times).'"#
        ));
    let args = app.get_matches();

    // load config file at first
    let mut config = match args.value_of("config") {
        Some(conf_file) => Config::from_file(conf_file).unwrap(),
        None => Config::default(),
    };
    // if "mqtt" plugin conf is not present, add it (empty to use default config)
    if config.plugin("mqtt").is_none() {
        config.insert_json5("plugins/mqtt", "{}").unwrap();
    }

    // apply zenoh related arguments over config
    // NOTE: only if args.occurrences_of()>0 to avoid overriding config with the default arg value
    if args.occurrences_of("id") > 0 {
        config
            .set_id(ZenohId::from_str(args.value_of("id").unwrap()).unwrap())
            .unwrap();
    }
    if args.occurrences_of("mode") > 0 {
        config
            .set_mode(Some(args.value_of("mode").unwrap().parse().unwrap()))
            .unwrap();
    }
    if let Some(endpoints) = args.values_of("connect") {
        config
            .connect
            .endpoints
            .extend(endpoints.map(|p| p.parse().unwrap()))
    }
    if let Some(endpoints) = args.values_of("listen") {
        config
            .listen
            .endpoints
            .extend(endpoints.map(|p| p.parse().unwrap()))
    }
    if args.is_present("no-multicast-scouting") {
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
    }
    if let Some(port) = args.value_of("rest-http-port") {
        config
            .insert_json5("plugins/rest/http_port", &format!(r#""{port}""#))
            .unwrap();
    }

    // Always add timestamps to publications (required for PublicationCache used in case of TRANSIENT_LOCAL topics)
    config
        .timestamping
        .set_enabled(Some(ModeDependentValue::Unique(true)))
        .unwrap();

    // apply MQTT related arguments over config
    insert_json5!(config, args, "plugins/mqtt/port", if "port",);
    insert_json5!(config, args, "plugins/mqtt/scope", if "scope",);
    insert_json5!(config, args, "plugins/mqtt/allow", if "allow", );
    insert_json5!(config, args, "plugins/mqtt/deny", if "deny", );
    insert_json5!(config, args, "plugins/mqtt/generalise_pubs", for "generalise-pub", .collect::<Vec<_>>());
    insert_json5!(config, args, "plugins/mqtt/generalise_subs", for "generalise-sub", .collect::<Vec<_>>());
    config
}

#[async_std::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("z=info")).init();
    log::info!("zenoh-bridge-mqtt {}", *zplugin_mqtt::LONG_VERSION);

    let config = parse_args();
    let rest_plugin = config.plugin("rest").is_some();

    // create a zenoh Runtime (to share with plugins)
    let runtime = zenoh::runtime::Runtime::new(config).await.unwrap();

    // start REST plugin
    if rest_plugin {
        use zenoh_plugin_trait::Plugin;
        zplugin_rest::RestPlugin::start("rest", &runtime).unwrap();
    }

    // start MQTT plugin
    use zenoh_plugin_trait::Plugin;
    zplugin_mqtt::MqttPlugin::start("mqtt", &runtime).unwrap();
    async_std::future::pending::<()>().await;
}
