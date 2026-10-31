//! Liveness identity for lease holders.
//!
//! A lease record carries the identity of the process that took it (see
//! [`crate::nats_lease::LeaseRecord`]). A later worker on the same host uses
//! that identity to prove the holder died — a SIGKILLed local worker, a
//! crashed daemon — and reclaim the lease immediately instead of waiting out
//! the lease TTL.
//!
//! The safety rule is one-directional: only provable death reclaims. Records
//! without the identity (written by older binaries), holders on a different
//! host, pid checks that can't be trusted and unreadable procfs all leave the
//! TTL as the only reclaim path. A live holder is never reclaimed.
//!
//! The support contract is the process's own procfs: `/proc` is assumed to be
//! the pid-namespace mount of the comparing process, and the pid-namespace is
//! part of the host identity, so holder pids and start ticks are comparable.
//! A host procfs bind-mounted into a nested pid namespace violates that and
//! is not supported.
//!
//! The host identity is a domain-separated SHA-256 of the machine's
//! `/etc/machine-id`, the kernel `boot_id` and the pid-namespace inode, so
//! the raw machine identifier is never published into the lease bucket. Only
//! Linux can produce a strong identity; on other platforms the holder records
//! no comparable host identity and the TTL stays the only reclaim path.

use serde::{Deserialize, Serialize};
#[cfg(target_os = "linux")]
use sha2::{Digest, Sha256};
use std::sync::LazyLock;

/// What this process could prove about the recorded holder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HolderLiveness {
    /// The pid exists and is the recorded process. Only Unix can construct
    /// this today (the existence probe plus the same-process check); other
    /// platforms answer `Unknown` and never mark a holder alive or dead.
    #[cfg_attr(not(unix), allow(dead_code))]
    Alive,
    /// The pid is gone, or exists but is provably a different process
    /// (pid reuse) or a zombie. The recorded holder cannot still hold a lease.
    Dead,
    /// Anything less than proof: unknown host, missing fields, a check this
    /// platform can't run, or an error such as EPERM. Callers fall back to the
    /// lease TTL.
    Unknown,
}

/// Identity of the process that took a lease, as recorded in the lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HolderIdentity {
    /// Pid of the holder in the pid namespace it shares with whoever compares
    /// the identity. Host identity includes the pid-namespace on Linux, so a
    /// matching host means the pid is comparable.
    pub pid: u32,
    /// Hashed host identity (machine, boot, pid namespace). `None` where the
    /// platform provides none; such holders are never treated as dead.
    #[serde(default)]
    pub host: Option<String>,
    /// Linux process start time in clock ticks since boot (field 22 of
    /// `/proc/<pid>/stat`). Distinguishes a reused pid from the recorded
    /// process. `None` where unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_ticks: Option<u64>,
}

/// Host identity of this process. `strong` records whether the value is
/// trustworthy enough to authorize a pid comparison: on Linux that requires
/// a readable `boot_id` and pid-namespace, which make the value unique per
/// boot and per pid namespace. Anything less (including a bare machine-id,
/// which cloned or container images can share) stays weak, and a weak host
/// never authorizes a reclaim.
#[derive(Debug)]
struct HostIdentity {
    value: Option<String>,
    strong: bool,
}

static HOST: LazyLock<HostIdentity> = LazyLock::new(host_identity);
static CURRENT: LazyLock<HolderIdentity> = LazyLock::new(|| HolderIdentity {
    pid: std::process::id(),
    host: HOST.value.clone(),
    start_ticks: start_ticks(),
});

/// Identity of this process, recorded in every lease it takes.
pub fn current() -> HolderIdentity {
    CURRENT.clone()
}

/// Whether this process can reclaim a dead holder's lease on this host.
/// Reclaim requires a strong host identity (see `HostIdentity`); platforms
/// without one keep the lease TTL as the only reclaim path. Integration
/// tests use this to pick reclaim-tight or TTL-tolerant assertions.
pub fn holder_reclaim_supported() -> bool {
    HOST.strong
}

/// Whether the recorded holder is provably dead on this host.
pub fn holder_liveness(holder: &HolderIdentity) -> HolderLiveness {
    holder_liveness_on(holder, &HOST)
}

fn holder_liveness_on(holder: &HolderIdentity, host: &HostIdentity) -> HolderLiveness {
    if !host.strong {
        return HolderLiveness::Unknown;
    }
    match holder.host.as_deref() {
        Some(recorded) if host.value.as_deref() == Some(recorded) => {
            process_liveness(holder.pid, holder.start_ticks)
        }
        // Missing fields, or a holder on a different host (which includes a
        // different boot or pid namespace on this machine).
        _ => HolderLiveness::Unknown,
    }
}

/// Stable, domain-separated one-way identifier for the host. Hashing keeps
/// the raw machine-id out of the shared lease bucket; two hosts collide only
/// with SHA-256-truncated odds.
#[cfg(target_os = "linux")]
fn hash_host_identity(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"harnx-lease-host-v1");
    for part in parts {
        hasher.update(b"\0");
        hasher.update(part.as_bytes());
    }
    let digest = hasher.finalize();
    digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(not(unix))]
fn host_identity() -> HostIdentity {
    HostIdentity {
        value: None,
        strong: false,
    }
}

#[cfg(not(unix))]
fn start_ticks() -> Option<u64> {
    None
}

#[cfg(not(unix))]
fn process_liveness(_pid: u32, _start_ticks: Option<u64>) -> HolderLiveness {
    HolderLiveness::Unknown
}

// Unix but not Linux: no procfs start time, and no boot id or pid-namespace
// component for the host identity, so a machine-id alone must not authorize
// a pid comparison (cloned or container images can share it). Reclaim stays
// disabled and the lease TTL is the only reclaim path.
#[cfg(all(unix, not(target_os = "linux")))]
fn host_identity() -> HostIdentity {
    HostIdentity {
        value: None,
        strong: false,
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn start_ticks() -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn machine_id() -> Option<String> {
    ["/etc/machine-id", "/var/lib/dbus/machine-id"]
        .into_iter()
        .find_map(read_trimmed)
}

#[cfg(target_os = "linux")]
fn read_trimmed(path: &str) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let trimmed = content.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(target_os = "linux")]
fn host_identity() -> HostIdentity {
    let machine = machine_id();
    let boot = read_trimmed("/proc/sys/kernel/random/boot_id");
    let pid_namespace = std::fs::read_link("/proc/self/ns/pid")
        .ok()
        .map(|link| link.to_string_lossy().into_owned());
    compose_linux_host_identity(machine, boot, pid_namespace)
}

/// Pure composition so the strength rule is testable without a machine whose
/// procfs is partially unreadable.
#[cfg(target_os = "linux")]
fn compose_linux_host_identity(
    machine: Option<String>,
    boot: Option<String>,
    pid_namespace: Option<String>,
) -> HostIdentity {
    // boot_id is random per boot and the pid-namespace inode makes
    // containers on one host differ (pids are namespaced, so a pid from
    // another namespace is not comparable). Requiring both also keeps
    // pre-reboot records incomparable, which start ticks otherwise would not:
    // ticks restart at each boot. machine-id contributes uniqueness across
    // hosts but never authorizes a comparison on its own, because cloned or
    // container images can share it.
    let strong = boot.is_some() && pid_namespace.is_some();
    let parts: [Option<String>; 3] = [machine, boot, pid_namespace];
    let raw: Vec<&str> = parts.iter().filter_map(|part| part.as_deref()).collect();
    HostIdentity {
        value: (!raw.is_empty()).then(|| hash_host_identity(&raw)),
        strong,
    }
}

#[cfg(target_os = "linux")]
fn start_ticks() -> Option<u64> {
    proc_stat(std::process::id()).map(|(_, start_ticks)| start_ticks)
}

#[cfg(unix)]
fn process_liveness(pid: u32, recorded_start_ticks: Option<u64>) -> HolderLiveness {
    // A pid outside the signal-API range says nothing: kill(0, 0) would probe
    // our own process group instead.
    if pid == 0 || pid > i32::MAX as u32 {
        return HolderLiveness::Unknown;
    }
    if let Some(conclusion) = existence_probe(pid) {
        return conclusion;
    }
    // The pid exists; whether it is still the recorded process is a
    // per-platform question.
    same_process_liveness(pid, recorded_start_ticks)
}

/// Signal-0 existence probe. `Some(liveness)` answers the question outright;
/// `None` means the pid exists and a same-process check is still needed.
#[cfg(unix)]
fn existence_probe(pid: u32) -> Option<HolderLiveness> {
    // Signal 0 only probes for existence; it is never delivered.
    // SAFETY: the only state libc::kill reads is the target pid.
    if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
        return Some(match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => HolderLiveness::Dead,
            // EPERM and anything else is uncertainty, not death.
            _ => HolderLiveness::Unknown,
        });
    }
    None
}

/// The probed pid exists; decide whether it is still the recorded process.
/// /proc is the caller's own pid-namespace mount, which the host identity
/// guarantees matches the holder's, so pids and start ticks are comparable.
#[cfg(target_os = "linux")]
fn same_process_liveness(pid: u32, recorded_start_ticks: Option<u64>) -> HolderLiveness {
    match proc_stat(pid) {
        Some((state, start_ticks)) => {
            // A zombie has terminated and can no longer renew the lease;
            // kill(pid, 0) still succeeds until the parent reaps it.
            if state == b'Z' {
                return HolderLiveness::Dead;
            }
            match recorded_start_ticks {
                // The pid was reused by a new process; the recorded holder
                // is gone.
                Some(recorded) if start_ticks != recorded => HolderLiveness::Dead,
                _ => HolderLiveness::Alive,
            }
        }
        // Unreadable (vanished between the kill probe and the read, or a
        // restricted procfs): no proof either way.
        None => HolderLiveness::Unknown,
    }
}

/// Only the existence probe is available here; the parameters matter only
/// on Linux.
#[cfg(all(unix, not(target_os = "linux")))]
fn same_process_liveness(_pid: u32, _recorded_start_ticks: Option<u64>) -> HolderLiveness {
    HolderLiveness::Alive
}

/// State (field 3) and start time in ticks since boot (field 22) from
/// `/proc/<pid>/stat`. `None` when the file is missing or malformed.
#[cfg(target_os = "linux")]
fn proc_stat(pid: u32) -> Option<(u8, u64)> {
    parse_proc_stat(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// `/proc/<pid>/stat` contents: `pid (comm) state ppid …` where `comm` may
/// contain spaces and parentheses, so parsing starts after the final `)`.
#[cfg(target_os = "linux")]
fn parse_proc_stat(stat: &str) -> Option<(u8, u64)> {
    let fields = stat.rsplit_once(')')?.1.split_ascii_whitespace();
    let mut fields = fields;
    let state = fields.next()?.as_bytes().first().copied()?;
    // Fields 3.. follow the command name; starttime is field 22, the 19th
    // token from here (field 3 + 19).
    let start_ticks = fields.nth(18)?.parse().ok()?;
    Some((state, start_ticks))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn current_host() -> HostIdentity {
        host_identity()
    }

    #[test]
    fn current_process_is_alive_on_its_own_host() {
        let host = current_host();
        if !host.strong {
            // No strong identity means no liveness proof exists at all.
            return;
        }
        assert_eq!(holder_liveness_on(&current(), &host), HolderLiveness::Alive);
    }

    #[test]
    fn dead_pid_on_same_host_is_dead() {
        let host = current_host();
        if !host.strong {
            // Hostname-only identity must never authorize a reclaim.
            return;
        }
        // Spawn and reap a child; a reaped child's pid is gone from /proc.
        // Both the test process and the child share the pid namespace under
        // the test sandbox.
        let child = std::process::Command::new("sleep")
            .arg("0")
            .spawn()
            .expect("spawn sleep for liveness test");
        let pid = child.id();
        let _ = child.wait_with_output();
        let holder = HolderIdentity {
            pid,
            host: host.value.clone(),
            start_ticks: None,
        };
        assert_eq!(holder_liveness_on(&holder, &host), HolderLiveness::Dead);
    }

    fn holder_on(host: Option<String>) -> HolderIdentity {
        HolderIdentity {
            pid: std::process::id(),
            host,
            start_ticks: None,
        }
    }

    #[test]
    fn holders_without_a_matching_host_are_unknown() {
        // A different host (which includes a different boot or pid namespace
        // on this machine) and a missing one are both unprovable; neither
        // may ever authorize a reclaim.
        for holder in [holder_on(Some("another-host".to_string())), holder_on(None)] {
            assert_eq!(
                holder_liveness_on(&holder, &current_host()),
                HolderLiveness::Unknown
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reused_pid_with_different_start_ticks_is_dead() {
        let host = current_host();
        if !host.strong {
            return;
        }
        let Some((_, start_ticks)) = proc_stat(std::process::id()) else {
            return;
        };
        let holder = HolderIdentity {
            pid: std::process::id(),
            host: host.value.clone(),
            // Same pid, different process: provably not the recorded holder.
            start_ticks: Some(start_ticks + 1),
        };
        assert_eq!(holder_liveness_on(&holder, &host), HolderLiveness::Dead);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn start_ticks_is_uptime_bounded_field_22() {
        // starttime (field 22) is clock ticks since boot; it must stay well
        // under the machine's uptime in ticks. A field-position mistake reads
        // something else — vsize looked like a start time once, growing with
        // every allocation and marking live holders dead.
        let Some((_, start_ticks)) = proc_stat(std::process::id()) else {
            panic!("proc_stat failed for the current process");
        };
        let uptime = std::fs::read_to_string("/proc/uptime").expect("/proc/uptime");
        let uptime_secs: f64 = uptime
            .split_ascii_whitespace()
            .next()
            .expect("uptime seconds")
            .parse()
            .expect("numeric uptime");
        // SAFETY: sysconf with a static name and no pointer argument.
        let clock_ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
        let uptime_ticks = uptime_secs * clock_ticks;
        assert!(start_ticks > 0);
        assert!(
            (start_ticks as f64) < uptime_ticks + 1_000.0,
            "start_ticks {start_ticks} not uptime-bounded ({uptime_ticks})"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn zombie_stat_line_parses_state_and_start_ticks() {
        let stat = "372 ((zombie) comm) Z 1 372 372 0 -1 4194560 0 0 0 2 0 0 0 0 2 0 1 0 999999 33219123 0 0 0 0 0 0 0 2147483647 1 1 0 0 0 0 0 0 0 0\n";
        let (state, start_ticks) = parse_proc_stat(stat).expect("zombie stat parses");
        assert_eq!(state, b'Z');
        assert_eq!(start_ticks, 999_999);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn malformed_stat_line_is_not_proof() {
        assert!(parse_proc_stat("no closing paren").is_none());
        assert!(parse_proc_stat("372 (comm) R").is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn strong_identity_requires_boot_id_and_pid_namespace() {
        let machine = Some("machine-id".to_string());
        let boot = Some("boot-id".to_string());
        let pid_namespace = Some("pid:[4026531836]".to_string());

        let all = compose_linux_host_identity(machine.clone(), boot.clone(), pid_namespace.clone());
        assert!(all.strong);
        assert!(all.value.is_some());

        // machine-id alone (cloned images share it) must not authorize a pid
        // comparison.
        let machine_only = compose_linux_host_identity(machine, None, None);
        assert!(!machine_only.strong);

        let without_boot =
            compose_linux_host_identity(Some("machine-id".into()), None, pid_namespace.clone());
        assert!(!without_boot.strong);

        let without_pid_namespace =
            compose_linux_host_identity(Some("machine-id".into()), boot.clone(), None);
        assert!(!without_pid_namespace.strong);

        // No machine-id at all still authorizes: boot id and pid namespace
        // alone identify the host for this boot.
        let without_machine =
            compose_linux_host_identity(None, boot.clone(), pid_namespace.clone());
        assert!(without_machine.strong);

        // Nothing readable: weak, and nothing to serialize.
        let nothing = compose_linux_host_identity(None, None, None);
        assert!(!nothing.strong);
        assert_eq!(nothing.value, None);

        // Equal inputs produce equal values, so same-host holders compare.
        assert_eq!(
            compose_linux_host_identity(
                Some("machine-id".into()),
                Some("boot-id".into()),
                pid_namespace
            )
            .value,
            all.value
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn host_identity_hash_is_stable_and_domain_separated() {
        // Pinned SHA-256 vector: h("harnx-lease-host-v1" || for each part:
        // NUL || part), truncated to 128 bits. Changing this changes lease
        // compatibility: old records compare unequal and fall back to the TTL.
        assert_eq!(
            hash_host_identity(&["machine-id", "boot-id", "pid:[4026531836]"]),
            "c9b1a9667ce1eae9742fb27d6f97012e"
        );
        assert_eq!(
            hash_host_identity(&["boot-id", "pid:[4026531836]"]),
            "dc5f45d9762a6f27847664041466c7bb"
        );
    }

    #[test]
    fn weak_host_identity_never_reclaims() {
        let weak = HostIdentity {
            value: Some("same-name-laptop".to_string()),
            strong: false,
        };
        let holder = HolderIdentity {
            pid: std::process::id(),
            host: Some("same-name-laptop".to_string()),
            start_ticks: None,
        };
        assert_eq!(holder_liveness_on(&holder, &weak), HolderLiveness::Unknown);
    }

    #[cfg(unix)]
    #[test]
    fn out_of_range_pid_is_unknown() {
        let host = HostIdentity {
            value: Some("host".to_string()),
            strong: true,
        };
        // pid 0 would probe our own process group; the max is beyond the
        // signal API's range. Neither may be treated as a liveness answer.
        for pid in [0, u32::MAX] {
            let holder = HolderIdentity {
                pid,
                host: host.value.clone(),
                start_ticks: None,
            };
            assert_eq!(holder_liveness_on(&holder, &host), HolderLiveness::Unknown);
        }
    }

    #[test]
    fn current_identity_records_pid_and_host() {
        let identity = current();
        assert_eq!(identity.pid, std::process::id());
        assert_eq!(identity.host, current_host().value);
    }

    #[test]
    fn recorded_identity_deserializes_without_optional_fields() {
        let json = format!("{{\"pid\": {}}}", std::process::id());
        let identity: HolderIdentity = serde_json::from_str(&json).expect("decode");
        assert_eq!(identity.host, None);
        assert_eq!(identity.start_ticks, None);
    }
}
