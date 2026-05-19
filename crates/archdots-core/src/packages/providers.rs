use std::collections::HashMap;

use serde::Deserialize;

use crate::error::PackageError;

#[derive(Debug)]
pub(crate) struct CuratedProvider {
    pub(crate) package: String,
    /// `true` when the TOML entry declares `source = "aur"` explicitly.
    /// When `true`, `provider_of` returns `PkgSource::Aur` without consulting
    /// the live `pacman -Qm` output.
    pub(crate) explicitly_aur: bool,
}

#[derive(Deserialize)]
struct BinaryEntry {
    name: String,
    package: String,
    #[serde(default)]
    source: String,
}

#[derive(Deserialize)]
struct BinaryProviders {
    binary: Vec<BinaryEntry>,
}

const BINARY_PROVIDERS_TOML: &str = include_str!("../../data/binary_providers.toml");

fn parse_from_str(s: &str) -> Result<HashMap<String, CuratedProvider>, PackageError> {
    let table: BinaryProviders =
        toml::from_str(s).map_err(|e| PackageError::CuratedTableParseError(e.to_string()))?;

    let mut map = HashMap::new();
    for entry in table.binary {
        if entry.package.is_empty() {
            return Err(PackageError::CuratedTableParseError(format!(
                "binary '{}' has empty package field",
                entry.name
            )));
        }
        if map.contains_key(&entry.name) {
            return Err(PackageError::CuratedTableParseError(format!(
                "duplicate binary entry: '{}'",
                entry.name
            )));
        }
        map.insert(
            entry.name,
            CuratedProvider {
                package: entry.package,
                explicitly_aur: entry.source == "aur",
            },
        );
    }
    Ok(map)
}

pub(crate) fn load_curated_providers() -> Result<HashMap<String, CuratedProvider>, PackageError> {
    parse_from_str(BINARY_PROVIDERS_TOML)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_table_loads_without_error() {
        load_curated_providers().expect("embedded binary_providers.toml must parse cleanly");
    }

    #[test]
    fn embedded_table_has_at_least_30_entries() {
        let map = load_curated_providers().unwrap();
        assert!(
            map.len() >= 30,
            "expected ≥30 curated entries, got {}",
            map.len()
        );
    }

    #[test]
    fn rofi_resolves_to_repo_package() {
        let map = load_curated_providers().unwrap();
        let p = map.get("rofi").expect("rofi must be in curated table");
        assert_eq!(p.package, "rofi");
        assert!(!p.explicitly_aur);
    }

    #[test]
    fn eww_resolves_to_aur_package() {
        let map = load_curated_providers().unwrap();
        let p = map.get("eww").expect("eww must be in curated table");
        assert_eq!(p.package, "eww");
        assert!(p.explicitly_aur);
    }

    #[test]
    fn duplicate_entry_returns_curated_table_parse_error() {
        let toml = r#"
[[binary]]
name = "foo"
package = "foo-pkg"

[[binary]]
name = "foo"
package = "foo-other"
"#;
        let result = parse_from_str(toml);
        assert!(
            matches!(result, Err(PackageError::CuratedTableParseError(_))),
            "expected CuratedTableParseError for duplicate entry, got: {result:?}"
        );
    }
}
