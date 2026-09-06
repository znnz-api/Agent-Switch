use anyhow::Result;
use serde_json::{Value, json};

const MODEL_SCRIPT: &str = include_str!("../assets/renderer-inject.js");

const MODEL_HEALTH: &str = "Boolean(globalThis.__ZNNZ_MODEL_UNLOCK__?.health?.().installed && globalThis.__ZNNZ_MODEL_UNLOCK__?.health?.().modelCount > 0)";
const MODEL_DETAILS: &str = "globalThis.__ZNNZ_MODEL_UNLOCK__?.health?.() ?? { installed: false, modelCount: 0, catalogSource: 'none', failures: [{ where: 'bootstrap', message: 'model injection object is missing' }] }";

#[derive(Clone, Debug)]
pub struct ScriptBundle {
    source: String,
    health_expression: &'static str,
    health_details_expression: &'static str,
}

impl ScriptBundle {
    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn health_expression(&self) -> &'static str {
        self.health_expression
    }

    pub fn health_details_expression(&self) -> &'static str {
        self.health_details_expression
    }
}

pub fn build(
    catalog_url: &str,
    catalog: &Value,
    default_model: Option<&str>,
    model_description: &str,
) -> Result<ScriptBundle> {
    crate::catalog::validate_catalog(catalog)?;
    let config = json!({
        "catalogUrl": catalog_url,
        "catalog": catalog,
        "defaultModel": default_model.unwrap_or_default(),
        "modelDescription": model_description,
        "version": env!("CARGO_PKG_VERSION"),
    });
    let source = format!(
        "globalThis.__ZNNZ_CLIENT_CONFIG__ = {};\n{}",
        serde_json::to_string(&config)?,
        MODEL_SCRIPT
    );
    Ok(ScriptBundle {
        source,
        health_expression: MODEL_HEALTH,
        health_details_expression: MODEL_DETAILS,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn catalog() -> Value {
        json!({
            "models": [{
                "slug": "gpt-test",
                "visibility": "list",
                "supported_in_api": true
            }]
        })
    }

    #[test]
    fn build_embeds_catalog_for_csp_restricted_renderers() {
        let script = build(
            "http://127.0.0.1:1234/catalog?token=test",
            &catalog(),
            Some("gpt-test"),
            "From gateway",
        )
        .unwrap();
        assert!(script.source().contains("gpt-test"));
        assert!(script.source().contains("\"catalog\""));
        assert!(script.source().contains("catalogSource = \"inline\""));
        assert!(!script.source().contains("72216192"));
        assert_eq!(script.health_expression(), MODEL_HEALTH);
    }
    #[test]
    fn renderer_injection_never_scans_or_patches_unrelated_app_state() {
        for forbidden in [
            "Response.prototype.json",
            "MutationObserver",
            "patchReact",
            "patchGraph",
            "patchNameArray",
        ] {
            assert!(
                !MODEL_SCRIPT.contains(forbidden),
                "renderer injection must not contain broad mutation hook: {forbidden}"
            );
        }
        for required in [
            "list-models-for-host",
            "model/list",
            "patchModelListResult",
            "patchModelPickerLabels",
            "schedulePickerLabelRefresh",
            "pointerdown",
            "data-codex-intelligence-trigger",
            "aria-controls",
            "107580212",
        ] {
            assert!(
                MODEL_SCRIPT.contains(required),
                "missing targeted model hook: {required}"
            );
        }
    }
}
