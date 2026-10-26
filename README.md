# ipfs_module: an IPFS node for Logos Basecamp

A Basecamp core module that gives every Logos app on the device an IPFS node, with
nothing to install: the package bundles a static [Kubo](https://github.com/ipfs/kubo),
starts it the first time an app asks, and stops it when its host stops.

Apps then fetch, pin and provide content-addressed data through **their own node**
instead of a third-party gateway: no gateway operator sees what is read, pinned content
survives whoever pinned it first, and anything read once stays available locally.

## What you get

| Method | Returns |
|---|---|
| `endpoints()` | `{ ok, api, gateway, peerId }` of the running node, **starting it if needed** |
| `status()` | `{ running, api, gateway, peerId, peers, pins, repoSize, offline, error }` |
| `start()` / `stop()` | start or stop the node |
| `configure(json)` | `{ "autostart": bool, "offline": bool, "binary": "/path/to/ipfs" }`, omitted keys stay |
| `log_tail(lines)` | the last lines of the daemon log |

Event: `node_changed(running)`.

`api` is a normal Kubo RPC endpoint and `gateway` a normal (trustless) gateway, both on
random loopback ports, so any IPFS client library works unchanged. Use the module as an
**optional** dependency and fall back to public gateways when it is absent:

```json
"optional_dependencies": [{ "name": "ipfs_module", "version": "~0.1.0" }]
```

```rust
// Rust consumer (logos-rust-sdk): the endpoint pair, starting the node on first use
let raw = ipfs_module::IpfsModuleClient::new().endpoints_with_timeout(Duration::from_secs(60))?;
```

## How it runs

- One Kubo per device, repo in the module's data directory, API and gateway on free
  loopback ports chosen at each start.
- Spawned from a long-lived supervisor thread with `PR_SET_PDEATHSIG`, so the node dies
  with its host. (`PR_SET_PDEATHSIG` is per *thread*: spawning from a short-lived IPC
  worker killed the daemon as soon as that call returned.)
- **Desktop-friendly settings**, applied on every start. Kubo's defaults flooded a NAT-ed
  home link in testing (every other request on the machine timed out until the node
  stopped); the UDP transports were the cause. So the node runs:
  - Kubo's `lowpower` profile: DHT client only, no relay service, no AutoNAT service;
  - **TCP only** (QUIC, WebTransport, WebRTC and WebSocket off);
  - 10-20 connections kept, and a **hard cap of 64** through libp2p's resource manager
    (`libp2p-resource-limit-overrides.json`). The connection manager alone only trims after a
    grace period, so a starting node still burst to 318 peers and Gnosis RPC calls from the
    same machine timed out; with the cap it peaked at 44 and calls stayed around 60-150 ms;
  - announcing **pinned content only**, not every block it ever fetched.
- Linux: `pkgsStatic.kubo` (one static binary, no system libraries). macOS: nixpkgs `kubo`.

## Used by

- [Fileverse for Basecamp](https://github.com/vpavlin/fileverse-basecamp): reads Fileverse documents
  through the node first, feeds every verified block into it, pins backups and published drafts.

## Build and test

```sh
nix build .#lgx-portable                 # the .lgx with the bundled Kubo
cd rust-lib
FV_KUBO_BIN=/path/to/ipfs cargo test --no-default-features -- --ignored
```

The supervisor test starts the node from a thread that then exits (as an IPC worker does)
and checks the daemon survives, round-trips a block through API and gateway, checks the
desktop settings, and restarts with the same peer id.

The Rust crate needs the Logos SDK staged at `logos-rust-sdk-src` for the module build
(see the builder docs); the supervisor and its tests build without it.

## Licence

MIT or Apache-2.0. Kubo is MIT/Apache-2.0.
