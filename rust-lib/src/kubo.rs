//! Supervises one Kubo daemon: its repo lives in the module's data directory,
//! its RPC API and gateway listen on free loopback ports, and it dies with the
//! process that started it (PR_SET_PDEATHSIG on Linux), so a crashed host never
//! leaves an orphaned node behind.

use std::io::Read;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// PR_SET_PDEATHSIG fires when the *thread* that spawned the child exits, not the
/// process. Module calls run on short-lived IPC worker threads, so the daemon is
/// spawned from this one thread, which lives as long as the module.
fn spawn_on_supervisor(mut cmd: Command) -> std::io::Result<Child> {
    use std::sync::mpsc::{channel, Sender};
    use std::sync::{Mutex, OnceLock};
    type Job = (Command, Sender<std::io::Result<Child>>);
    static SUPERVISOR: OnceLock<Mutex<Sender<Job>>> = OnceLock::new();
    let tx = SUPERVISOR.get_or_init(|| {
        let (tx, rx) = channel::<Job>();
        std::thread::Builder::new()
            .name("ipfs-supervisor".into())
            .spawn(move || {
                for (mut cmd, reply) in rx {
                    let _ = reply.send(cmd.spawn());
                }
            })
            .expect("supervisor thread");
        Mutex::new(tx)
    });
    let (rtx, rrx) = channel();
    #[cfg(target_os = "linux")]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    tx.lock().unwrap().send((cmd, rtx)).map_err(|e| std::io::Error::other(e.to_string()))?;
    rrx.recv().map_err(|e| std::io::Error::other(e.to_string()))?
}

/// Kubo's `lowpower` profile, plus announcing only pinned content (what this device
/// chose to keep or publish) instead of every block it ever fetched.
pub const DESKTOP_CONFIG: &[(&str, &str)] = &[
    ("Routing.Type", "\"autoclient\""),
    ("Swarm.ConnMgr", r#"{"Type":"basic","LowWater":10,"HighWater":20,"GracePeriod":"20s"}"#),
    ("Swarm.RelayService", r#"{"Enabled":false}"#),
    ("AutoNAT", r#"{"ServiceMode":"disabled"}"#),
    // UDP transports (QUIC, WebTransport, WebRTC) were what flooded a NAT-ed link in
    // testing: with them every request on the machine timed out at 20 s, TCP-only kept
    // ~200 ms with 130+ peers and still fetched in a few seconds.
    ("Swarm.Transports.Network", r#"{"QUIC":false,"WebTransport":false,"WebRTCDirect":false,"Websocket":false}"#),
];

/// The announce strategy moved from `Reprovider.Strategy` to `Provide.Strategy` in
/// Kubo 0.38 (and older keys then stop the daemon), so pick by version.
fn provide_strategy_key(bin: &Path, repo: &Path) -> &'static str {
    let v = run(bin, repo, &["version", "--number"]).unwrap_or_default();
    let mut it = v.trim().split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let (major, minor) = (it.next().unwrap_or(0), it.next().unwrap_or(0));
    if major > 0 || minor >= 38 { "Provide.Strategy" } else { "Reprovider.Strategy" }
}

pub struct Kubo {
    pub bin: PathBuf,
    pub repo: PathBuf,
    pub offline: bool,
    child: Option<Child>,
    pub api: String,
    pub gateway: String,
    pub peer_id: String,
    pub last_error: String,
}

fn free_port() -> Result<u16, String> {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .map_err(|e| format!("no free port: {e}"))
}

fn run(bin: &Path, repo: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new(bin)
        .args(args)
        .env("IPFS_PATH", repo)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not run {}: {e}", bin.display()))?;
    if !out.status.success() {
        return Err(format!(
            "ipfs {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

impl Kubo {
    pub fn new(bin: PathBuf, repo: PathBuf) -> Self {
        Kubo {
            bin,
            repo,
            offline: false,
            child: None,
            api: String::new(),
            gateway: String::new(),
            peer_id: String::new(),
            last_error: String::new(),
        }
    }

    pub fn running(&mut self) -> bool {
        match self.child.as_mut().map(|c| c.try_wait()) {
            Some(Ok(None)) => true,
            Some(Ok(Some(status))) => {
                self.last_error = format!("ipfs daemon exited ({status}); see {}", self.log_path().display());
                self.child = None;
                false
            }
            _ => false,
        }
    }

    pub fn log_path(&self) -> PathBuf {
        self.repo.with_extension("log")
    }

    /// Initialise the repo once, then point API and gateway at fresh loopback ports.
    fn prepare(&mut self) -> Result<(u16, u16), String> {
        if !self.repo.join("config").exists() {
            std::fs::create_dir_all(&self.repo).map_err(|e| e.to_string())?;
            // randomports: the swarm port is random too, so two nodes on one machine never clash.
            run(&self.bin, &self.repo, &["init", "--profile=randomports"])?;
        }
        let (api, gw) = (free_port()?, free_port()?);
        run(&self.bin, &self.repo, &["config", "Addresses.API", &format!("/ip4/127.0.0.1/tcp/{api}")])?;
        run(&self.bin, &self.repo, &["config", "Addresses.Gateway", &format!("/ip4/127.0.0.1/tcp/{gw}")])?;
        // A desktop citizen, applied on every start so older repos get it too. Kubo's
        // defaults (DHT server, hundreds of connections) saturated a home connection in
        // testing: every other request on the machine timed out until the node stopped.
        for (k, v) in DESKTOP_CONFIG {
            run(&self.bin, &self.repo, &["config", "--json", k, v])?;
        }
        run(&self.bin, &self.repo, &["config", provide_strategy_key(&self.bin, &self.repo), "pinned"])?;
        // listen on TCP only (keep the random port init chose)
        let swarm: Vec<String> = serde_json::from_str(&run(&self.bin, &self.repo, &["config", "Addresses.Swarm"])?).unwrap_or_default();
        let tcp: Vec<String> = swarm.into_iter().filter(|a| a.contains("/tcp/") && !a.contains("/ws")).collect();
        if !tcp.is_empty() {
            run(&self.bin, &self.repo, &["config", "--json", "Addresses.Swarm", &serde_json::to_string(&tcp).unwrap()])?;
        }
        Ok((api, gw))
    }

    pub fn start(&mut self, wait: Duration) -> Result<Value, String> {
        if self.running() {
            return Ok(self.status_json());
        }
        if !self.bin.exists() {
            return Err(format!("ipfs binary not found at {}", self.bin.display()));
        }
        let (api, gw) = self.prepare()?;
        let log = std::fs::File::create(self.log_path()).map_err(|e| e.to_string())?;
        let mut cmd = Command::new(&self.bin);
        cmd.arg("daemon").arg("--enable-gc");
        if self.offline {
            cmd.arg("--offline");
        }
        cmd.env("IPFS_PATH", &self.repo)
            .stdin(Stdio::null())
            .stdout(log.try_clone().map_err(|e| e.to_string())?)
            .stderr(log);
        let child = spawn_on_supervisor(cmd).map_err(|e| format!("could not start ipfs daemon: {e}"))?;
        self.child = Some(child);
        self.api = format!("http://127.0.0.1:{api}");
        self.gateway = format!("http://127.0.0.1:{gw}");
        let t0 = Instant::now();
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .map_err(|e| e.to_string())?;
        loop {
            if !self.running() {
                let mut tail = String::new();
                let _ = std::fs::File::open(self.log_path()).and_then(|mut f| f.read_to_string(&mut tail));
                let tail: String = tail.lines().rev().take(5).collect::<Vec<_>>().join(" | ");
                self.last_error = format!("ipfs daemon exited during start: {tail}");
                return Err(self.last_error.clone());
            }
            if let Ok(v) = client
                .post(format!("{}/api/v0/id", self.api))
                .send()
                .and_then(|r| r.json::<Value>())
            {
                self.peer_id = v["ID"].as_str().unwrap_or("").to_string();
                self.last_error.clear();
                return Ok(self.status_json());
            }
            if t0.elapsed() > wait {
                self.stop();
                self.last_error = "ipfs daemon did not answer in time".into();
                return Err(self.last_error.clone());
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    pub fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            unsafe {
                libc::kill(c.id() as i32, libc::SIGTERM);
            }
            let t0 = Instant::now();
            while t0.elapsed() < Duration::from_secs(5) {
                if let Ok(Some(_)) = c.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    pub fn status_json(&mut self) -> Value {
        let running = self.running();
        json!({
            "ok": true,
            "running": running,
            "api": if running { self.api.clone() } else { String::new() },
            "gateway": if running { self.gateway.clone() } else { String::new() },
            "peerId": if running { self.peer_id.clone() } else { String::new() },
            "offline": self.offline,
            "binary": self.bin.display().to_string(),
            "repo": self.repo.display().to_string(),
            "error": self.last_error,
        })
    }

    /// A few numbers worth showing: connected peers, repo size, pinned roots.
    pub fn stats(&self) -> Value {
        let client = match reqwest::blocking::Client::builder().timeout(Duration::from_secs(3)).build() {
            Ok(c) => c,
            Err(_) => return json!({}),
        };
        let call = |p: &str| -> Option<Value> {
            client.post(format!("{}/api/v0/{p}", self.api)).send().ok()?.json::<Value>().ok()
        };
        let peers = call("swarm/peers").and_then(|v| v["Peers"].as_array().map(|a| a.len())).unwrap_or(0);
        let repo = call("repo/stat?size-only=true").map(|v| v["RepoSize"].clone()).unwrap_or(Value::Null);
        let pins = call("pin/ls?type=recursive")
            .and_then(|v| v["Keys"].as_object().map(|o| o.len()))
            .unwrap_or(0);
        json!({ "peers": peers, "repoSize": repo, "pins": pins })
    }
}

impl Drop for Kubo {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs a kubo binary: FV_KUBO_BIN=/path/to/ipfs cargo test --no-default-features -- --ignored
    #[test]
    #[ignore]
    fn starts_answers_and_stops() {
        let bin = PathBuf::from(std::env::var("FV_KUBO_BIN").expect("FV_KUBO_BIN"));
        let dir = std::env::temp_dir().join(format!("ipfs-module-test-{}", std::process::id()));
        let mut k = Kubo::new(bin, dir.join("repo"));
        k.offline = true;
        // Start from a thread that then exits, as an IPC worker does: the node must survive it.
        let (k2, s) = std::thread::spawn(move || {
            let s = k.start(Duration::from_secs(60)).unwrap();
            (k, s)
        })
        .join()
        .unwrap();
        let mut k = k2;
        std::thread::sleep(Duration::from_secs(2));
        assert!(k.running(), "daemon died with its spawning thread");
        let cfg: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("repo/config")).unwrap()).unwrap();
        assert_eq!(cfg["Swarm"]["Transports"]["Network"]["QUIC"], false);
        assert_eq!(cfg["Routing"]["Type"], "autoclient");
        assert!(cfg["Addresses"]["Swarm"].as_array().unwrap().iter().all(|a| a.as_str().unwrap().contains("/tcp/")));
        assert_eq!(s["running"], true);
        assert!(s["peerId"].as_str().unwrap().starts_with("12D3"));
        // a block round trip through the RPC API and the gateway
        let c = reqwest::blocking::Client::new();
        let form = reqwest::blocking::multipart::Form::new()
            .part("file", reqwest::blocking::multipart::Part::bytes(b"hello logos".to_vec()).file_name("b"));
        let put: Value = c
            .post(format!("{}/api/v0/block/put?cid-codec=raw", k.api))
            .multipart(form)
            .send()
            .unwrap()
            .json()
            .unwrap();
        let cid = put["Key"].as_str().unwrap().to_string();
        let got = c.get(format!("{}/ipfs/{cid}?format=raw", k.gateway)).send().unwrap().bytes().unwrap();
        assert_eq!(&got[..], b"hello logos");
        // a second start is a no-op, a restart reuses the repo (same peer id)
        let peer = k.peer_id.clone();
        k.start(Duration::from_secs(5)).unwrap();
        k.stop();
        assert!(!k.running());
        let s = k.start(Duration::from_secs(60)).unwrap();
        assert_eq!(s["peerId"], peer.as_str());
        k.stop();
        let _ = std::fs::remove_dir_all(dir);
    }
}
