//! Multi-Homed IPv6/IPv4 Dual-Stack Transport & Endpoint Health Pool.
//!
//! Provides high-availability egress routing and connection health management:
//! 1. Rate-limit and concurrency distribution: balances traffic across multiple dual-stack
//!    or proxy egress interfaces, isolating rate limit windows per `(egress_node, model)`.
//! 2. Isolated endpoint cooldown: when an egress path experiences rate limits or handshake
//!    errors, `EgressPool` temporarily marks only that specific `(exit, model)` pair for
//!    cooldown and immediately dispatches requests to alternative healthy routes.

use crate::fingerprint::SessionLedger;
use reqwest::Client;
use serde::Serialize;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    Quota429,
    FreeTier403,
    RegionBlock,
    Upstream5xx,
    Transport,
}

impl std::fmt::Display for FailureClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FailureClass::Quota429 => write!(f, "quota429"),
            FailureClass::FreeTier403 => write!(f, "free_tier403"),
            FailureClass::RegionBlock => write!(f, "region_block"),
            FailureClass::Upstream5xx => write!(f, "upstream5xx"),
            FailureClass::Transport => write!(f, "transport"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct EgressNode {
    pub id: String,
    pub spec: String,
    pub client: Client,
    /// Set for `relay://` exits: instead of a local proxy socket, upstream requests
    /// are forwarded as JSON envelopes through an HTTP relay endpoint (e.g. a Modal
    /// serverless container that boots on demand and yields a fresh egress IP on
    /// every cold start — see `modal/relay.py`).
    pub relay: Option<RelayConfig>,
}

/// Connection parameters of a `relay://<https-url>[#key=<secret>]` egress exit.
#[derive(Debug, Clone)]
pub struct RelayConfig {
    /// HTTPS endpoint of the relay (the `relay://` scheme prefix is stripped).
    pub url: String,
    /// Optional shared secret, sent to the relay as `x-lanehub-relay-key`.
    pub key: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CooldownEntry {
    pub exit_id: String,
    pub model: String,
    pub class: FailureClass,
    pub remaining_secs: u64,
    pub detail: String,
}

struct BanRecord {
    until: Instant,
    class: FailureClass,
    detail: String,
    /// Consecutive failures while already on cooldown; drives exponential backoff.
    attempts: u32,
}

pub struct EgressPool {
    nodes: Vec<EgressNode>,
    bans: Mutex<HashMap<(String, String), BanRecord>>,
    pub sessions: SessionLedger,
}

/// xorshift64 state for randomized candidate ordering (per-process, lock-free).
static ROTATE_STATE: AtomicU64 = AtomicU64::new(0x9e37_79b9_7f4a_7c15);

fn next_random() -> u64 {
    let mut x = ROTATE_STATE.load(Ordering::Relaxed);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    ROTATE_STATE.store(x, Ordering::Relaxed);
    x
}

impl Default for EgressPool {
    fn default() -> Self {
        Self::new(&["direct".to_string()])
    }
}

impl EgressPool {
    /// Build an egress pool from transport route specifications:
    /// - `"auto"` (expands to `["ipv6", "ipv4"]` dual-stack sockets — compatible with Cloudflare WARP and multi-stack networks)
    /// - `"ipv6"` (explicit IPv6 `[::]` socket binding)
    /// - `"ipv4"` (explicit IPv4 `0.0.0.0` socket binding)
    /// - `"direct"` (default OS routing table)
    /// - `"http://127.0.0.1:7890"` / `"socks5://127.0.0.1:1080"` (proxy transport endpoints)
    /// - `"bind://192.168.31.135"` (bind socket to a specific local interface IP)
    /// - `"relay://https://relay-host[#key=<secret>]"` (serverless HTTP relay with a fresh
    ///   egress IP per cold boot, e.g. Modal — see `modal/relay.py`)
    pub fn new(specs: &[String]) -> Self {
        let mut expanded = Vec::new();
        for raw in specs {
            let s = raw.trim();
            if s.eq_ignore_ascii_case("auto") {
                expanded.push("ipv6".to_string());
                expanded.push("ipv4".to_string());
            } else if !s.is_empty() {
                expanded.push(s.to_string());
            }
        }
        let mut nodes = Vec::new();
        for (idx, spec) in expanded.iter().enumerate() {
            let relay = parse_relay_spec(spec);
            // Relay specs never leak their `#key=` fragment into diagnostics.
            let stored_spec = relay
                .as_ref()
                .map(|r| format!("relay://{}", r.url))
                .unwrap_or_else(|| spec.clone());
            if let Ok(client) = build_client_for_spec(spec) {
                let id = match stored_spec.as_str() {
                    "direct" | "ipv6" | "ipv4" => stored_spec.clone(),
                    _ => format!("exit-{idx}:{stored_spec}"),
                };
                nodes.push(EgressNode {
                    id,
                    spec: stored_spec,
                    client,
                    relay,
                });
            }
        }
        if nodes.is_empty() {
            nodes.push(EgressNode {
                id: "direct".to_string(),
                spec: "direct".to_string(),
                client: build_client_for_spec("direct").expect("default reqwest client"),
                relay: None,
            });
        }
        Self {
            nodes,
            bans: Mutex::new(HashMap::new()),
            sessions: SessionLedger::default(),
        }
    }

    pub fn nodes(&self) -> &[EgressNode] {
        &self.nodes
    }

    /// Pick up to `max_candidates` healthy exits for `model`, preferring a stable exit
    /// for `conv_seed` first (to maximize prompt cache hits) and rotating through the rest.
    pub fn candidates_for(&self, conv_seed: &str, model: &str, max_candidates: usize) -> Vec<EgressNode> {
        let n = self.nodes.len();
        if n == 0 {
            return vec![];
        }
        let start = if !conv_seed.is_empty() {
            // Stable exit per conversation for prompt-cache affinity...
            let hash = conv_seed
                .bytes()
                .fold(0usize, |acc, b| acc.wrapping_mul(31).wrapping_add(b as usize));
            hash % n
        } else {
            // ...but a randomized start per stateless request, so after a quota
            // cooldown different requests land on different exits instead of
            // always hammering the same one first.
            (next_random() as usize) % n
        };

        let now = Instant::now();
        let bans = self.bans.lock().unwrap();
        let mut healthy = Vec::with_capacity(n);
        let mut fallback = Vec::new();

        // Relay exits are a paid (Modal) safety net: only burn relay credits when
        // no direct exit can serve the request, so they sort after all direct ones.
        let mut direct = Vec::with_capacity(n);
        let mut relay = Vec::new();
        for offset in 0..n {
            let node = &self.nodes[(start + offset) % n];
            let key = (node.id.clone(), model.to_string());
            if let Some(ban) = bans.get(&key) {
                if ban.until > now {
                    fallback.push(node.clone());
                    continue;
                }
            }
            if node.relay.is_some() {
                relay.push(node.clone());
            } else {
                direct.push(node.clone());
            }
        }
        healthy.extend(direct);
        healthy.extend(relay);

        // If all exits are currently on cooldown, still allow one probe attempt on the
        // one whose cooldown expires soonest (seamless resume after quota reset).
        if healthy.is_empty() && !fallback.is_empty() {
            fallback.sort_by_key(|node| {
                bans
                    .get(&(node.id.clone(), model.to_string()))
                    .map(|ban| ban.until.saturating_duration_since(now))
                    .unwrap_or_default()
            });
            healthy.push(fallback.remove(0));
        }
        drop(bans);

        if healthy.len() > max_candidates {
            healthy.truncate(max_candidates);
        }
        healthy
    }

    /// Record a transport or endpoint failure on `(exit_id, model)` to initiate a cooldown window.
    ///
    /// Repeated failures on an already-cooled pair back off exponentially (up to 1h),
    /// so a persistently broken route stops churning the prober and failover loop.
    /// `Quota429` windows are server-driven and always honor `Retry-After` as-is.
    pub fn report_failure(
        &self,
        exit_id: &str,
        model: &str,
        class: FailureClass,
        retry_after_secs: Option<u64>,
        detail: impl Into<String>,
    ) {
        // Relay exits get a fresh egress IP on every cold boot, so an hours-long
        // upstream `Retry-After` would idle a route that a container recycle would
        // have healed. Cool down only long enough for the relay container to be
        // reclaimed (see SCALEDOWN_WINDOW_SECS in modal/relay.py).
        let is_relay = self
            .nodes
            .iter()
            .any(|n| n.id == exit_id && n.relay.is_some());
        let base_ttl = match class {
            FailureClass::Quota429 if is_relay => {
                Duration::from_secs(RELAY_QUOTA_COOLDOWN_SECS)
            }
            FailureClass::Quota429 => {
                Duration::from_secs(retry_after_secs.unwrap_or(1800).clamp(30, 86400))
            }
            FailureClass::FreeTier403 | FailureClass::RegionBlock => Duration::from_secs(900),
            FailureClass::Upstream5xx => Duration::from_secs(45),
            FailureClass::Transport => Duration::from_secs(30),
        };
        let mut bans = self.bans.lock().unwrap();
        let now = Instant::now();
        let key = (exit_id.to_string(), model.to_string());
        let attempts = match bans.get(&key) {
            Some(prev) if prev.until > now => prev.attempts.saturating_add(1),
            _ => 0,
        };
        let ttl = match class {
            FailureClass::Quota429 => base_ttl,
            _ => base_ttl
                .mul_f64(2f64.powi(attempts.min(8) as i32))
                .min(Duration::from_secs(3600)),
        };
        bans.insert(
            key,
            BanRecord {
                until: now + ttl,
                class,
                detail: detail.into(),
                attempts,
            },
        );
    }

    /// Clear cooldown status on `(exit_id, model)` upon receiving a successful 200 OK response.
    pub fn revive(&self, exit_id: &str, model: &str) {
        let mut bans = self.bans.lock().unwrap();
        bans.remove(&(exit_id.to_string(), model.to_string()));
    }

    /// Seconds until the earliest `Quota429` cooldown for `model` expires across all
    /// exits (`u64::MAX` when none are quota-limited; 0 when a window just cleared).
    pub fn soonest_quota_reset_secs(&self, model: &str) -> u64 {
        let now = Instant::now();
        let bans = self.bans.lock().unwrap();
        self.nodes
            .iter()
            .filter_map(|n| bans.get(&(n.id.clone(), model.to_string())))
            .filter(|ban| ban.class == FailureClass::Quota429)
            .map(|ban| ban.until.saturating_duration_since(now).as_secs())
            .min()
            .unwrap_or(u64::MAX)
    }

    /// True when at least one exit for `model` is not on cooldown.
    ///
    /// Used by the background prober to skip models whose quota is exhausted on
    /// every exit, instead of burning upstream requests to renew long bans.
    pub fn model_has_healthy_exit(&self, model: &str) -> bool {
        let now = Instant::now();
        let bans = self.bans.lock().unwrap();
        self.nodes.iter().any(|n| {
            match bans.get(&(n.id.clone(), model.to_string())) {
                Some(ban) => ban.until <= now,
                None => true,
            }
        })
    }

    /// Snapshot active cooldowns for `/v1/egress` diagnostics.
    pub fn active_cooldowns(&self) -> Vec<CooldownEntry> {
        let now = Instant::now();
        let mut bans = self.bans.lock().unwrap();
        bans.retain(|_, v| v.until > now);
        bans.iter()
            .map(|((exit_id, model), rec)| CooldownEntry {
                exit_id: exit_id.clone(),
                model: model.clone(),
                class: rec.class,
                remaining_secs: rec.until.saturating_duration_since(now).as_secs(),
                detail: rec.detail.clone(),
            })
            .collect()
    }
}

impl CooldownEntry {
    /// Stable snake_case label for logs and reports.
    pub fn class_str(&self) -> &'static str {
        match self.class {
            FailureClass::Quota429 => "quota429",
            FailureClass::FreeTier403 => "free_tier403",
            FailureClass::RegionBlock => "region_block",
            FailureClass::Upstream5xx => "upstream5xx",
            FailureClass::Transport => "transport",
        }
    }
}

/// Cooldown applied to relay exits on upstream quota `429` responses.
///
/// Deliberately longer than `SCALEDOWN_WINDOW_SECS` (60s) in `modal/relay.py`:
/// by the time the cooldown expires, the relay container has been reclaimed and
/// the retry cold-boots onto a fresh egress IP with a clean quota window.
const RELAY_QUOTA_COOLDOWN_SECS: u64 = 90;

/// Parse a `relay://<https-url>[#key=<secret>]` egress spec.
///
/// Returns `None` for any other spec shape. The `#key=` fragment (if present)
/// is stripped from the stored spec so it never leaks into `/v1/egress`
/// diagnostics or logs.
fn parse_relay_spec(spec: &str) -> Option<RelayConfig> {
    let rest = spec.strip_prefix("relay://")?;
    let (url, key) = match rest.split_once("#key=") {
        Some((u, k)) => (u.trim().to_string(), Some(k.trim().to_string())),
        None => (rest.trim().to_string(), None),
    };
    if url.is_empty() {
        return None;
    }
    Some(RelayConfig { url, key })
}

fn build_client_for_spec(spec: &str) -> Result<Client, reqwest::Error> {
    let mut builder = Client::builder()
        .connect_timeout(Duration::from_secs(20))
        // Per-read timeout: bounds stalled upstreams so a silent connection can
        // never hang a probe (or a live request) forever, while long-running SSE
        // streams that keep sending data stay alive.
        .read_timeout(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(60))
        .tcp_keepalive(Duration::from_secs(30));

    if spec.eq_ignore_ascii_case("ipv6") {
        builder = builder.local_address(IpAddr::V6(Ipv6Addr::UNSPECIFIED));
    } else if spec.eq_ignore_ascii_case("ipv4") {
        builder = builder.local_address(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    } else if let Some(ip_str) = spec
        .strip_prefix("bind://")
        .or_else(|| spec.strip_prefix("interface://"))
    {
        if let Ok(ip) = ip_str.trim().parse::<IpAddr>() {
            builder = builder.local_address(ip);
        }
    } else if spec.starts_with("relay://") {
        // Relay exits reach a stable public HTTPS endpoint; the rotating egress IP
        // lives on the relay container itself. Allow a longer connect window so a
        // cold-booted relay container (a few seconds) never trips the timeout.
        builder = builder.connect_timeout(Duration::from_secs(60));
    } else if spec != "direct" {
        let clean_proxy = spec.trim_end_matches("#pool");
        let proxy = reqwest::Proxy::all(clean_proxy)?;
        // Disable keep-alive on rotating proxy tunnels so rotating proxies don't pin a single exit
        builder = builder.proxy(proxy).pool_max_idle_per_host(0);
    }

    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_expands_to_dual_stack() {
        let pool = EgressPool::new(&["auto".to_string()]);
        let ids: Vec<&str> = pool.nodes().iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["ipv6", "ipv4"]);
    }

    #[test]
    fn empty_specs_fall_back_to_direct() {
        let pool = EgressPool::new(&[]);
        assert_eq!(pool.nodes().len(), 1);
        assert_eq!(pool.nodes()[0].id, "direct");
    }

    #[test]
    fn failure_cooldowns_are_per_exit_model_pair() {
        let pool = EgressPool::new(&["direct".to_string()]);
        pool.report_failure("direct", "beam-free", FailureClass::Upstream5xx, None, "boom");
        assert_eq!(pool.active_cooldowns().len(), 1);
        assert_eq!(pool.active_cooldowns()[0].model, "beam-free");
        // The banned pair still gets a single fallback probe attempt...
        assert_eq!(pool.candidates_for("", "beam-free", 3).len(), 1);
        // ...but an unrelated model on the same exit is unaffected.
        assert_eq!(pool.candidates_for("", "cat-jam-free", 3).len(), 1);
        pool.revive("direct", "beam-free");
        assert_eq!(pool.active_cooldowns().len(), 0);
    }

    #[test]
    fn candidates_skip_banned_exit() {
        let pool = EgressPool::new(&["ipv6".to_string(), "ipv4".to_string()]);
        pool.report_failure("ipv6", "beam-free", FailureClass::Transport, None, "down");
        let candidates = pool.candidates_for("", "beam-free", 3);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].id, "ipv4");
    }

    #[test]
    fn relay_exit_is_strict_standby() {
        // Even with a randomized start, healthy relay candidates always sort
        // after every healthy direct exit so relay credits are only spent
        // when no direct exit can serve.
        let pool = EgressPool::new(&[
            "relay://https://r.modal.run".to_string(),
            "ipv6".to_string(),
            "ipv4".to_string(),
        ]);
        for _ in 0..32 {
            let candidates = pool.candidates_for("", "beam-free", 3);
            assert_eq!(candidates.len(), 3);
            let relay_pos = candidates
                .iter()
                .position(|c| c.relay.is_some())
                .expect("relay candidate present");
            assert_eq!(relay_pos, 2, "relay must be last: {:?}", candidates);
        }
    }

    #[test]
    fn proxy_and_bind_specs_build_clients() {
        let pool = EgressPool::new(&[
            "socks5://127.0.0.1:1080".to_string(),
            "bind://192.168.1.10".to_string(),
        ]);
        assert_eq!(pool.nodes().len(), 2);
        assert!(pool.nodes()[0].id.starts_with("exit-"));
    }

    #[test]
    fn repeat_failures_back_off_exponentially() {
        let pool = EgressPool::new(&["direct".to_string()]);
        pool.report_failure("direct", "m", FailureClass::Transport, None, "x");
        let c0 = pool.active_cooldowns()[0].remaining_secs;
        assert!((25..=30).contains(&c0), "first failure: {c0}s");
        pool.report_failure("direct", "m", FailureClass::Transport, None, "x");
        let c1 = pool.active_cooldowns()[0].remaining_secs;
        assert!(c1 > c0 && c1 <= 60, "second failure should double: {c1}s");
        pool.report_failure("direct", "m", FailureClass::Transport, None, "x");
        let c2 = pool.active_cooldowns()[0].remaining_secs;
        assert!(c2 > c1 && c2 <= 120, "third failure should double again: {c2}s");
    }

    #[test]
    fn quota_cooldowns_honor_retry_after_without_backoff() {
        let pool = EgressPool::new(&["direct".to_string()]);
        pool.report_failure("direct", "q", FailureClass::Quota429, Some(100), "quota");
        pool.report_failure("direct", "q", FailureClass::Quota429, Some(100), "quota");
        let remaining = pool.active_cooldowns()[0].remaining_secs;
        assert!(
            (95..=100).contains(&remaining),
            "quota window must follow Retry-After, not double: {remaining}s"
        );
    }

    #[test]
    fn soonest_quota_reset_reports_minimum_across_exits() {
        let pool = EgressPool::new(&["direct".to_string()]);
        // No quota cooldowns at all -> u64::MAX sentinel.
        assert_eq!(pool.soonest_quota_reset_secs("m"), u64::MAX);
        // Two distinct proxy exits with different windows -> the smaller one wins.
        let pool2 = EgressPool::new(&[
            "socks5://127.0.0.1:1081".to_string(),
            "socks5://127.0.0.1:1082".to_string(),
        ]);
        pool2.report_failure(pool2.nodes()[0].id.as_str(), "m", FailureClass::Quota429, Some(600), "quota");
        pool2.report_failure(pool2.nodes()[1].id.as_str(), "m", FailureClass::Quota429, Some(30), "quota");
        let soonest = pool2.soonest_quota_reset_secs("m");
        assert!((25..=30).contains(&soonest), "min window must win: {soonest}s");
        // A non-quota cooldown must not count.
        pool2.report_failure(pool2.nodes()[0].id.as_str(), "other", FailureClass::Transport, None, "x");
        assert_eq!(pool2.soonest_quota_reset_secs("other"), u64::MAX);
    }

    #[test]
    fn candidates_randomized_start_spreads_across_exits() {
        let pool = EgressPool::new(&[
            "socks5://127.0.0.1:1081".to_string(),
            "socks5://127.0.0.1:1082".to_string(),
            "socks5://127.0.0.1:1083".to_string(),
        ]);
        assert_eq!(pool.nodes().len(), 3);
        // Without a conversation seed, the first candidate should vary over many draws.
        let mut first_ids = std::collections::HashSet::new();
        for _ in 0..64 {
            let cands = pool.candidates_for("", "m", 1);
            first_ids.insert(cands[0].id.clone());
        }
        assert!(
            first_ids.len() > 1,
            "randomized rotation must spread first choice, got {first_ids:?}"
        );
    }

    #[test]
    fn model_has_healthy_exit_reflects_cooldowns() {
        let pool = EgressPool::new(&["ipv6".to_string(), "ipv4".to_string()]);
        assert!(pool.model_has_healthy_exit("m"));
        pool.report_failure("ipv6", "m", FailureClass::Transport, None, "x");
        assert!(pool.model_has_healthy_exit("m"), "one healthy exit left");
        pool.report_failure("ipv4", "m", FailureClass::Transport, None, "x");
        assert!(!pool.model_has_healthy_exit("m"), "all exits cooled down");
    }

    #[test]
    fn relay_spec_parses_and_strips_key_from_diagnostics() {
        let pool = EgressPool::new(&["relay://https://x.modal.run#key=hunter2".to_string()]);
        assert_eq!(pool.nodes().len(), 1);
        let node = &pool.nodes()[0];
        assert_eq!(node.spec, "relay://https://x.modal.run");
        assert!(
            !node.spec.contains("hunter2"),
            "the #key fragment must never leak into the stored spec"
        );
        let relay = node.relay.as_ref().expect("relay config parsed");
        assert_eq!(relay.url, "https://x.modal.run");
        assert_eq!(relay.key.as_deref(), Some("hunter2"));
    }

    #[test]
    fn relay_quota_cooldown_is_clamped_to_boot_window() {
        let pool = EgressPool::new(&["relay://https://x.modal.run".to_string()]);
        let exit_id = pool.nodes()[0].id.clone();
        pool.report_failure(&exit_id, "m", FailureClass::Quota429, Some(40000), "quota");
        let remaining = pool.active_cooldowns()[0].remaining_secs;
        assert!(
            (RELAY_QUOTA_COOLDOWN_SECS - 10..=RELAY_QUOTA_COOLDOWN_SECS).contains(&remaining),
            "relay quota window must track the container recycle window, not Retry-After: {remaining}s"
        );
    }
}
