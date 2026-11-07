//! Model Catalog & Route Lane Definitions for OpenCode Zen (built-in native wire engine),
//! Kilo Free Pool and OpenCode Zen Endpoints.

use serde::{Deserialize, Serialize};

pub const OPENCODE_ZEN_BASE: &str = "https://opencode.ai/inference/openai/v1";
pub const OPENCODE_LEGACY_BASE: &str = "https://opencode.ai/inference/v1";
pub const KILO_GATEWAY_BASE: &str = "https://api.kilo.ai/api/gateway/v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lane {
    OpenCodeZen,
    KiloFree,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    pub id: String,
    pub lane: Lane,
    pub context_window: u64,
    pub max_output: u64,
    pub vision: bool,
    pub reasoning: bool,
}

/// Process-wide catalog: built-in entries refreshed once at startup from each
/// lane's `/models` metadata endpoint (best-effort; failures keep builtin values).
/// All callers must use this instead of `builtin_catalog` so metadata stays current.
pub fn catalog() -> &'static [ModelEntry] {
    // Prefer the refreshed table; fall back to builtin on first touch before init.
    let cell = catalog_cell();
    cell.get_or_init(builtin_catalog).as_slice()
}

/// Shared storage so `init_catalog_refresh` (possibly on another thread) can
/// hand the refreshed table to `catalog` regardless of call order.
fn catalog_cell() -> &'static std::sync::OnceLock<Vec<ModelEntry>> {
    static CELL: std::sync::OnceLock<Vec<ModelEntry>> = std::sync::OnceLock::new();
    &CELL
}

/// Blocking upstream metadata refresh. Call once from a synchronous context
/// (process start, before any async runtime touches `catalog()`); the refreshed
/// table replaces the OnceLock payload via `set`. Failures are logged and keep
/// builtin values.
pub fn init_catalog_refresh() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        // Run the blocking HTTP refresh on a plain OS thread so it never
        // interacts with (or drops inside) an async runtime.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut entries = builtin_catalog();
            let refreshed = crate::catalog_refresh::refresh_catalog(&mut entries);
            let _ = tx.send((entries, refreshed));
        });
        let Ok((mut entries, refreshed)) = rx.recv_timeout(std::time::Duration::from_secs(15)) else {
            tracing::warn!("catalog refresh timed out; keeping builtin values");
            return;
        };
        if !refreshed.is_empty() {
            tracing::info!(lanes = ?refreshed, "catalog refreshed from upstream /models");
        }
        catalog_cell().set(entries).ok();
    });
}

pub fn builtin_catalog() -> Vec<ModelEntry> {
    vec![
        // OpenCode Lane
        ModelEntry {
            id: "beam-free".into(),
            lane: Lane::OpenCodeZen,
            context_window: 1_048_576,
            max_output: 131_072,
            vision: false,
            reasoning: true,
        },
        ModelEntry {
            id: "nemotron-3.5-lightning-free".into(),
            lane: Lane::OpenCodeZen,
            context_window: 262_144,
            max_output: 262_144,
            vision: false,
            reasoning: true,
        },
        ModelEntry {
            id: "flint-40b-free".into(),
            lane: Lane::OpenCodeZen,
            context_window: 262_144,
            max_output: 65_536,
            vision: false,
            reasoning: true,
        },
        ModelEntry {
            id: "deepseek-v4.1-flash-free".into(),
            lane: Lane::OpenCodeZen,
            context_window: 200000,
            max_output: 32000,
            vision: true,
            reasoning: true,
        },
        ModelEntry {
            id: "glm-5.3-flash-free".into(),
            lane: Lane::OpenCodeZen,
            context_window: 200000,
            max_output: 32000,
            vision: true,
            reasoning: true,
        },
        ModelEntry {
            id: "mimo-v2.6-flash-free".into(),
            lane: Lane::OpenCodeZen,
            context_window: 200000,
            max_output: 32000,
            vision: true,
            reasoning: true,
        },
        ModelEntry {
            id: "nemotron-3.5-super-vl-free".into(),
            lane: Lane::OpenCodeZen,
            context_window: 1048576,
            max_output: 131072,
            vision: true,
            reasoning: true,
        },
        ModelEntry {
            id: "big-pickle".into(),
            lane: Lane::OpenCodeZen,
            context_window: 200_000,
            max_output: 32_000,
            vision: false,
            reasoning: true,
        },
        // Kilo Lane
        ModelEntry {
            id: "kilo-auto/free".into(),
            lane: Lane::KiloFree,
            context_window: 256000,
            max_output: 32000,
            vision: false,
            reasoning: true,
        },
        ModelEntry {
            id: "reflection/beam-free".into(),
            lane: Lane::KiloFree,
            context_window: 1_048_576,
            max_output: 131_072,
            vision: false,
            reasoning: true,
        },
        ModelEntry {
            id: "nvidia/nemotron-3.5-super-vl:free".into(),
            lane: Lane::KiloFree,
            context_window: 1000000,
            max_output: 131072,
            vision: true,
            reasoning: true,
        },
        ModelEntry {
            id: "nvidia/nemotron-3-ultra-550b-a55b:free".into(),
            lane: Lane::KiloFree,
            context_window: 1_000_000,
            max_output: 65_536,
            vision: false,
            reasoning: true,
        },
        ModelEntry {
            id: "nvidia/nemotron-3.5-lightning:free".into(),
            lane: Lane::KiloFree,
            context_window: 1_000_000,
            max_output: 65_536,
            vision: false,
            reasoning: true,
        },
        ModelEntry {
            id: "cohere/north-mini-code:free".into(),
            lane: Lane::KiloFree,
            context_window: 256_000,
            max_output: 65_536,
            vision: false,
            reasoning: true,
        },
        ModelEntry {
            id: "dots-studio/dots-3-note-preview:free".into(),
            lane: Lane::KiloFree,
            context_window: 512000,
            max_output: 460800,
            vision: true,
            reasoning: true,
        },
        ModelEntry {
            id: "stealth/glyph-cluster".into(),
            lane: Lane::KiloFree,
            context_window: 256000,
            max_output: 256000,
            vision: false,
            reasoning: true,
        },
        ModelEntry {
            id: "openrouter/free".into(),
            lane: Lane::KiloFree,
            context_window: 200000,
            max_output: 65536,
            vision: true,
            reasoning: true,
        },
           ]
}

/// Resolve a model string (including optional `opencode/` or `kilo/` prefix or role alias)
/// into `(Lane, canonical_model_id)`.
pub fn resolve_model(raw: &str) -> (Lane, String) {
    let trimmed = raw.trim();
    let base = if let Some(idx) = trimmed.rfind('(') {
        if trimmed.ends_with(')') {
            trimmed[..idx].trim()
        } else {
            trimmed
        }
    } else {
        trimmed
    };

    if let Some(rest) = base.strip_prefix("opencode/") {
        return (Lane::OpenCodeZen, rest.to_string());
    }
    if let Some(rest) = base.strip_prefix("kilo/") {
        return (Lane::KiloFree, rest.to_string());
    }
    if let Some(rest) = base.strip_prefix("nvidia/") {
        return (Lane::KiloFree, format!("nvidia/{rest}"));
    }
    if let Some(rest) = base.strip_prefix("openrouter/") {
        return (Lane::KiloFree, format!("openrouter/{rest}"));
    }

    match base {
        "auto" | "main" | "fast" => (Lane::OpenCodeZen, "beam-free".to_string()),
        "deep" => (Lane::OpenCodeZen, "deepseek-v4.1-flash-free".to_string()),
        "code" => (Lane::OpenCodeZen, "deepseek-v4.1-flash-free".to_string()),
        "vision" | "vl" => (Lane::KiloFree, "nvidia/nemotron-3.5-super-vl:free".to_string()),
        other if other.contains('/') || other.ends_with(":free") => {
            (Lane::KiloFree, other.to_string())
        }
        other => (Lane::OpenCodeZen, other.to_string()),
    }
}

/// Direct Kilo Gateway equivalent for an OpenCode Zen model ID, when the same model
/// (or model family) is hosted on both lanes.
pub fn kilo_equivalent(opencode_id: &str) -> Option<&'static str> {
    match opencode_id {
        "beam-free" => Some("reflection/beam-free"),
        "nemotron-3.5-lightning-free" => Some("nvidia/nemotron-3.5-lightning:free"),
        "nemotron-3.5-super-vl-free" => Some("nvidia/nemotron-3.5-super-vl:free"),
        "mimo-v2.6-flash-free" => Some("xiaomi/mimo-v2.6-flash:free"),
        "mimo-v2-pro-free" => Some("xiaomi/mimo-v2-pro:free"),
        "mimo-v2-omni-free" => Some("xiaomi/mimo-v2-omni:free"),
        "minimax-m3.1-flash-free" => Some("minimax/minimax-m3.1-flash:free"),
        "minimax-m2.5-free" => Some("minimax/minimax-m2.5:free"),
        "north-mini-code-free" => Some("cohere/north-mini-code:free"),
        "qwen3.6-plus-free" => Some("qwen/qwen3.6-plus:free"),
        _ => None,
    }
}

/// Direct OpenCode Zen equivalent for a Kilo Gateway model ID, when hosted on both keyless lanes.
pub fn opencode_equivalent(kilo_id: &str) -> Option<&'static str> {
    match kilo_id {
        "reflection/beam-free" => Some("beam-free"),
        "nvidia/nemotron-3.5-lightning:free" => Some("nemotron-3.5-lightning-free"),
        "nvidia/nemotron-3.5-super-vl:free" => Some("nemotron-3.5-super-vl-free"),
        "xiaomi/mimo-v2.6-flash:free" => Some("mimo-v2.6-flash-free"),
        _ => None,
    }
}

/// Ordered failover chain when all egress paths for `primary` are unavailable.
/// Bundles keyless OpenCode Zen + Kilo Free Pool model entries.
pub fn failover_chain(primary: &str) -> Vec<(Lane, String)> {
    let (primary_lane, primary_id) = resolve_model(primary);
    let mut chain = vec![(primary_lane, primary_id.clone())];

    if primary_lane == Lane::OpenCodeZen {
        if let Some(twin) = kilo_equivalent(&primary_id) {
            chain.push((Lane::KiloFree, twin.to_string()));
        }
    } else if primary_lane == Lane::KiloFree {
        if let Some(twin) = opencode_equivalent(&primary_id) {
            chain.push((Lane::OpenCodeZen, twin.to_string()));
        }
    }

    let defaults = [
        (Lane::KiloFree, "kilo-auto/free"),
        (Lane::KiloFree, "reflection/beam-free"),
        (Lane::KiloFree, "nvidia/nemotron-3.5-super-vl:free"),
        (Lane::OpenCodeZen, "beam-free"),
        (Lane::OpenCodeZen, "deepseek-v4.1-flash-free"),
        (Lane::OpenCodeZen, "glm-5.3-flash-free"),
        (Lane::OpenCodeZen, "nemotron-3.5-lightning-free"),
    ];

    for (lane, id) in defaults {
        if !chain.iter().any(|(l, m)| *l == lane && m == id) {
            chain.push((lane, id.to_string()));
        }
    }


    chain
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_model_prefixes_and_aliases() {
        assert_eq!(
            resolve_model("opencode/beam-free"),
            (Lane::OpenCodeZen, "beam-free".to_string())
        );
        assert_eq!(
            resolve_model("kilo/kilo-auto/free"),
            (Lane::KiloFree, "kilo-auto/free".to_string())
        );
        assert_eq!(
            resolve_model("nvidia/nemotron-3.5-lightning:free"),
            (
                Lane::KiloFree,
                "nvidia/nemotron-3.5-lightning:free".to_string()
            )
        );
        assert_eq!(
            resolve_model("auto"),
            (Lane::OpenCodeZen, "beam-free".to_string())
        );
        assert_eq!(
            resolve_model("code"),
            (Lane::OpenCodeZen, "deepseek-v4.1-flash-free".to_string())
        );
        assert_eq!(
            resolve_model("vision"),
            (
                Lane::KiloFree,
                "nvidia/nemotron-3.5-super-vl:free".to_string()
            )
        );
        assert_eq!(
            resolve_model("beam-free"),
            (Lane::OpenCodeZen, "beam-free".to_string())
        );
    }

    #[test]
    fn resolve_model_strips_role_suffix() {
        assert_eq!(
            resolve_model("beam-free(fast)"),
            (Lane::OpenCodeZen, "beam-free".to_string())
        );
    }

    #[test]
    fn failover_chain_starts_with_primary_and_no_duplicates() {
        let chain = failover_chain("deepseek-v4.1-flash-free");
        assert_eq!(
            chain[0],
            (Lane::OpenCodeZen, "deepseek-v4.1-flash-free".to_string())
        );
        let unique: std::collections::HashSet<_> = chain
            .iter()
            .map(|(l, m)| (format!("{l:?}"), m.clone()))
            .collect();
        assert_eq!(unique.len(), chain.len(), "chain entries must be unique");
        assert!(chain.len() > 5, "failover chain should offer fallbacks");
    }

    #[test]
    fn failover_chain_primary_not_repeated() {
        let chain = failover_chain("beam-free");
        let count = chain
            .iter()
            .filter(|(l, m)| *l == Lane::OpenCodeZen && m == "beam-free")
            .count();
        assert_eq!(count, 1);
    }
}
