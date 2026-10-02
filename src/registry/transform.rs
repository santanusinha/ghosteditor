//! Transform: mason-registry compiled JSON -> our recipe list (P2.5).
//!
//! Shared by the xtask codegen and the background refresh. Pure functions:
//! JSON in, recipes out. Unknown shapes are skipped, never a crash.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Asset, Overlay, Recipe, Source};

/// One `[[server]]` entry from registry/overlay.toml (codegen input).
#[derive(Debug, Clone, Deserialize)]
pub struct OverlayEntry {
    pub name: String,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub default_for: Vec<String>,
    /// Force a binary name (exotic mason templates).
    #[serde(default)]
    pub bin: Option<String>,
    /// Pin a package off (manual install only).
    #[serde(default)]
    pub supported: Option<bool>,
}

#[derive(Serialize)]
pub struct Snapshot {
    pub pin: String,
    pub generated: String,
    pub recipes: Vec<super::Recipe>,
}

/// Filter to LSP packages, merge the overlay, sort by name.
pub fn transform_packages(packages: &[Value], overlay: &[OverlayEntry], pin: &str) -> Snapshot {
    let mut recipes: Vec<Recipe> = packages
        .iter()
        .filter_map(|package| transform_one(package, overlay))
        .collect();
    recipes.sort_by(|a, b| a.name.cmp(&b.name));
    Snapshot {
        pin: pin.to_string(),
        generated: stamp(),
        recipes,
    }
}

fn stamp() -> String {
    // UTC seconds since epoch; a monotonic stamp is enough here.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("unix-{secs}")
}

fn transform_one(package: &Value, overlay: &[OverlayEntry]) -> Option<Recipe> {
    let categories = string_list(package.get("categories"));
    if !categories.iter().any(|c| c == "LSP") {
        return None;
    }
    let name = package.get("name")?.as_str()?.to_string();
    let homepage = package
        .get("homepage")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let languages: Vec<String> = string_list(package.get("languages"))
        .into_iter()
        .map(|l| l.to_lowercase())
        .collect();

    let source = package.get("source")?;
    let purl = source.get("id")?.as_str()?;
    let (kind, package_name, version, extra) = parse_purl(purl)?;

    let mut assets: Vec<Asset> = Vec::new();
    if kind == "github" {
        match source.get("asset") {
            Some(Value::Array(list)) => {
                for a in list {
                    if let Some(asset) = parse_asset(a) {
                        assets.push(asset);
                    }
                }
            }
            Some(a @ Value::Object(_)) => {
                if let Some(asset) = parse_asset(a) {
                    assets.push(asset);
                }
            }
            _ => {}
        }
    }

    let mut extras = extra;
    if let Some(list) = source.get("extra_packages").and_then(|v| v.as_array()) {
        for e in list {
            if let Some(s) = e.as_str() {
                extras.push(s.to_string());
            }
        }
    }

    let bin = pick_bin(package, &name);
    let supported = matches!(
        kind.as_str(),
        "npm" | "pypi" | "cargo" | "golang" | "github"
    ) && (kind != "github" || !assets.is_empty());

    let (repo, package_field) = if kind == "github" {
        (Some(package_name.clone()), package_name.clone())
    } else {
        (None, package_name.clone())
    };

    let mut recipe = Recipe {
        name: name.clone(),
        version,
        languages,
        homepage,
        supported,
        source: Source {
            kind,
            package: package_field,
            extra: extras,
            repo,
            assets,
        },
        bin,
        overlay: None,
    };

    if let Some(entry) = overlay.iter().find(|s| s.name == name) {
        if let Some(force_bin) = &entry.bin {
            recipe.bin = Some(force_bin.clone());
        }
        if let Some(supported) = entry.supported {
            recipe.supported = supported;
        }
        let language = entry
            .language
            .clone()
            .or_else(|| recipe.languages.first().cloned())
            .unwrap_or_else(|| name.clone());
        recipe.overlay = Some(Overlay {
            language,
            extensions: entry.extensions.clone(),
            args: entry.args.clone(),
            default_for: entry.default_for.clone(),
        });
    }

    Some(recipe)
}

fn parse_asset(value: &Value) -> Option<Asset> {
    let target = value.get("target")?.as_str()?.to_string();
    let file = value.get("file")?.as_str()?.to_string();
    let bin = value
        .get("bin")
        .and_then(|v| v.as_str())
        .map(clean_bin_path);
    Some(Asset { target, file, bin })
}

/// `exec:libexec/bin/x` -> `libexec/bin/x`. Plain paths stay as-is.
fn clean_bin_path(raw: &str) -> String {
    raw.strip_prefix("exec:").unwrap_or(raw).to_string()
}

/// Parse `pkg:TYPE/NAME@VERSION[?EXTRA]`. NAME may contain `/` (golang
/// modules) or a URL-encoded scope (`%40`).
fn parse_purl(purl: &str) -> Option<(String, String, String, Vec<String>)> {
    let body = purl.strip_prefix("pkg:")?;
    let (kind, rest) = body.split_once('/')?;
    let (name_part, version_and_extra) = rest.rsplit_once('@')?;
    let (version, query) = version_and_extra
        .split_once('?')
        .unwrap_or((version_and_extra, ""));
    let mut extra = Vec::new();
    for pair in query.split('&').filter(|s| !s.is_empty()) {
        if let Some((key, value)) = pair.split_once('=') {
            if key == "extra" {
                extra.push(value.to_string());
            }
        }
    }
    let name = name_part.replace("%40", "@");
    Some((kind.to_string(), name, version.to_string(), extra))
}

/// Choose one executable from the mason `bin` map and resolve its mapping.
fn pick_bin(package: &Value, name: &str) -> Option<String> {
    let map = package.get("bin")?.as_object()?;
    let mut keys: Vec<&String> = map.keys().collect();
    if keys.is_empty() {
        return None;
    }
    let score = |key: &String| -> u8 {
        if key.ends_with("-langserver") {
            0
        } else if key.ends_with("-lsp") {
            1
        } else if key == name {
            2
        } else {
            3
        }
    };
    keys.sort_by_key(|k| (score(k), k.as_str()));
    let mapping = map.get(keys[0])?.as_str()?;
    resolve_bin_mapping(mapping)
}

fn resolve_bin_mapping(mapping: &str) -> Option<String> {
    for prefix in ["npm:", "pypi:", "golang:"] {
        if let Some(rest) = mapping.strip_prefix(prefix) {
            return Some(rest.to_string());
        }
    }
    if mapping == "{{source.asset.bin}}" {
        // Resolved at install time from the per-target asset.
        return None;
    }
    if mapping.contains("{{") {
        // Exotic template: overlay or user config supplies the name.
        return None;
    }
    Some(mapping.to_string())
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_npm_purl_with_extra() {
        let (kind, name, version, extra) =
            parse_purl("pkg:pypi/python-lsp-server@1.15.0?extra=all").unwrap();
        assert_eq!(
            (kind.as_str(), name.as_str(), version.as_str()),
            ("pypi", "python-lsp-server", "1.15.0")
        );
        assert_eq!(extra, vec!["all".to_string()]);
    }

    #[test]
    fn parses_scoped_npm_purl() {
        let (_, name, version, _) =
            parse_purl("pkg:npm/%40angular/language-server@22.2.0").unwrap();
        assert_eq!(name, "@angular/language-server");
        assert_eq!(version, "22.2.0");
    }

    #[test]
    fn parses_golang_module_purl() {
        let (kind, name, version, _) =
            parse_purl("pkg:golang/golang.org/x/tools/gopls@v0.23.0").unwrap();
        assert_eq!(kind, "golang");
        assert_eq!(name, "golang.org/x/tools/gopls");
        assert_eq!(version, "v0.23.0");
    }

    #[test]
    fn bin_pick_prefers_langserver_suffix() {
        let package = json!({
            "name": "pyright",
            "bin": { "pyright": "npm:pyright", "pyright-langserver": "npm:pyright-langserver" }
        });
        assert_eq!(
            pick_bin(&package, "pyright").as_deref(),
            Some("pyright-langserver")
        );
    }

    #[test]
    fn exotic_template_yields_none() {
        assert_eq!(resolve_bin_mapping("{{source.build.bin.wrapper}}"), None);
        assert_eq!(
            resolve_bin_mapping("npm:pyright-langserver").as_deref(),
            Some("pyright-langserver")
        );
    }

    #[test]
    fn transform_filters_non_lsp_and_merges_overlay() {
        let packages = vec![
            json!({
                "name": "pyright",
                "homepage": "https://github.com/RobertCraigie/pyright",
                "languages": ["Python"],
                "categories": ["LSP"],
                "source": { "id": "pkg:npm/pyright@1.1.414" },
                "bin": { "pyright": "npm:pyright", "pyright-langserver": "npm:pyright-langserver" }
            }),
            json!({
                "name": "ruff",
                "homepage": "https://example.com",
                "languages": ["Python"],
                "categories": ["Linter", "Formatter"],
                "source": { "id": "pkg:pypi/ruff@0.1.0" }
            }),
        ];
        let overlay = vec![OverlayEntry {
            name: "pyright".into(),
            language: Some("python".into()),
            extensions: vec![".py".into()],
            args: vec!["--stdio".into()],
            default_for: vec![".py".into()],
            bin: None,
            supported: None,
        }];
        let snapshot = transform_packages(&packages, &overlay, "test-pin");
        assert_eq!(snapshot.recipes.len(), 1);
        let recipe = &snapshot.recipes[0];
        assert_eq!(recipe.name, "pyright");
        assert_eq!(recipe.source.kind, "npm");
        assert!(recipe.supported);
        let overlay = recipe.overlay.as_ref().unwrap();
        assert_eq!(overlay.language, "python");
        assert_eq!(overlay.args, vec!["--stdio".to_string()]);
    }
}
