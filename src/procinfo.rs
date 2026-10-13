//! Process lookups used to find, stop, and restart the node.
//!
//! The implementation is picked by the build target (`cargo build --target …`):
//! Linux (including Docker images for Umbrel / Start9) reads `/proc` directly, so it
//! needs no `lsof` or `ps` in the image. Other Unix targets (macOS, BSD) use `lsof`
//! and `ps`. Linux still falls back to `lsof` / `ps` if `/proc` gives no answer.

use std::path::PathBuf;
use std::process::Command;

/// PIDs listening on this TCP port.
pub fn listener_pids(port: u16) -> Vec<u32> {
    #[cfg(target_os = "linux")]
    {
        let pids = linux::listener_pids(port);
        if !pids.is_empty() {
            return pids;
        }
    }
    lsof_listener_pids(port)
}

/// Short process name.
pub fn comm(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    if let Some(c) = linux::comm(pid) {
        return Some(c);
    }
    let out = Command::new("ps").args(["-p", &pid.to_string(), "-o", "comm="]).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Full argv of a running process.
pub fn argv(pid: u32) -> Option<Vec<String>> {
    #[cfg(target_os = "linux")]
    if let Some(a) = linux::argv(pid) {
        return Some(a);
    }
    let out = Command::new("ps").args(["-p", &pid.to_string(), "-www", "-o", "args="]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    // Our start commands do not use quoted arguments.
    let argv: Vec<String> = String::from_utf8_lossy(&out.stdout).split_whitespace().map(str::to_string).collect();
    (!argv.is_empty()).then_some(argv)
}

/// Working directory of a running process.
pub fn cwd(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    if let Some(p) = linux::cwd(pid) {
        return Some(p);
    }
    let out = Command::new("lsof").args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"]).output().ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix('n'))
        .map(PathBuf::from)
        .find(|p| p.is_dir())
}

/// Parent process id.
pub fn ppid(pid: u32) -> Option<u32> {
    #[cfg(target_os = "linux")]
    if let Some(p) = linux::ppid(pid) {
        return Some(p);
    }
    let out = Command::new("ps").args(["-p", &pid.to_string(), "-o", "ppid="]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// `pid` is `root` or one of its descendants (walks parents, at most 16 levels).
pub fn is_same_or_descendant(pid: u32, root: u32) -> bool {
    let mut cur = pid;
    for _ in 0..16 {
        if cur == root {
            return true;
        }
        match ppid(cur) {
            Some(p) if p > 1 && p != cur => cur = p,
            _ => return false,
        }
    }
    false
}

/// `Some((pid, name))` for the first process listening on `port` that is not `ours` (nor a child of it).
/// `ours: None` means any listener counts. `name` is the executable's file name, or `?`.
pub fn foreign_listener(port: u16, ours: Option<u32>) -> Option<(u32, String)> {
    let pid = listener_pids(port)
        .into_iter()
        .find(|p| ours.is_none_or(|o| !is_same_or_descendant(*p, o)))?;
    let name = comm(pid)
        .and_then(|c| std::path::Path::new(c.trim()).file_name().map(|n| n.to_string_lossy().into_owned()))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "?".into());
    Some((pid, name))
}

fn lsof_listener_pids(port: u16) -> Vec<u32> {
    let Ok(out) = Command::new("lsof").args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-t"]).output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout).split_whitespace().filter_map(|w| w.parse().ok()).collect()
}

/// Socket inodes in LISTEN state on `port`, from `/proc/net/tcp` or `tcp6` text.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn listen_inodes(proc_net_tcp: &str, port: u16) -> Vec<u64> {
    const LISTEN: &str = "0A";
    proc_net_tcp
        .lines()
        .skip(1)
        .filter_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            let local_port = cols.get(1)?.rsplit_once(':')?.1;
            let p = u16::from_str_radix(local_port, 16).ok()?;
            (p == port && *cols.get(3)? == LISTEN).then(|| cols.get(9)?.parse().ok())?
        })
        .filter(|inode| *inode != 0)
        .collect()
}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::PathBuf;

    pub fn listener_pids(port: u16) -> Vec<u32> {
        let mut inodes = Vec::new();
        for f in ["/proc/net/tcp", "/proc/net/tcp6"] {
            if let Ok(text) = std::fs::read_to_string(f) {
                inodes.extend(super::listen_inodes(&text, port));
            }
        }
        if inodes.is_empty() {
            return Vec::new();
        }
        let wanted: Vec<String> = inodes.iter().map(|i| format!("socket:[{i}]")).collect();
        let Ok(procs) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        let mut pids = Vec::new();
        for entry in procs.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
                continue;
            };
            let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
                continue; // Not ours, or exited.
            };
            let owns = fds.flatten().any(|fd| {
                std::fs::read_link(fd.path())
                    .map(|t| wanted.iter().any(|w| t.as_os_str() == w.as_str()))
                    .unwrap_or(false)
            });
            if owns {
                pids.push(pid);
            }
        }
        pids
    }

    pub fn ppid(pid: u32) -> Option<u32> {
        // `pid (comm) S ppid ...`: comm may contain spaces and parentheses, so split after the last ')'.
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let rest = &stat[stat.rfind(')')? + 1..];
        rest.split_whitespace().nth(1)?.parse().ok()
    }

    pub fn comm(pid: u32) -> Option<String> {
        std::fs::read_to_string(format!("/proc/{pid}/comm")).ok().map(|s| s.trim().to_string())
    }

    pub fn argv(pid: u32) -> Option<Vec<String>> {
        let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        let argv: Vec<String> = raw
            .split(|b| *b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        (!argv.is_empty()).then_some(argv)
    }

    pub fn cwd(pid: u32) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok().filter(|p| p.is_dir())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_listening_socket_inode_for_port() {
        // 0x47D4 = 18388 listening; 0x4844 = 18500 established; 0x47D4 again but not LISTEN.
        let text = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 0100007F:47D4 00000000:0000 0A 00000000:00000000 00:00000000 00000000   501        0 123456 1 0000000000000000 100 0 0 10 0\n\
   1: 0100007F:4844 0100007F:C350 01 00000000:00000000 00:00000000 00000000   501        0 654321 1 0000000000000000 20 4 30 10 -1\n\
   2: 0100007F:47D4 0100007F:C351 01 00000000:00000000 00:00000000 00000000   501        0 777777 1 0000000000000000 20 4 30 10 -1\n";
        assert_eq!(listen_inodes(text, 18388), vec![123456]);
        assert!(listen_inodes(text, 18500).is_empty());
        assert!(listen_inodes("", 18388).is_empty());
    }

    #[test]
    fn looks_up_this_process() {
        let me = std::process::id();
        assert!(comm(me).is_some_and(|c| !c.is_empty()));
        assert!(argv(me).is_some_and(|a| !a.is_empty()));
        assert!(cwd(me).is_some());
    }

    #[test]
    fn parents_and_descendants() {
        let me = std::process::id();
        assert!(ppid(me).is_some_and(|p| p > 0));
        let mut child = Command::new("sleep").arg("5").spawn().unwrap();
        assert_eq!(ppid(child.id()), Some(me));
        assert!(is_same_or_descendant(child.id(), me));
        assert!(is_same_or_descendant(me, me));
        assert!(!is_same_or_descendant(me, child.id()));
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn foreign_listener_names_the_owner_and_skips_the_node_and_its_children() {
        // This test process listens, so it is the "foreign" owner for a node that is not it ...
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let me = std::process::id();
        let (pid, name) = foreign_listener(port, None).expect("a listener");
        assert_eq!(pid, me);
        assert!(!name.is_empty());
        assert_eq!(foreign_listener(port, Some(1)).map(|f| f.0), Some(me), "not the node's");
        // ... but it is not foreign to a node that is (or contains) the listener.
        assert_eq!(foreign_listener(port, Some(me)), None);
        drop(l);
        assert_eq!(foreign_listener(port, None), None, "nobody listens any more");
    }
}
