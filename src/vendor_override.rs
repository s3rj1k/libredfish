//! Optional vendor override from a JSON file named by `LIBREDFISH_VENDOR_OVERRIDE_FILE`.
//! Inert when unset, see `tests/rune/README.md` for format and precedence.

use serde::Deserialize;

use crate::model::service_root::{RedfishVendor, ServiceRoot};
use crate::RedfishError;

/// Environment variable holding the path to the vendor override JSON file.
pub(crate) const ENV: &str = "LIBREDFISH_VENDOR_OVERRIDE_FILE";

#[derive(Debug, Deserialize)]
struct Entry {
    /// BMC remote address, matched against the endpoint host.
    addr: String,
    /// Optional manager id. When present, the entry only matches that manager.
    #[serde(default)]
    manager: Option<String>,
    vendor: RedfishVendor,
    /// Optional free form discriminator handed to the vendor implementation.
    #[serde(default)]
    variant: Option<String>,
    /// Optional path to a Rune script implementing the vendor (used by `Rune`).
    #[serde(default)]
    script: Option<String>,
    /// Optional free form JSON blob handed to the vendor implementation as is.
    #[serde(default)]
    data: Option<serde_json::Value>,
}

/// The parts of an entry a vendor implementation reads, everything but the vendor
/// itself. Carried on `RedfishStandard` into `set_vendor`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct VendorExtras {
    pub variant: Option<String>,
    pub script: Option<String>,
    pub data: Option<serde_json::Value>,
}

/// A matched vendor override, the forced vendor plus the extras that ride with it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct VendorOverride {
    pub vendor: RedfishVendor,
    pub extras: VendorExtras,
}

impl VendorOverride {
    /// Pin the forced vendor onto a freshly fetched service root, so `vendor()`
    /// reports it ahead of auto detection.
    pub(crate) fn stamp(&self, body: &mut ServiceRoot) {
        body.override_vendor = Some(self.vendor);
        if body.vendor.is_none() {
            body.vendor = Some(self.vendor.to_string());
        }
    }
}

fn parse(contents: &str) -> Result<Vec<Entry>, serde_json::Error> {
    serde_json::from_str(contents)
}

/// Pick the override for a host and manager id. An entry naming both wins over an
/// address only entry, which defaults for any manager at that address.
fn select(entries: &[Entry], host: &str, manager_id: &str) -> Option<VendorOverride> {
    entries
        .iter()
        .find(|e| e.addr == host && e.manager.as_deref() == Some(manager_id))
        .or_else(|| {
            entries
                .iter()
                .find(|e| e.addr == host && e.manager.is_none())
        })
        .map(|e| VendorOverride {
            vendor: e.vendor,
            extras: VendorExtras {
                variant: e.variant.clone(),
                script: e.script.clone(),
                data: e.data.clone(),
            },
        })
}

/// Resolve a vendor override for the given endpoint. `Ok(None)` when the variable is
/// unset or nothing matches, `Err(FileError)` when a configured file is unusable.
pub(crate) async fn resolve(
    host: &str,
    manager_id: &str,
) -> Result<Option<VendorOverride>, RedfishError> {
    let Ok(path) = std::env::var(ENV) else {
        return Ok(None);
    };
    resolve_from_file(&path, host, manager_id).await
}

/// Resolve an override for this endpoint and pin it onto a freshly fetched service
/// root. Inert when no entry matches, so callers can stamp unconditionally.
pub(crate) async fn stamp(
    body: &mut ServiceRoot,
    host: &str,
    manager_id: &str,
) -> Result<(), RedfishError> {
    if let Some(ov) = resolve(host, manager_id).await? {
        ov.stamp(body);
    }
    Ok(())
}

/// [`resolve`] for an explicit file, the env var only supplies the path.
async fn resolve_from_file(
    path: &str,
    host: &str,
    manager_id: &str,
) -> Result<Option<VendorOverride>, RedfishError> {
    let contents = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| RedfishError::FileError(format!("vendor override {path}: {e}")))?;
    let entries = parse(&contents)
        .map_err(|e| RedfishError::FileError(format!("vendor override {path}: {e}")))?;
    Ok(select(&entries, host, manager_id))
}

#[cfg(test)]
mod test {
    use super::*;

    const JSON: &str = r#"[
        { "addr": "10.42.0.5", "vendor": "Rune", "variant": "model-x" },
        { "addr": "10.42.0.6", "manager": "1", "vendor": "Dell" },
        { "addr": "10.42.0.6", "vendor": "NvidiaDpu" }
    ]"#;

    fn entries() -> Vec<Entry> {
        parse(JSON).unwrap()
    }

    #[test]
    fn addr_only_match_returns_vendor_and_variant() {
        let m = select(&entries(), "10.42.0.5", "1").unwrap();
        assert_eq!(m.vendor, RedfishVendor::Rune);
        assert_eq!(m.extras.variant.as_deref(), Some("model-x"));
    }

    #[test]
    fn addr_plus_manager_beats_addr_only() {
        // 10.42.0.6 has an entry for manager "1" (Dell) and an address only entry (NvidiaDpu).
        let with_mgr = select(&entries(), "10.42.0.6", "1").unwrap();
        assert_eq!(with_mgr.vendor, RedfishVendor::Dell);
        assert_eq!(with_mgr.extras.variant, None);

        // A different manager falls back to the address only default.
        let other_mgr = select(&entries(), "10.42.0.6", "2").unwrap();
        assert_eq!(other_mgr.vendor, RedfishVendor::NvidiaDpu);
    }

    #[test]
    fn no_match_returns_none() {
        assert!(select(&entries(), "10.0.0.99", "1").is_none());
    }

    // A manager scoped entry matches only that manager, which is why
    // `create_client_impl` looks up twice.
    #[test]
    fn manager_scoped_entry_matches_only_that_manager() {
        const ONLY_MGR: &str = r#"[{ "addr": "10.42.0.7", "manager": "BMC_1", "vendor": "Dell" }]"#;
        let entries = parse(ONLY_MGR).unwrap();
        assert_eq!(
            select(&entries, "10.42.0.7", "BMC_1").map(|m| m.vendor),
            Some(RedfishVendor::Dell)
        );
        assert!(select(&entries, "10.42.0.7", "BMC_0").is_none());
    }

    fn temp_file(name: &str, contents: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    // Everything an entry carries reaches the caller. The file is the only source of
    // variant, script and data.
    #[tokio::test]
    async fn file_entry_carries_vendor_variant_script_and_data() {
        let path = temp_file(
            "libredfish_vendor_override_full.json",
            r#"[{ "addr": "10.42.0.5", "vendor": "Rune", "variant": "model-x",
                  "script": "/etc/bmc.rn", "data": { "k": [1, 2] } }]"#,
        );
        let m = resolve_from_file(path.to_str().unwrap(), "10.42.0.5", "1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(m.vendor, RedfishVendor::Rune);
        assert_eq!(m.extras.variant.as_deref(), Some("model-x"));
        assert_eq!(m.extras.script.as_deref(), Some("/etc/bmc.rn"));
        assert_eq!(m.extras.data.unwrap(), serde_json::json!({ "k": [1, 2] }));

        // A host the file does not name is left to auto detection.
        assert!(resolve_from_file(path.to_str().unwrap(), "10.42.0.9", "1")
            .await
            .unwrap()
            .is_none());
    }

    // A broken file fails client creation instead of falling back to the vendor the
    // operator just excluded.
    #[tokio::test]
    async fn unreadable_or_malformed_file_errors() {
        let missing = std::env::temp_dir().join("libredfish_vendor_override_absent.json");
        let _ = std::fs::remove_file(&missing);
        assert!(matches!(
            resolve_from_file(missing.to_str().unwrap(), "10.42.0.5", "1").await,
            Err(RedfishError::FileError(_))
        ));

        let bad = temp_file("libredfish_vendor_override_bad.json", "{ not json");
        assert!(matches!(
            resolve_from_file(bad.to_str().unwrap(), "10.42.0.5", "1").await,
            Err(RedfishError::FileError(_))
        ));
    }

    // Without the env var the whole mechanism is inert, so an unconfigured caller
    // never pays for a file read.
    #[tokio::test]
    async fn unset_env_var_is_inert() {
        // Only ever removed here, no test in this binary sets it.
        std::env::remove_var(ENV);
        assert!(resolve("10.42.0.5", "1").await.unwrap().is_none());
    }

    #[test]
    fn malformed_or_unknown_vendor_errors() {
        assert!(parse("{ not json").is_err());
        // An unknown RedfishVendor variant name also fails to deserialize.
        assert!(parse(r#"[{"addr":"x","vendor":"Nope"}]"#).is_err());
    }
}
