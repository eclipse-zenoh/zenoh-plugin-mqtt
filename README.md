<img src="https://raw.githubusercontent.com/eclipse-zenoh/zenoh/main/zenoh-dragon.png" height="150">

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

# MQTT plugin and standalone `zenoh-bridge-mqtt`

:point_right: **Install latest release:** see [below](#how-to-install-it)

:point_right: **Docker image:** see [below](#docker-image)

:point_right: **Build "main" branch:** see [below](#how-to-build-it)

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

## MQTT Retained Messages

The MQTT plugin supports MQTT retained messages by leveraging Zenoh's storage layer. When enabled, messages published with the RETAIN flag set are stored in Zenoh storage and delivered to new subscribers.

### Configuration

Retained messages require both:
1. The `retained_enabled` option in the MQTT plugin configuration (default: `true`)
2. A Zenoh storage configured for the `__retained__/**` key expression pattern

#### Example Configuration

For in-memory retained messages (non-persistent):

```json5
{
  "plugins": {
    "mqtt": {
      "port": "0.0.0.0:1883",
      "retained_enabled": true  // Default
    },
    "storage_manager": {
      "storages": {
        "mqtt_retained": {
          "key_expr": "__retained__/**",
          "volume": { "id": "memory" },
          "complete": true
        }
      }
    }
  }
}
```

For persistent retained messages using RocksDB:

```json5
{
  "plugins": {
    "mqtt": {
      "port": "0.0.0.0:1883",
      "scope": "mqtt"  // Optional scope
    },
    "storage_manager": {
      "volumes": {
        "rocksdb_volume": {
          "id": "rocksdb",
          "db_path": "/var/zenoh/mqtt_retained"
        }
      },
      "storages": {
        "mqtt_retained": {
          "key_expr": "mqtt/__retained__/**",  // Match scope if used
          "volume": { "id": "rocksdb_volume" },
          "complete": true
        }
      }
    }
  }
}
```

**Important:** The `key_expr` must match your MQTT plugin's scope:
- Without scope: `__retained__/**`
- With scope `mqtt`: `mqtt/__retained__/**`

The `complete: true` setting is required for retained messages to work correctly.

### Disabling Retained Messages

To disable retained message support, set `retained_enabled` to `false` in the MQTT plugin configuration.

## Configuration

`zenoh-bridge-mqtt` can be configured via a JSON5 file passed via the `-c`argument. You can see a commented example of such configuration file: [`DEFAULT_CONFIG.json5`](DEFAULT_CONFIG.json5).

The `"mqtt"` part of this same configuration file can also be used in the configuration file for the zenoh router (within its `"plugins"` part). The router will automatically try to load the plugin library (`zenoh_plugin_mqtt`) at startup and apply its configuration.

`zenoh-bridge-mqtt` also accepts the following arguments. If set, each argument will override the similar setting from the configuration file:

- zenoh-related arguments:
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
- MQTT-related arguments:
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
  - **`--tx-channel-size <Unsigned Integer>`** :  Size of the MQTT transmit channel (default: 65536). The channel buffers messages from Zenoh until they can be sent to MQTT clients. If the channel capacity is reached new messages from Zenoh will be dropped until space becomes available.
  - **`--dictionary-file <FILE>`** :  Path to a file containing the MQTT client username/password dictionary.
  - **`--server-private-key <FILE>`** :  Path to the TLS private key for the MQTT server. If specified a valid certificate for the server must also be provided.
  - **`--server-certificate <FILE>`** :  Path to the TLS public certificate for the MQTT server. If specified a valid private key for the server must also be provided.
  - **`--root-ca-certificate <FILE>`** :  Path to the certificate of the certificate authority used to validate clients connecting to the MQTT server. If specified a valid private key and certificate for the server must also be provided.

## Admin space

The zenoh bridge for MQTT exposes an administration space allowing to get some information on its status and configuration.
This administration space is accessible via any zenoh API, including the REST API that you can activate at `zenoh-bridge-mqtt` startup using the `--rest-http-port` argument.

The `zenoh-bridge-mqtt` exposes this administration space with paths prefixed by `@/service/<uuid>/mqtt` (where `<uuid>` is the unique identifier of the bridge instance). The informations are then organized with such paths:

- `@/service/<uuid>/mqtt/version` : the bridge version
- `@/service/<uuid>/mqtt/config` : the bridge configuration

Example of queries on administration space using the REST API with the `curl` command line tool (don't forget to activate the REST API with `--rest-http-port 8000` argument):

```bash
curl http://localhost:8000:/@/service/**
```

> _Pro tip: pipe the result into [**jq**](https://stedolan.github.io/jq/) command for JSON pretty print or transformation._

## MQTTS support

The MQTT plugin and standalone bridge for Eclipse Zenoh supports MQTTS. MQTTS can be configured in two ways:

- server side authentication: MQTT clients validate the servers TLS certificate but not the other way around.
- mutual authentication (mTLS): where both server and clients validate each other.

 MQTTS can be configured via the configuration file or, if using the standalone bridge, via command line arguments.

### Server side authentication configuration

Server side authentication requires both a private key and certificate for the server. These can be provided as either a file or as a base 64 encoded string.

In the configuration file, the required **tls** fields when using files are **server_private_key** and **server_certificate**. When using base 64 encoded strings the required **tls** fields are **server_private_key_base64** and **server_certificate_base64**.

An example configuration file supporting server side authentication would be:

```json
{
  "plugins": {
    "mqtt": {
      "tls": {
        "server_private_key": "/path/to/private-key.pem",
        "server_certificate": "/path/to/certificate.pem"
      }
    }
  }
}
```

The standalone bridge (`zenoh-bridge-mqtt`) also allows the required files to be provided through the **`--server-private-key`** and **`--server-certificate`** command line arguments.

### Mutual authentication (mTLS) configuration

In order to enable mutual authentication a certificate for the certificate authority used to validate clients connecting to the MQTT server must also be provided. This can be provided as either a file or a base 64 encoded string.

In the configuration file, the required **tls** field when using a file is **root_ca_certificate**. When using base 64 encoded strings the required **tls** field when using a file is **root_ca_certificate_base64**.

An example configuration file supporting server side authentication would be:

```json
{
  "plugins": {
    "mqtt": {
      "tls": {
        "server_private_key": "/path/to/private-key.pem",
        "server_certificate": "/path/to/certificate.pem",
        "root_ca_certificate": "/path/to/root-ca-certificate.pem"
      }
    }
  }
}
```

The standalone bridge (`zenoh-bridge-mqtt`) also allows the required file to be provided through the **`--root-ca-certificate`** command line argument.

## Username/password authentication

The MQTT plugin and standalone bridge for Eclipse Zenoh supports basic username/password authentication of MQTT clients. Credentials are provided via a dictionary file with each line containing the username and password for a single user in the following format:

```raw
username:password
```

Username/passord authentication can be configured via the configuration file or, if using the standalone bridge, via command line arguments.

In the configuration file, the required **auth** field for configuring the dictionary file is **dictionary_file**.

An example configuration file supporting username/password authentication would be:

```json
{
  "plugins": {
    "mqtt": {
      "auth": {
        "dictionary_file": "/path/to/dictionary-file",
      }
    }
  }
}
```

The standalone bridge (`zenoh-bridge-mqtt`) also allows the required file to be provided through the **`--dictionary-file`** command line argument.

### Security considerations

Usernames and passwords are sent as part of the MQTT `CONNECT` message in clear text. As such, they can potentially be viewed using tools such as [Wireshark](https://www.wireshark.org/).

To prevent this, it is highly recommended that this feature is used in conjunction with the MQTTS feature to ensure credentials are encrypted on the wire.

## How to install it

To install the latest release of either the MQTT plugin for the Zenoh router, either the `zenoh-bridge-mqtt` standalone executable, you can do as follows:

### Manual installation (all platforms)

All release packages can be downloaded from:  

- [https://download.eclipse.org/zenoh/zenoh-plugin-mqtt/latest/](https://download.eclipse.org/zenoh/zenoh-plugin-mqtt/latest/)

Each subdirectory has the name of the Rust target. See the platforms each target corresponds to on [https://doc.rust-lang.org/stable/rustc/platform-support.html](https://doc.rust-lang.org/stable/rustc/platform-support.html)

Choose your platform and download:

- the `zenoh-plugin-mqtt-<version>-<platform>.zip` file for the plugin.  
  Then unzip it in the same directory than `zenohd` or to any directory where it can find the plugin library (e.g. /usr/lib)
- the `zenoh-bridge-mqtt-<version>-<platform>.zip` file for the standalone executable.  
  Then unzip it where you want, and run the extracted `zenoh-bridge-mqtt` binary.

### Linux Debian

Add Eclipse Zenoh private repository to the sources list:

```bash
echo "deb [trusted=yes] https://download.eclipse.org/zenoh/debian-repo/ /" | sudo tee -a /etc/apt/sources.list > /dev/null
sudo apt update
```

Then either:

- install the plugin with: `sudo apt install zenoh-plugin-mqtt`.
- install the standalone executable with: `sudo apt install zenoh-bridge-mqtt`.

## Docker image

The **`zenoh-bridge-mqtt`** standalone executable is also available as a [Docker images](https://hub.docker.com/r/eclipse/zenoh-bridge-mqtt/tags?page=1&ordering=last_updated) for both amd64 and arm64. To get it, do:

- `docker pull eclipse/zenoh-bridge-mqtt:latest` for the latest release
- `docker pull eclipse/zenoh-bridge-mqtt:main` for the main branch version (nightly build)

Usage: **`docker run --init -p 1883:1883 eclipse/zenoh-bridge-mqtt`**  
It supports the same command line arguments than the `zenoh-bridge-mqtt` (see above or check with `-h` argument).

## How to build it

> :warning: **WARNING** :warning: : As Rust doesn't have a stable ABI, the plugins should be
built with the exact same Rust version than `zenohd`, and using for `zenoh` dependency the same version (or commit number) than 'zenohd'.
Otherwise, incompatibilities in memory mapping of shared types between `zenohd` and the library can lead to a `"SIGSEV"` crash.

In order to build the zenoh bridge for MQTT you only need to install [Rust](https://www.rust-lang.org/tools/install). If you already have the Rust toolchain installed, make sure it is up-to-date with:

```bash
rustup update
```

Then, you may clone the repository on your machine:

```bash
git clone https://github.com/eclipse-zenoh/zenoh-plugin-mqtt.git
cd zenoh-plugin-mqtt
cargo build --release
```

The standalone executable binary `zenoh-bridge-mqtt` and a plugin shared library (`*.so` on Linux, `*.dylib` on Mac OS, `*.dll` on Windows) to be dynamically
loaded by the zenoh router `zenohd` will be generated in the `target/release` subdirectory.
