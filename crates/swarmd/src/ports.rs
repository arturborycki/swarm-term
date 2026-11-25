//! The TCP ports programs in each session listen on: a development server
//! started in a session shows up here, so the app can offer to open it (and,
//! on a remote host, forward it first). One sweep covers every session: read
//! the process table, take each session's process tree, then match the trees
//! against the system's listening sockets (`/proc` on Linux, `lsof` on macOS).
//! Programs that left the session's tree, such as daemons, aren't found.

use std::collections::{BTreeMap, HashMap, HashSet};

use swarm_proto::{ListeningPort, SessionId, SessionInfo};

/// A process: its parent and its name.
#[derive(Clone, Debug)]
pub(crate) struct Proc {
    pub ppid: u32,
    pub name: String,
}

/// A listening socket held by `pid`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Listener {
    pub pid: u32,
    pub port: u16,
    pub loopback: bool,
}

/// Listening ports for each running session in `sessions`.
pub fn session_ports(sessions: &[SessionInfo]) -> Vec<(SessionId, Vec<ListeningPort>)> {
    let roots: Vec<(SessionId, u32)> =
        sessions.iter().filter(|s| s.is_running() && s.pid > 0).map(|s| (s.id.clone(), s.pid)).collect();
    if roots.is_empty() {
        return Vec::new();
    }
    let procs = process_table();
    let trees: Vec<(SessionId, HashSet<u32>)> =
        roots.into_iter().map(|(id, pid)| (id, descendants(&procs, pid))).collect();
    let pids: HashSet<u32> = trees.iter().flat_map(|(_, t)| t.iter().copied()).collect();
    let listeners = listeners_of(&pids);
    assemble(&trees, &procs, &listeners)
}

/// `root` and every process below it.
pub(crate) fn descendants(procs: &HashMap<u32, Proc>, root: u32) -> HashSet<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (&pid, p) in procs {
        children.entry(p.ppid).or_default().push(pid);
    }
    let mut tree = HashSet::from([root]);
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        for &child in children.get(&pid).into_iter().flatten() {
            // A pid can't be its own ancestor, but guard against odd tables.
            if tree.insert(child) {
                stack.push(child);
            }
        }
    }
    tree
}

/// One entry per session and port, lowest port first; a port is loopback-only
/// when every socket on it is (a server often listens on both 127.0.0.1 and ::1).
pub(crate) fn assemble(
    trees: &[(SessionId, HashSet<u32>)],
    procs: &HashMap<u32, Proc>,
    listeners: &[Listener],
) -> Vec<(SessionId, Vec<ListeningPort>)> {
    let mut out = Vec::new();
    for (id, tree) in trees {
        let mut ports: BTreeMap<u16, ListeningPort> = BTreeMap::new();
        for l in listeners.iter().filter(|l| tree.contains(&l.pid)) {
            ports
                .entry(l.port)
                .and_modify(|p| p.loopback_only &= l.loopback)
                .or_insert_with(|| ListeningPort {
                    port: l.port,
                    pid: l.pid,
                    program: procs.get(&l.pid).map(|p| p.name.clone()).unwrap_or_default(),
                    loopback_only: l.loopback,
                });
        }
        if !ports.is_empty() {
            out.push((id.clone(), ports.into_values().collect()));
        }
    }
    out
}

#[cfg(target_os = "linux")]
fn process_table() -> HashMap<u32, Proc> {
    let mut procs = HashMap::new();
    let Ok(dir) = std::fs::read_dir("/proc") else { return procs };
    for entry in dir.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { continue };
        if let Some(p) = parse_proc_stat(&stat) {
            procs.insert(pid, p);
        }
    }
    procs
}

/// `/proc/PID/stat`: "PID (NAME) STATE PPID ...". NAME may hold spaces and
/// parentheses, so it ends at the last ')'.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_proc_stat(stat: &str) -> Option<Proc> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let name = stat.get(open + 1..close)?.to_string();
    let mut rest = stat.get(close + 1..)?.split_whitespace();
    let _state = rest.next()?;
    let ppid = rest.next()?.parse().ok()?;
    Some(Proc { ppid, name })
}

#[cfg(target_os = "linux")]
fn listeners_of(pids: &HashSet<u32>) -> Vec<Listener> {
    // Listening sockets by inode, then the inodes each process holds.
    let mut by_inode: HashMap<u64, (u16, bool)> = HashMap::new();
    for (path, v6) in [("/proc/net/tcp", false), ("/proc/net/tcp6", true)] {
        if let Ok(table) = std::fs::read_to_string(path) {
            by_inode.extend(parse_proc_net_tcp(&table, v6));
        }
    }
    let mut out = Vec::new();
    if by_inode.is_empty() {
        return out;
    }
    for &pid in pids {
        let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else { continue };
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else { continue };
            let Some(inode) = target
                .to_str()
                .and_then(|t| t.strip_prefix("socket:["))
                .and_then(|t| t.strip_suffix(']'))
                .and_then(|t| t.parse::<u64>().ok())
            else {
                continue;
            };
            if let Some(&(port, loopback)) = by_inode.get(&inode) {
                out.push(Listener { pid, port, loopback });
            }
        }
    }
    out
}

/// Listening sockets in a `/proc/net/tcp` or `tcp6` table, by inode:
/// (port, bound to loopback only).
#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_proc_net_tcp(table: &str, v6: bool) -> HashMap<u64, (u16, bool)> {
    const LISTEN: &str = "0A";
    let mut out = HashMap::new();
    for line in table.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 10 || f[3] != LISTEN {
            continue;
        }
        let Some((addr, port)) = f[1].split_once(':') else { continue };
        let (Ok(port), Ok(inode)) = (u16::from_str_radix(port, 16), f[9].parse::<u64>()) else { continue };
        if inode == 0 {
            continue;
        }
        out.insert(inode, (port, hex_addr_is_loopback(addr, v6)));
    }
    out
}

/// The kernel prints addresses as 32-bit words in host order (little-endian
/// on every platform Swarm runs on).
#[cfg(any(target_os = "linux", test))]
fn hex_addr_is_loopback(addr: &str, v6: bool) -> bool {
    let words: Option<Vec<[u8; 4]>> = (0..addr.len() / 8)
        .map(|i| addr.get(i * 8..i * 8 + 8).and_then(|w| u32::from_str_radix(w, 16).ok()).map(u32::to_le_bytes))
        .collect();
    let Some(words) = words else { return false };
    if !v6 {
        return words.first().is_some_and(|w| w[0] == 127);
    }
    let bytes: Vec<u8> = words.concat();
    let Ok(octets) = <[u8; 16]>::try_from(bytes) else { return false };
    let ip = std::net::Ipv6Addr::from(octets);
    ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
}

#[cfg(target_os = "macos")]
fn process_table() -> HashMap<u32, Proc> {
    let mut procs = HashMap::new();
    let Ok(out) = std::process::Command::new("/bin/ps").args(["-A", "-o", "pid=,ppid=,comm="]).output() else {
        return procs;
    };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut f = line.split_whitespace();
        let (Some(pid), Some(ppid)) = (f.next().and_then(|v| v.parse().ok()), f.next().and_then(|v| v.parse().ok()))
        else {
            continue;
        };
        // comm is the program's path and may hold spaces.
        let comm = f.collect::<Vec<_>>().join(" ");
        let name = comm.rsplit('/').next().unwrap_or(&comm).to_string();
        procs.insert(pid, Proc { ppid, name });
    }
    procs
}

#[cfg(target_os = "macos")]
fn listeners_of(pids: &HashSet<u32>) -> Vec<Listener> {
    if pids.is_empty() {
        return Vec::new();
    }
    let list = pids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    // lsof exits 1 when nothing matches; its output is still read.
    let Ok(out) = std::process::Command::new("/usr/sbin/lsof")
        .args(["-nP", "-a", "-iTCP", "-sTCP:LISTEN", "-p", &list, "-Fpn"])
        .output()
    else {
        return Vec::new();
    };
    parse_lsof(&String::from_utf8_lossy(&out.stdout))
}

/// `lsof -Fpn` output: a "p<pid>" line, then "n<address>:<port>" per socket
/// ("n*:3000", "n127.0.0.1:3000", "n[::1]:3000"); other fields are skipped.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn parse_lsof(output: &str) -> Vec<Listener> {
    let mut out = Vec::new();
    let mut pid = None;
    for line in output.lines() {
        if let Some(p) = line.strip_prefix('p') {
            pid = p.parse::<u32>().ok();
        } else if let (Some(name), Some(pid)) = (line.strip_prefix('n'), pid) {
            let Some((host, port)) = name.rsplit_once(':') else { continue };
            let Ok(port) = port.parse::<u16>() else { continue };
            let host = host.trim_start_matches('[').trim_end_matches(']');
            let loopback = host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
            out.push(Listener { pid, port, loopback });
        }
    }
    out
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_table() -> HashMap<u32, Proc> {
    HashMap::new()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn listeners_of(_pids: &HashSet<u32>) -> Vec<Listener> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(rows: &[(u32, u32, &str)]) -> HashMap<u32, Proc> {
        rows.iter().map(|&(pid, ppid, name)| (pid, Proc { ppid, name: name.to_string() })).collect()
    }

    #[test]
    fn descendants_follow_the_whole_tree() {
        let procs = table(&[(10, 1, "zsh"), (11, 10, "npm"), (12, 11, "node"), (20, 1, "other"), (21, 20, "node")]);
        assert_eq!(descendants(&procs, 10), HashSet::from([10, 11, 12]));
    }

    #[test]
    fn ports_go_to_the_session_whose_tree_holds_them() {
        let procs = table(&[(10, 1, "zsh"), (12, 10, "node"), (20, 1, "zsh"), (21, 20, "python3")]);
        let trees = vec![
            (SessionId::from("a"), descendants(&procs, 10)),
            (SessionId::from("b"), descendants(&procs, 20)),
            (SessionId::from("c"), descendants(&procs, 30)),
        ];
        let listeners = vec![
            Listener { pid: 12, port: 3000, loopback: true },
            Listener { pid: 12, port: 3000, loopback: false },
            Listener { pid: 12, port: 9229, loopback: true },
            Listener { pid: 21, port: 8000, loopback: true },
            Listener { pid: 99, port: 5432, loopback: false },
        ];
        let got = assemble(&trees, &procs, &listeners);
        assert_eq!(got.len(), 2, "session c has no ports and isn't listed");
        assert_eq!(got[0].0, SessionId::from("a"));
        let a: Vec<(u16, bool, &str)> = got[0].1.iter().map(|p| (p.port, p.loopback_only, p.program.as_str())).collect();
        assert_eq!(a, vec![(3000, false, "node"), (9229, true, "node")]);
        assert_eq!(got[1].1[0].port, 8000);
        assert_eq!(got[1].1[0].program, "python3");
    }

    #[test]
    fn proc_stat_names_may_hold_spaces_and_parens() {
        let p = parse_proc_stat("4242 (tmux: server (x)) S 1 4242 4242 0 -1").unwrap();
        assert_eq!(p.name, "tmux: server (x)");
        assert_eq!(p.ppid, 1);
    }

    #[test]
    fn proc_net_tcp_finds_listeners_and_loopback() {
        let v4 = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:0BB8 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 1111 1 0 100 0 0 10 0
   1: 00000000:1F40 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 2222 1 0 100 0 0 10 0
   2: 0100007F:0BB8 0100007F:C350 01 00000000:00000000 00:00000000 00000000  1000        0 3333 1 0 100 0 0 10 0";
        let got = parse_proc_net_tcp(v4, false);
        assert_eq!(got.get(&1111), Some(&(3000, true)));
        assert_eq!(got.get(&2222), Some(&(8000, false)));
        assert!(!got.contains_key(&3333), "an established connection isn't a listener");

        let v6 = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000001000000:0BB8 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 4444 1 0 100 0 0 10 0
   1: 00000000000000000000000000000000:0BB8 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 5555 1 0 100 0 0 10 0
   2: 0000000000000000FFFF00000100007F:0BB9 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 6666 1 0 100 0 0 10 0";
        let got = parse_proc_net_tcp(v6, true);
        assert_eq!(got.get(&4444), Some(&(3000, true)), "::1");
        assert_eq!(got.get(&5555), Some(&(3000, false)), "::");
        assert_eq!(got.get(&6666), Some(&(3001, true)), "::ffff:127.0.0.1");
    }

    #[test]
    fn lsof_output_gives_pid_port_and_loopback() {
        let out = "p501\nf20\nn127.0.0.1:3000\nf21\nn[::1]:3000\np502\nf7\nn*:8080\n";
        assert_eq!(
            parse_lsof(out),
            vec![
                Listener { pid: 501, port: 3000, loopback: true },
                Listener { pid: 501, port: 3000, loopback: true },
                Listener { pid: 502, port: 8080, loopback: false },
            ]
        );
    }
}
