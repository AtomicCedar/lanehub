//! Live catalog refresh: pull once at startup from each lane's `/models`
//! metadata endpoint and override the built-in entries with upstream values.
//!
//! Design (learned from jiesou/dsh-opencode-zen-free-provider): fetch exactly
//! once, never poll; on any failure keep the built-in table untouched. Upstream
//! metadata drifts constantly (see 2026-11-09 audit where 12 of 17 entries
//! differed), so a best-effort refresh beats hand-maintained numbers.

use crate::catalog::{Lane, ModelEntry};

#[derive(serde::Deserialize)]
struct OcLimit {
    #[serde(default)]
    context: Option<u64>,
    #[serde(default)]
    output: Option<u64>,
}

#[derive(serde::Deserialize)]
struct OcModel {
    id: String,
    #[serde(default)]
    limit: Option<OcLimit>,
    #[serde(default)]
    modalities: Option<OcModalities>,
}

#[derive(serde::Deserialize)]
struct OcModalities {
    #[serde(default)]
    input: Vec<String>,
}

#[derive(serde::Deserialize)]
struct KiloModel {
    id: String,
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default)]
    top_provider: Option<KiloTopProvider>,
    #[serde(default)]
    architecture: Option<KiloArchitecture>,
}

#[derive(serde::Deserialize)]
struct KiloTopProvider {
    #[serde(default, rename = "max_completion_tokens")]
    max_completion_tokens: Option<u64>,
}

#[derive(serde::Deserialize)]
struct KiloArchitecture {
    #[serde(default)]
    input_modalities: Vec<String>,
}

fn vision_from_inputs(inputs: &[String]) -> bool {
    inputs.iter().any(|i| i.starts_with("image"))
}

/// Override built-in entries with upstream `/models` metadata.
/// Returns `(entries, refreshed_lanes)`; never removes built-in models —
/// upstream only adds/updates, the built-in table remains the base set.
pub fn refresh_catalog(entries: &mut [ModelEntry]) -> Vec<&'static str> {
    let mut refreshed = Vec::new();
    if refresh_opencode(entries).is_ok() {
        refreshed.push("opencode");
    }
    if refresh_kilo(entries).is_ok() {
        refreshed.push("kilo");
    }
    refreshed
}

fn refresh_opencode(entries: &mut [ModelEntry]) -> Result<(), Box<dyn std::error::Error>> {
    let body: serde_json::Value = fetch_json(
        "https://opencode.ai/inference/openai/v1/models",
        &[("Authorization", "Bearer public")],
        &[("User-Agent", crate::fingerprint::OPENCODE_CLIENT_UA)],
    )?;
    let models: Vec<OcModel> = serde_json::from_value(body["data"].clone())?;
    let by_id: std::collections::HashMap<String, &OcModel> =
        models.iter().map(|m| (m.id.clone(), m)).collect();
    for e in entries.iter_mut().filter(|e| e.lane == Lane::OpenCodeZen) {
        if let Some(m) = by_id.get(&e.id) {
            apply_oc(m, e);
        }
    }
    Ok(())
}

fn refresh_kilo(entries: &mut [ModelEntry]) -> Result<(), Box<dyn std::error::Error>> {
    let body: serde_json::Value = fetch_json(
        "https://api.kilo.ai/api/gateway/v1/models",
        &[("Authorization", "Bearer anonymous")],
        &[("User-Agent", crate::fingerprint::OPENCODE_CLIENT_UA)],
    )?;
    let models: Vec<KiloModel> = serde_json::from_value(body["data"].clone())?;
    let by_id: std::collections::HashMap<String, &KiloModel> =
        models.iter().map(|m| (m.id.clone(), m)).collect();
    for e in entries.iter_mut().filter(|e| e.lane == Lane::KiloFree) {
        if let Some(m) = by_id.get(&e.id) {
            apply_kilo(m, e);
        }
    }
    Ok(())
}

fn apply_oc(m: &OcModel, e: &mut ModelEntry) {
    if let Some(l) = &m.limit {
        if let Some(c) = l.context {
            e.context_window = c;
        }
        if let Some(o) = l.output {
            e.max_output = o;
        }
    }
    if let Some(mods) = &m.modalities {
        e.vision = vision_from_inputs(&mods.input);
    }
}

fn apply_kilo(m: &KiloModel, e: &mut ModelEntry) {
    if let Some(c) = m.context_length {
        e.context_window = c;
    }
    if let Some(tp) = &m.top_provider {
        if let Some(o) = tp.max_completion_tokens {
            e.max_output = o;
        }
    }
    if let Some(arch) = &m.architecture {
        e.vision = vision_from_inputs(&arch.input_modalities);
    }
}

fn fetch_json(
    url: &str,
    auth: &[(&str, &str)],
    extra: &[(&str, &str)],
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let mut req = client.get(url);
    for (k, v) in auth {
        req = req.header(*k, *v);
    }
    for (k, v) in extra {
        req = req.header(*k, *v);
    }
    Ok(req.send()?.error_for_status()?.json()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vision_detection_accepts_mime_subtypes() {
        // OpenCode lists inputs as `image/jpeg`, `image/png` — not bare `image`.
        assert!(vision_from_inputs(&["text".into(), "image/png".into()]));
        assert!(!vision_from_inputs(&["text".into()]));
    }

    #[test]
    fn refresh_keeps_builtin_entries_on_missing_upstream() {
        let mut entries = builtin_test_entries();
        // Only Kilo exists upstream; missing OpenCode ids must survive.
        let _ = refresh_catalog(&mut entries);
        assert!(
            entries.iter().any(|e| e.lane == Lane::OpenCodeZen),
            "builtin opencode entries must never be dropped"
        );
        assert!(entries.iter().all(|e| e.context_window > 0));
    }

    fn builtin_test_entries() -> Vec<ModelEntry> {
        crate::catalog::builtin_catalog()
    }
}
