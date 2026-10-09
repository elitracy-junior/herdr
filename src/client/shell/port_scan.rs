//! Which local ports each process is listening on, refreshed off the UI thread.
//!
//! Enumerating listening sockets costs upwards of 150ms, which is far too much
//! to spend on the client's loop: the sidebar would stall every time it ran.
//! A worker thread owns that cost and publishes a map the loop reads under a
//! lock it holds for as long as a clone takes.
//!
//! The answer is allowed to lag. Ports change when a server starts or stops,
//! which is rare next to how often the sidebar draws, and a port shown a few
//! seconds late is worth more than a sidebar that hitches.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Listening ports by process id.
pub(super) type PortMap = HashMap<u32, Vec<u16>>;

#[derive(Clone)]
pub(super) struct ListeningPorts {
    ports: Arc<Mutex<PortMap>>,
    wanted: Arc<AtomicBool>,
}

/// How often the worker looks. Slow on purpose; see the module comment.
const SCAN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

impl ListeningPorts {
    /// Start the worker. It idles until something asks for ports, so a session
    /// that never shows the Projects tree never pays for a scan.
    pub(super) fn spawn() -> Self {
        let this = Self {
            ports: Arc::new(Mutex::new(PortMap::new())),
            wanted: Arc::new(AtomicBool::new(false)),
        };
        let worker = this.clone();
        std::thread::Builder::new()
            .name("herdr-port-scan".into())
            .spawn(move || loop {
                // `wanted` is cleared after each scan, so a tree that stops
                // being drawn stops the scanning within one interval.
                if worker.wanted.swap(false, Ordering::Relaxed) {
                    let scanned = scan();
                    if let Ok(mut ports) = worker.ports.lock() {
                        *ports = scanned;
                    }
                }
                std::thread::sleep(SCAN_INTERVAL);
            })
            .ok();
        this
    }

    /// Ask for a scan on the next tick.
    pub(super) fn request(&self) {
        self.wanted.store(true, Ordering::Relaxed);
    }

    /// Ports held by any of these processes, lowest first and deduplicated.
    pub(super) fn for_pids(&self, pids: &[u32]) -> Vec<u16> {
        let Ok(ports) = self.ports.lock() else {
            return Vec::new();
        };
        let mut found = pids
            .iter()
            .filter_map(|pid| ports.get(pid))
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        found.sort_unstable();
        found.dedup();
        found
    }
}

/// `-b -w` keeps lsof off blocking kernel calls, which is most of its cost, and
/// `-F` asks for field output rather than a table, which has no column widths to
/// misparse. Fields arrive as a stream: `p<pid>` begins a process, `n<name>`
/// gives each of its sockets.
fn scan() -> PortMap {
    let Ok(output) = std::process::Command::new("lsof")
        .args(["-nPbw", "-FpPn", "-iTCP", "-sTCP:LISTEN"])
        .output()
    else {
        return PortMap::new();
    };
    let mut map = PortMap::new();
    let mut pid = None;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((tag, value)) = line.split_at_checked(1) else {
            continue;
        };
        match tag {
            "p" => pid = value.parse::<u32>().ok(),
            "n" => {
                let Some(pid) = pid else { continue };
                if let Some(port) = listening_port(value) {
                    let entry: &mut Vec<u16> = map.entry(pid).or_default();
                    if !entry.contains(&port) {
                        entry.push(port);
                    }
                }
            }
            _ => {}
        }
    }
    map
}

/// The port from an lsof socket name: `*:3500`, `127.0.0.1:8787`, `[::1]:3000`.
fn listening_port(name: &str) -> Option<u16> {
    name.rsplit_once(':')
        .map(|(_, port)| port)
        .unwrap_or(name)
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_port_is_read_from_every_address_shape() {
        assert_eq!(listening_port("*:3500"), Some(3500));
        assert_eq!(listening_port("127.0.0.1:8787"), Some(8787));
        assert_eq!(listening_port("[::1]:3000"), Some(3000));
        // A socket with no port is not a listening address worth showing.
        assert_eq!(listening_port("*"), None);
        assert_eq!(listening_port(""), None);
    }
}
