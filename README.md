<img src="https://raw.githubusercontent.com/eclipse-zenoh/zenoh/master/zenoh-dragon.png" height="150">

[![CI](https://github.com/eclipse-zenoh/zenoh-plugin-mqtt/workflows/Rust/badge.svg)](https://github.com/eclipse-zenoh/zenoh-plugin-mqtt/actions?query=workflow%3ARust)
[![Discussion](https://img.shields.io/badge/discussion-on%20github-blue)](https://github.com/eclipse-zenoh/roadmap/discussions)
[![Discord](https://img.shields.io/badge/chat-on%20discord-blue)](https://discord.gg/2GJ958VuHs)
[![License](https://img.shields.io/badge/License-EPL%202.0-blue)](https://choosealicense.com/licenses/epl-2.0/)
[![License](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)

# Eclipse Zenoh
The Eclipse Zenoh: Zero Overhead Pub/sub, Store/Query and Compute.

Zenoh (pronounce _/zeno/_) unifies data in motion, data at rest and computations. It carefully blends traditional pub/sub with geo-distributed storages, queries and computations, while retaining a level of time and space efficiency that is well beyond any of the mainstream stacks.

Check the website [zenoh.io](http://zenoh.io) and the [roadmap](https://github.com/eclipse-zenoh/roadmap) for more detailed information.

-------------------------------
# MQTT plugin for Eclipse Zenoh, and standalone `zenoh-bridge-mqtt`

<!-- :point_right: **Download stable versions:** https://download.eclipse.org/zenoh/zenoh-plugin-mqtt/ -->

<!-- :point_right: **Docker image:** see [below](#Docker-image) -->

:point_right: **Build "master" branch:** see [below](#How-to-build-it)

## Background

[MQTT](https://mqtt.org/) is a pub/sub protocol leveraging a broker to route the messages between the MQTT clients.
The MQTT plugin for Eclipse Zenoh acts as a MQTT broker, accepting connections from MQTT clients (V3 and V5) and translating the MQTT pub/sub into a Zenoh pub/sub.
I.e.:
 - a MQTT publication on topic `device/123/temperature` is routed as a Zenoh publication on key expression `device/123/temperature`
 - a MQTT subscription on topic `device/#` is mapped to a Zenoh subscription on key expression `device/**`

This allows a close intergration of any MQTT system with Zenoh, but also brings to MQTT systems the benefits of a Zenoh routing infrastructure.
Some examples of use cases:
 - Routing MQTT from the device to the Edge and to the Cloud
 - Bridging 2 distinct MQTT systems across the Internet, with NAT traversal
 - Pub/sub to MQTT via the Zenoh REST API
 - MQTT-ROS2 (robot) communication
 - Store MQTT publications in any Zenoh storage (RocksDB, InfluxDB, file system...)
 - MQTT record/replay with InfluxDB as a storage

The MQTT plugin for Eclipse Zenoh is available either as a dynamic library to be loaded by the Zenoh router (`zenohd`), either as a standalone executable (`zenoh-bridge-mqtt`) that can acts as a Zenoh client or peer.

## Configuration

`zenoh-bridge-mqtt` can be configured via a JSON5 file passed via the `-c`argument. You can see a commented example of such configuration file: [`DEFAULT_CONFIG.json5`](DEFAULT_CONFIG.json5).

The `"mqtt"` part of this same configuration file can also be used in the configuration file for the zenoh router (within its `"plugins"` part). The router will automatically try to load the plugin library (`zplugin_mqtt`) at startup and apply its configuration.

`zenoh-bridge-mqtt` also accepts the following arguments. If set, each argument will override the similar setting from the configuration file:
 * zenoh-related arguments:
   - **`-c, --config <FILE>`** : a config file
   - **`-m, --mode <MODE>`** : The zenoh session mode. Default: `peer` Possible values: `peer` or `client`.  
      See [zenoh documentation](https://zenoh.io/docs/getting-started/key-concepts/#deployment-units) for more details.
   - **`-l, --listen <LOCATOR>`** : A locator on which this router will listen for incoming sessions. Repeat this option to open several listeners. Example of locator: `tcp/localhost:7447`.
   - **`-e, --peer <LOCATOR>`** : A peer locator this router will try to connect to (typically another bridge or a zenoh router). Repeat this option to connect to several peers. Example of locator: `tcp/<ip-address>:7447`.
   - **`--no-multicast-scouting`** : disable the zenoh scouting protocol that allows automatic discovery of zenoh peers and routers.
   - **`-i, --id <hex_string>`** : The identifier (as an hexadecimal string - e.g.: 0A0B23...) that the zenoh bridge must use. **WARNING: this identifier must be unique in the system!** If not set, a random UUIDv4 will be used.
   - **`--rest-http-port [PORT | IP:PORT]`** : Configures HTTP interface for the REST API (disabled by default, setting this option enables it). Accepted values:
       - a port number
       - a string with format `<local_ip>:<port_number>` (to bind the HTTP server to a specific interface).
 * MQTT-related arguments:
   - **`-p, --port [PORT | IP:PORT]`** : The address to bind the MQTT server. Default: `"0.0.0.0:1883"`. Accepted values:
       - a port number (`"0.0.0.0"` will be used as IP to bind, meaning any interface of the host)
       - a string with format `<local_ip>:<port_number>` (to bind the MQTT server to a specific interface).
   - **`-s, --scope <String>`** : A string added as prefix to all routed MQTT topics when mapped to a zenoh key expression. This should be used to avoid conflicts when several distinct MQTT systems using the same topics names are routed via Zenoh.
   - **`-a, --allow <String>`** :  A regular expression matching the MQTT topic name that must be routed via zenoh. By default all topics are allowed. If both `--allow` and `--deny` are set a topic will be allowed if it matches only the 'allow' expression.
   - **`--deny <String>`** :  A regular expression matching the MQTT topic name that must not be routed via zenoh. By default no topics are denied. If both `--allow` and `--deny` are set a topic will be allowed if it matches only the 'allow' expression.
   - **`-w, --generalise-pub <String>`** :  A list of key expressions to use for generalising the declaration of
     the zenoh publications, and thus minimizing the discovery traffic (usable multiple times).
     See [this blog](https://zenoh.io/blog/2021-03-23-discovery/#leveraging-resource-generalisation) for more details.
   - **`-r, --generalise-sub <String>`** :  A list of key expressions to use for generalising the declaration of
     the zenoh subscriptions, and thus minimizing the discovery traffic (usable multiple times).
     See [this blog](https://zenoh.io/blog/2021-03-23-discovery/#leveraging-resource-generalisation) for more details.

## Admin space

The zenoh bridge for MQTT exposes an administration space allowing to get some information on its status and configuration.
This administration space is accessible via any zenoh API, including the REST API that you can activate at `zenoh-bridge-mqtt` startup using the `--rest-http-port` argument.

The `zenoh-bridge-mqtt` exposes this administration space with paths prefixed by `@/service/<uuid>/mqtt` (where `<uuid>` is the unique identifier of the bridge instance). The informations are then organized with such paths:
 - `@/service/<uuid>/mqtt/version` : the bridge version
 - `@/service/<uuid>/mqtt/config` : the bridge configuration

Example of queries on administration space using the REST API with the `curl` command line tool (don't forget to activate the REST API with `--rest-http-port 8000` argument):
 - ```bash
   curl http://localhost:8000:/@/service/**
   ```

> _Pro tip: pipe the result into [**jq**](https://stedolan.github.io/jq/) command for JSON pretty print or transformation._


## How to build it

> :warning: **WARNING** :warning: : Zenoh and its ecosystem are under active development. When you build from git, make sure you also build from git any other Zenoh repository you plan to use (e.g. binding, plugin, backend, etc.). It may happen that some changes in git are not compatible with the most recent packaged Zenoh release (e.g. deb, docker, pip). We put particular effort in mantaining compatibility between the various git repositories in the Zenoh project.

In order to build the zenoh bridge for MQTT you only need to install [Rust](https://www.rust-lang.org/tools/install).

Then, you may clone the repository on your machine:

```bash
$ git clone https://github.com/eclipse-zenoh/zenoh-plugin-mqtt.git
$ cd zenoh-plugin-mqtt
```

You can then choose between building the zenoh bridge for MQTT:
- as a plugin library that can be dynamically loaded by the zenoh router (`zenohd`):
```bash
$ cargo build --release -p zplugin-mqtt
```
The plugin shared library (`*.so` on Linux, `*.dylib` on Mac OS, `*.dll` on Windows) will be generated in the `target/release` subdirectory.

- or as a standalone executable binary:
```bash
$ cargo build --release -p zenoh-bridge-mqtt
```
The **`zenoh-bridge-mqtt`** binary will be generated in the `target/release` sub-directory.
