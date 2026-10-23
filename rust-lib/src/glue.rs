//! Logos module glue for `ipfs_module` (rust-first authoring).
//!
//! Every method returns `{ "ok": true, ... }` or `{ "ok": false, "error": "..." }`.
//! `endpoints()` starts the node on first use, so a consumer needs no setup step:
//! it asks for the endpoints and talks Kubo RPC / trustless gateway HTTP to them.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::kubo::Kubo;

pub trait IpfsModule: Send + Sync + 'static {
    /// Start the node if it is not running. Returns its status.
    fn start(&self) -> String;
    /// Stop the node.
    fn stop(&self) -> String;
    /// `{ running, api, gateway, peerId, offline, peers, repoSize, pins, error }`.
    fn status(&self) -> String;
    /// `{ api, gateway }` of a running node, starting it if needed.
    fn endpoints(&self) -> String;
    /// `{ "autostart": bool, "offline": bool, "binary": "/path/to/ipfs" }`; omitted keys stay.
    fn configure(&self, config_json: String) -> String;
    /// The last lines of the daemon log.
    fn log_tail(&self, lines: i64) -> String;

    fn on_context_ready(&self, _ctx: &RustModuleContext) {}
}

pub trait IpfsModuleEvents {
    /// The node started, stopped or failed: re-read `status()`.
    fn node_changed(&self, running: bool);
}

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/generated/provider_gen.rs"));

const START_WAIT: Duration = Duration::from_secs(90);

#[derive(Default)]
struct Settings {
    autostart: bool,
    offline: bool,
    binary: String,
}

#[derive(Default)]
struct IpfsModuleImpl {
    // `Mutex` comes from the generated scaffold.
    node: Arc<Mutex<Option<Kubo>>>,
    settings: Arc<Mutex<Settings>>,
    dir: Arc<Mutex<Option<PathBuf>>>,
}

fn err(e: impl std::fmt::Display) -> String {
    json!({ "ok": false, "error": e.to_string() }).to_string()
}

impl IpfsModuleImpl {
    fn settings_path(&self) -> Option<PathBuf> {
        self.dir.lock().unwrap().as_ref().map(|d| d.join("settings.json"))
    }

    fn save(&self) {
        let s = self.settings.lock().unwrap();
        if let Some(p) = self.settings_path() {
            let _ = std::fs::write(p, json!({ "autostart": s.autostart, "offline": s.offline, "binary": s.binary }).to_string());
        }
    }

    fn binary(&self) -> PathBuf {
        let s = self.settings.lock().unwrap();
        if !s.binary.is_empty() {
            return PathBuf::from(&s.binary);
        }
        if let Some(d) = crate::plugin_dir() {
            let p = d.join("ipfs");
            if p.exists() {
                return p;
            }
        }
        PathBuf::from("ipfs")
    }

    fn with_node<T>(&self, f: impl FnOnce(&mut Kubo) -> T) -> Result<T, String> {
        let mut g = self.node.lock().unwrap();
        if g.is_none() {
            let dir = self.dir.lock().unwrap().clone().ok_or("module not ready yet")?;
            *g = Some(Kubo::new(self.binary(), dir.join("repo")));
        }
        let k = g.as_mut().unwrap();
        k.offline = self.settings.lock().unwrap().offline;
        Ok(f(k))
    }

    fn start_node(&self) -> Result<Value, String> {
        let r = self.with_node(|k| k.start(START_WAIT))?;
        emit_node_changed(r.is_ok());
        r
    }
}

impl IpfsModule for IpfsModuleImpl {
    fn on_context_ready(&self, ctx: &RustModuleContext) {
        let dir = PathBuf::from(&ctx.instance_persistence_path);
        let _ = std::fs::create_dir_all(&dir);
        *self.dir.lock().unwrap() = Some(dir.clone());
        let mut s = self.settings.lock().unwrap();
        s.autostart = true;
        if let Ok(b) = std::fs::read(dir.join("settings.json")) {
            if let Ok(v) = serde_json::from_slice::<Value>(&b) {
                s.autostart = v["autostart"].as_bool().unwrap_or(true);
                s.offline = v["offline"].as_bool().unwrap_or(false);
                s.binary = v["binary"].as_str().unwrap_or("").to_string();
            }
        }
    }

    fn start(&self) -> String {
        match self.start_node() {
            Ok(v) => v.to_string(),
            Err(e) => err(e),
        }
    }

    fn stop(&self) -> String {
        let _ = self.with_node(|k| k.stop());
        emit_node_changed(false);
        json!({ "ok": true, "running": false }).to_string()
    }

    fn status(&self) -> String {
        match self.with_node(|k| {
            let mut v = k.status_json();
            if v["running"] == true {
                if let (Some(o), Value::Object(st)) = (v.as_object_mut(), k.stats()) {
                    o.extend(st);
                }
            }
            v
        }) {
            Ok(v) => v.to_string(),
            Err(e) => err(e),
        }
    }

    fn endpoints(&self) -> String {
        let running = self.with_node(|k| k.running()).unwrap_or(false);
        if !running {
            if !self.settings.lock().unwrap().autostart {
                return err("the IPFS node is stopped (autostart is off)");
            }
            if let Err(e) = self.start_node() {
                return err(e);
            }
        }
        match self.with_node(|k| json!({ "ok": true, "api": k.api, "gateway": k.gateway, "peerId": k.peer_id })) {
            Ok(v) => v.to_string(),
            Err(e) => err(e),
        }
    }

    fn configure(&self, config_json: String) -> String {
        let v: Value = match serde_json::from_str(&config_json) {
            Ok(v) => v,
            Err(e) => return err(format!("config is not JSON: {e}")),
        };
        {
            let mut s = self.settings.lock().unwrap();
            if let Some(b) = v["autostart"].as_bool() {
                s.autostart = b;
            }
            if let Some(b) = v["offline"].as_bool() {
                s.offline = b;
            }
            if let Some(b) = v["binary"].as_str() {
                s.binary = b.to_string();
            }
        }
        self.save();
        // A new binary applies on the next start.
        if v.get("binary").is_some() {
            let mut g = self.node.lock().unwrap();
            if let Some(k) = g.as_mut() {
                k.stop();
            }
            *g = None;
        }
        let s = self.settings.lock().unwrap();
        json!({ "ok": true, "autostart": s.autostart, "offline": s.offline, "binary": s.binary }).to_string()
    }

    fn log_tail(&self, lines: i64) -> String {
        let path = match self.with_node(|k| k.log_path()) {
            Ok(p) => p,
            Err(e) => return err(e),
        };
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let all: Vec<&str> = text.lines().collect();
        let n = lines.clamp(1, 500) as usize;
        let tail = all[all.len().saturating_sub(n)..].join("\n");
        json!({ "ok": true, "log": tail }).to_string()
    }
}

#[no_mangle]
pub extern "Rust" fn logos_module_install() {
    install::<IpfsModuleImpl>();
}
