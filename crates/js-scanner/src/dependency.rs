//! package.json dependency analysis.
//!
//! Checks if a project has specific dependencies at specific version ranges.
//! Supports npm workspaces by walking workspace package.json files.
//!
//! Also scans the lockfile (yarn.lock / package-lock.json) to find packages
//! whose transitive dependencies conflict with the condition. For example,
//! if the condition matches `@patternfly/react-core <= 5.99.99`, and the
//! lockfile shows that `@patternfly/react-topology@5.2.1` depends on
//! `@patternfly/react-core@^5.1.1`, an additional incident is emitted
//! for `react-topology` to flag the version conflict.

use crate::lockfile;
use anyhow::Result;
use frontend_core::capabilities::DependencyCondition;
use frontend_core::incident::{Incident, Location, Position};
use regex::Regex;
use std::path::{Path, PathBuf};

/// Parsed dependency from package.json.
#[derive(Debug, Clone)]
pub struct PackageDependency {
    pub name: String,
    pub version: String,
    pub dep_type: String, // "dependencies", "devDependencies", "peerDependencies"
    pub line_number: u32,
}

/// Check package.json files for dependencies matching the condition.
///
/// Walks the root package.json and any workspace package.json files.
/// Also scans the lockfile for transitive dependency conflicts: if a
/// direct dependency of the project depends on the matched package at
/// a version within the condition's bounds, an additional incident is
/// emitted for the dependent package.
pub fn check_dependencies(root: &Path, condition: &DependencyCondition) -> Result<Vec<Incident>> {
    let mut incidents = Vec::new();

    // Collect all package.json paths to check (root + workspaces)
    let pkg_paths = find_package_jsons(root);

    for pkg_path in &pkg_paths {
        let results = check_single_package_json(pkg_path, condition)?;
        incidents.extend(results);
    }

    // Lockfile scan: find direct dependencies that transitively depend on
    // the matched package at a version within the condition's bounds.
    if let Some(ref target_name) = condition.name {
        let lockfile_incidents =
            check_lockfile_dependents(root, target_name, condition, &pkg_paths);
        incidents.extend(lockfile_incidents);
    }

    Ok(incidents)
}

/// Scan the lockfile for packages that depend on `target_name` at a version
/// within the condition's bounds. For each such package that is also a direct
/// dependency of the project (in a package.json), emit an incident.
fn check_lockfile_dependents(
    root: &Path,
    target_name: &str,
    condition: &DependencyCondition,
    pkg_paths: &[PathBuf],
) -> Vec<Incident> {
    let lockfile_entries = match lockfile::parse_lockfile(root) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::debug!(
                error = %e,
                "Lockfile parsing failed, skipping dependent scan"
            );
            return Vec::new();
        }
    };

    if lockfile_entries.is_empty() {
        return Vec::new();
    }

    let dependents = lockfile::find_dependents(
        &lockfile_entries,
        target_name,
        condition.upperbound.as_deref(),
    );

    if dependents.is_empty() {
        return Vec::new();
    }

    // Collect all direct dependencies from all package.json files so we
    // only emit incidents for packages the user can actually update.
    let direct_deps = collect_direct_deps(pkg_paths);

    let mut incidents = Vec::new();
    for dep in &dependents {
        // Only flag dependents that are declared in a package.json
        if let Some((pkg_path, dep_type, line_number)) = direct_deps.get(&dep.name) {
            let file_uri = format!("file://{}", pkg_path.display());
            let content = match std::fs::read_to_string(pkg_path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let mut incident = Incident::new(
                file_uri,
                *line_number,
                Location {
                    start: Position {
                        line: line_number.saturating_sub(1),
                        character: 0,
                    },
                    end: Position {
                        line: line_number.saturating_sub(1),
                        character: 0,
                    },
                },
            );

            incident.variables.insert(
                "dependencyName".into(),
                serde_json::Value::String(dep.name.clone()),
            );
            incident.variables.insert(
                "dependencyVersion".into(),
                serde_json::Value::String(dep.version.clone()),
            );
            incident.variables.insert(
                "dependencyType".into(),
                serde_json::Value::String(dep_type.clone()),
            );
            incident.variables.insert(
                "isDependentOf".into(),
                serde_json::Value::String(target_name.to_string()),
            );

            // The constraint the dependent has on the target package
            if let Some(constraint) = dep.dependencies.get(target_name) {
                incident.variables.insert(
                    "dependentConstraint".into(),
                    serde_json::Value::String(constraint.clone()),
                );
            }

            incident.code_snip = Some(frontend_core::incident::extract_code_snip(
                &content,
                *line_number,
                3,
            ));

            tracing::info!(
                dependent = %dep.name,
                dependent_version = %dep.version,
                depends_on = %target_name,
                constraint = ?dep.dependencies.get(target_name),
                "Lockfile conflict: dependent requires old version"
            );

            incidents.push(incident);
        }
    }

    incidents
}

/// Collect all direct dependencies from all package.json files.
/// Returns a map: package_name → (pkg_path, dep_type, line_number).
fn collect_direct_deps(
    pkg_paths: &[PathBuf],
) -> std::collections::HashMap<String, (PathBuf, String, u32)> {
    let mut deps = std::collections::HashMap::new();
    let dep_sections = ["dependencies", "devDependencies", "peerDependencies"];

    for pkg_path in pkg_paths {
        let content = match std::fs::read_to_string(pkg_path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let pkg: serde_json::Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => continue,
        };

        for section in &dep_sections {
            if let Some(section_deps) = pkg.get(section).and_then(|v| v.as_object()) {
                for (name, _) in section_deps {
                    let line = find_key_line(&content, name);
                    deps.entry(name.clone())
                        .or_insert_with(|| (pkg_path.clone(), section.to_string(), line));
                }
            }
        }
    }

    deps
}

/// Directories to skip when scanning for nested workspace roots.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    "dist",
    "build",
    ".cache",
    ".next",
    "coverage",
    "__mocks__",
];

/// Find all package.json files to check: root + workspace members.
///
/// Supports two common monorepo layouts:
///
/// 1. **Root-level workspaces** — `root/package.json` has a `workspaces` field
///    (e.g. `["packages/*"]`). Workspace member package.json files are expanded
///    via glob.
///
/// 2. **Nested workspace roots** — `root/package.json` is a script-only
///    orchestrator with no `workspaces` field, but a subdirectory like
///    `frontend/package.json` declares its own workspaces. In this case we
///    scan immediate child directories of `root` for package.json files that
///    have a `workspaces` field and expand those too.
pub fn find_package_jsons(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let root_pkg = root.join("package.json");

    if !root_pkg.exists() {
        return paths;
    }

    // Try root-level workspace expansion first.
    let root_has_workspaces = expand_workspaces(&root_pkg, &mut paths);

    // If root had no workspaces, scan immediate child directories for nested
    // workspace roots (e.g. frontend/package.json with its own workspaces).
    if !root_has_workspaces {
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                let child = entry.path();
                if !child.is_dir() {
                    continue;
                }
                let dir_name = child
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                if SKIP_DIRS.contains(&dir_name.as_str()) {
                    continue;
                }

                let child_pkg = child.join("package.json");
                if child_pkg.exists() && child_pkg != root_pkg {
                    // Expand this child's workspaces (if any)
                    expand_workspaces(&child_pkg, &mut paths);

                    // Always include the child package.json itself — it may
                    // declare its own dependencies that need checking.
                    if !paths.contains(&child_pkg) {
                        paths.push(child_pkg);
                    }
                }
            }
        }
    }

    // Always include root package.json (check it last so workspace-specific
    // deps are found first)
    paths.push(root_pkg);

    paths
}

/// Read the `workspaces` field from a `package.json` and expand its globs,
/// adding each discovered workspace member `package.json` to `paths`.
///
/// Returns `true` if a `workspaces` field was present (even if it was empty).
fn expand_workspaces(pkg_path: &Path, paths: &mut Vec<PathBuf>) -> bool {
    let content = match std::fs::read_to_string(pkg_path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let pkg: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let workspaces = match pkg.get("workspaces") {
        Some(ws) => ws,
        None => return false,
    };

    // workspaces can be an array of globs or an object with "packages" array
    let workspace_globs = match workspaces {
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect::<Vec<_>>(),
        serde_json::Value::Object(obj) => obj
            .get("packages")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    };

    // Resolve globs relative to the directory containing pkg_path.
    let base_dir = pkg_path.parent().unwrap_or(Path::new("."));

    for glob_pattern in &workspace_globs {
        // Simple glob expansion: handle "client", "client/*", "packages/*"
        let pattern = base_dir.join(glob_pattern).join("package.json");
        if let Ok(entries) = glob::glob(&pattern.to_string_lossy()) {
            for entry in entries.flatten() {
                if entry.exists() && &entry != pkg_path && !paths.contains(&entry) {
                    paths.push(entry);
                }
            }
        } else {
            // Fallback: try as a direct directory
            let direct = base_dir.join(glob_pattern).join("package.json");
            if direct.exists() && &direct != pkg_path && !paths.contains(&direct) {
                paths.push(direct);
            }
        }
    }

    true
}

/// Check a single package.json for dependencies matching the condition.
fn check_single_package_json(
    pkg_path: &Path,
    condition: &DependencyCondition,
) -> Result<Vec<Incident>> {
    if !pkg_path.exists() {
        return Ok(Vec::new());
    }

    let content = std::fs::read_to_string(pkg_path)?;
    let pkg: serde_json::Value = serde_json::from_str(&content)?;
    let file_uri = format!("file://{}", pkg_path.display());

    let mut incidents = Vec::new();

    let dep_sections = ["dependencies", "devDependencies", "peerDependencies"];
    for section in &dep_sections {
        if let Some(deps) = pkg.get(section).and_then(|v| v.as_object()) {
            for (name, version_val) in deps {
                let version = version_val.as_str().unwrap_or_default();

                let name_matches = match (&condition.name, &condition.nameregex) {
                    (Some(exact), _) => name == exact,
                    (_, Some(re_str)) => {
                        let re = Regex::new(re_str)?;
                        re.is_match(name)
                    }
                    (None, None) => true,
                };

                if !name_matches {
                    continue;
                }

                // Check version bounds
                if !version_in_bounds(version, condition) {
                    continue;
                }

                // Find line number of this dependency in the JSON
                let line_number = find_key_line(&content, name);

                let mut incident = Incident::new(
                    file_uri.clone(),
                    line_number,
                    Location {
                        start: Position {
                            line: line_number.saturating_sub(1),
                            character: 0,
                        },
                        end: Position {
                            line: line_number.saturating_sub(1),
                            character: 0,
                        },
                    },
                );
                incident.variables.insert(
                    "dependencyName".into(),
                    serde_json::Value::String(name.clone()),
                );
                incident.variables.insert(
                    "dependencyVersion".into(),
                    serde_json::Value::String(version.to_string()),
                );
                incident.variables.insert(
                    "dependencyType".into(),
                    serde_json::Value::String(section.to_string()),
                );

                // Add code snippet
                incident.code_snip = Some(frontend_core::incident::extract_code_snip(
                    &content,
                    line_number,
                    3,
                ));

                incidents.push(incident);
            }
        }
    }

    Ok(incidents)
}

/// Check if a version string falls within the condition's bounds.
///
/// Strips npm version range prefixes (^, ~, >=, etc.) to extract the
/// base semver for comparison.
fn version_in_bounds(version_str: &str, condition: &DependencyCondition) -> bool {
    // If no bounds specified, all versions match
    if condition.upperbound.is_none() && condition.lowerbound.is_none() {
        return true;
    }

    let base = strip_npm_prefix(version_str);
    let parsed = match parse_loose_semver(base) {
        Some(v) => v,
        None => return true, // Can't parse -> don't filter, let it match
    };

    if let Some(ref ub) = condition.upperbound {
        if let Some(bound) = parse_loose_semver(strip_npm_prefix(ub)) {
            if parsed > bound {
                return false;
            }
        }
    }

    if let Some(ref lb) = condition.lowerbound {
        if let Some(bound) = parse_loose_semver(strip_npm_prefix(lb)) {
            if parsed < bound {
                return false;
            }
        }
    }

    true
}

/// Strip npm version range prefixes: ^, ~, >=, <=, >, <, =
pub fn strip_npm_prefix(version: &str) -> &str {
    let v = version.trim();
    if let Some(rest) = v.strip_prefix(">=") {
        rest.trim()
    } else if let Some(rest) = v.strip_prefix("<=") {
        rest.trim()
    } else if let Some(rest) = v.strip_prefix('^') {
        rest.trim()
    } else if let Some(rest) = v.strip_prefix('~') {
        rest.trim()
    } else if let Some(rest) = v.strip_prefix('>') {
        rest.trim()
    } else if let Some(rest) = v.strip_prefix('<') {
        rest.trim()
    } else if let Some(rest) = v.strip_prefix('=') {
        rest.trim()
    } else {
        v
    }
}

/// Loose semver parsing: extracts (major, minor, patch) from a version string.
/// Handles versions like "5.4.2", "6.0.0-alpha.1", "5.4.2-rc.1".
fn parse_loose_semver(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim();
    // Strip prerelease/build metadata for comparison
    let version_part = s.split('-').next().unwrap_or(s);
    let version_part = version_part.split('+').next().unwrap_or(version_part);

    let parts: Vec<&str> = version_part.split('.').collect();
    let major = parts.first()?.parse::<u64>().ok()?;
    let minor = parts
        .get(1)
        .and_then(|p| p.parse::<u64>().ok())
        .unwrap_or(0);
    let patch = parts
        .get(2)
        .and_then(|p| p.parse::<u64>().ok())
        .unwrap_or(0);

    Some((major, minor, patch))
}

/// Find the line number of a JSON key in the source text.
fn find_key_line(content: &str, key: &str) -> u32 {
    let search = format!("\"{}\"", key);
    for (i, line) in content.lines().enumerate() {
        if line.contains(&search) {
            return (i + 1) as u32;
        }
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_npm_prefix() {
        assert_eq!(strip_npm_prefix("^6.4.1"), "6.4.1");
        assert_eq!(strip_npm_prefix("~5.4.2"), "5.4.2");
        assert_eq!(strip_npm_prefix(">=1.0.0"), "1.0.0");
        assert_eq!(strip_npm_prefix("5.4.2"), "5.4.2");
    }

    #[test]
    fn test_parse_loose_semver() {
        assert_eq!(parse_loose_semver("5.4.2"), Some((5, 4, 2)));
        assert_eq!(parse_loose_semver("6.0.0-alpha.1"), Some((6, 0, 0)));
        assert_eq!(parse_loose_semver("6.4.1"), Some((6, 4, 1)));
    }

    #[test]
    fn test_version_in_bounds() {
        let cond = DependencyCondition {
            name: None,
            nameregex: None,
            upperbound: Some("5.99.99".into()),
            lowerbound: None,
        };
        // v5 should match (<=5.99.99)
        assert!(version_in_bounds("^5.4.14", &cond));
        assert!(version_in_bounds("5.4.2", &cond));
        // v6 should NOT match (>5.99.99)
        assert!(!version_in_bounds("^6.4.1", &cond));
        assert!(!version_in_bounds("6.0.0", &cond));
    }

    // ── Lockfile dependent detection integration tests ───────────────

    #[test]
    fn test_check_dependencies_finds_lockfile_dependents_npm() {
        let dir = tempfile::tempdir().unwrap();

        // package.json: react-core already at v6, but react-topology at v5
        std::fs::write(
            dir.path().join("package.json"),
            r#"{
  "name": "test-project",
  "devDependencies": {
    "@patternfly/react-core": "^6.4.1",
    "@patternfly/react-topology": "5.2.1",
    "@patternfly/react-table": "^6.4.1"
  }
}"#,
        )
        .unwrap();

        // package-lock.json: topology depends on react-core ^5.1.1
        std::fs::write(
            dir.path().join("package-lock.json"),
            r#"{
  "lockfileVersion": 3,
  "packages": {
    "": { "name": "test-project" },
    "node_modules/@patternfly/react-core": {
      "version": "6.4.1",
      "dependencies": {}
    },
    "node_modules/@patternfly/react-topology": {
      "version": "5.2.1",
      "dependencies": {
        "@patternfly/react-core": "^5.1.1",
        "@patternfly/react-icons": "^5.1.1"
      }
    },
    "node_modules/@patternfly/react-table": {
      "version": "6.4.1",
      "dependencies": {
        "@patternfly/react-core": "^6.0.0"
      }
    }
  }
}"#,
        )
        .unwrap();

        let condition = DependencyCondition {
            name: Some("@patternfly/react-core".into()),
            nameregex: None,
            upperbound: Some("5.99.99".into()),
            lowerbound: None,
        };

        let incidents = check_dependencies(dir.path(), &condition).unwrap();

        // react-core itself is at v6 — no primary match.
        // But react-topology depends on react-core ^5.1.1 — lockfile dependent match.
        // react-table depends on react-core ^6.0.0 — NOT a match (above upperbound).
        assert_eq!(
            incidents.len(),
            1,
            "Expected 1 dependent incident, got {}",
            incidents.len()
        );

        let incident = &incidents[0];
        assert_eq!(
            incident
                .variables
                .get("dependencyName")
                .and_then(|v| v.as_str()),
            Some("@patternfly/react-topology")
        );
        assert_eq!(
            incident
                .variables
                .get("dependencyVersion")
                .and_then(|v| v.as_str()),
            Some("5.2.1")
        );
        assert_eq!(
            incident
                .variables
                .get("isDependentOf")
                .and_then(|v| v.as_str()),
            Some("@patternfly/react-core")
        );
        assert_eq!(
            incident
                .variables
                .get("dependentConstraint")
                .and_then(|v| v.as_str()),
            Some("^5.1.1")
        );
    }

    #[test]
    fn test_check_dependencies_finds_lockfile_dependents_yarn() {
        let dir = tempfile::tempdir().unwrap();

        std::fs::write(
            dir.path().join("package.json"),
            r#"{
  "name": "test-project",
  "devDependencies": {
    "@patternfly/react-core": "^6.4.1",
    "@patternfly/react-log-viewer": "5.3.0"
  }
}"#,
        )
        .unwrap();

        std::fs::write(
            dir.path().join("yarn.lock"),
            r#"# yarn lockfile v1

__metadata:
  version: 8

"@patternfly/react-core@npm:^6.4.1":
  version: 6.4.1
  resolution: "@patternfly/react-core@npm:6.4.1"
  languageName: node
  linkType: hard

"@patternfly/react-log-viewer@npm:5.3.0":
  version: 5.3.0
  resolution: "@patternfly/react-log-viewer@npm:5.3.0"
  dependencies:
    "@patternfly/react-core": "npm:^5.0.0"
    "@patternfly/react-styles": "npm:^5.0.0"
  languageName: node
  linkType: hard
"#,
        )
        .unwrap();

        let condition = DependencyCondition {
            name: Some("@patternfly/react-core".into()),
            nameregex: None,
            upperbound: Some("5.99.99".into()),
            lowerbound: None,
        };

        let incidents = check_dependencies(dir.path(), &condition).unwrap();

        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0]
                .variables
                .get("dependencyName")
                .and_then(|v| v.as_str()),
            Some("@patternfly/react-log-viewer")
        );
        assert_eq!(
            incidents[0]
                .variables
                .get("isDependentOf")
                .and_then(|v| v.as_str()),
            Some("@patternfly/react-core")
        );
    }

    #[test]
    fn test_check_dependencies_no_false_positives_for_v6_dependents() {
        let dir = tempfile::tempdir().unwrap();

        // All packages at v6 — no conflicts
        std::fs::write(
            dir.path().join("package.json"),
            r#"{
  "devDependencies": {
    "@patternfly/react-core": "^6.4.1",
    "@patternfly/react-table": "^6.4.1"
  }
}"#,
        )
        .unwrap();

        std::fs::write(
            dir.path().join("package-lock.json"),
            r#"{
  "lockfileVersion": 3,
  "packages": {
    "": {},
    "node_modules/@patternfly/react-core": {
      "version": "6.4.1"
    },
    "node_modules/@patternfly/react-table": {
      "version": "6.4.1",
      "dependencies": {
        "@patternfly/react-core": "^6.0.0"
      }
    }
  }
}"#,
        )
        .unwrap();

        let condition = DependencyCondition {
            name: Some("@patternfly/react-core".into()),
            nameregex: None,
            upperbound: Some("5.99.99".into()),
            lowerbound: None,
        };

        let incidents = check_dependencies(dir.path(), &condition).unwrap();
        assert!(
            incidents.is_empty(),
            "Expected 0 incidents (all deps at v6), got {}",
            incidents.len()
        );
    }

    #[test]
    fn test_check_dependencies_skips_non_direct_deps() {
        let dir = tempfile::tempdir().unwrap();

        // package.json does NOT include react-topology as a direct dep
        std::fs::write(
            dir.path().join("package.json"),
            r#"{
  "devDependencies": {
    "@patternfly/react-core": "^6.4.1"
  }
}"#,
        )
        .unwrap();

        // But lockfile shows react-topology (transitive only, not in package.json)
        std::fs::write(
            dir.path().join("package-lock.json"),
            r#"{
  "lockfileVersion": 3,
  "packages": {
    "": {},
    "node_modules/@patternfly/react-core": {
      "version": "6.4.1"
    },
    "node_modules/@patternfly/react-topology": {
      "version": "5.2.1",
      "dependencies": {
        "@patternfly/react-core": "^5.1.1"
      }
    }
  }
}"#,
        )
        .unwrap();

        let condition = DependencyCondition {
            name: Some("@patternfly/react-core".into()),
            nameregex: None,
            upperbound: Some("5.99.99".into()),
            lowerbound: None,
        };

        let incidents = check_dependencies(dir.path(), &condition).unwrap();
        assert!(
            incidents.is_empty(),
            "Should not emit incidents for non-direct deps, got {}",
            incidents.len()
        );
    }

    // ── Nested workspace discovery tests ─────────────────────────────

    #[test]
    fn test_find_package_jsons_nested_workspace_root() {
        // Simulates the ACM console layout:
        //   root/package.json          (no workspaces, script-only)
        //   root/frontend/package.json (workspaces: ["packages/*"])
        //   root/frontend/packages/sdk/package.json
        //   root/backend/package.json  (no workspaces)
        let dir = tempfile::tempdir().unwrap();

        // Root: script-only orchestrator, no workspaces
        std::fs::write(
            dir.path().join("package.json"),
            r#"{ "name": "root", "scripts": { "start": "echo hi" } }"#,
        )
        .unwrap();

        // frontend/package.json with workspaces
        let frontend = dir.path().join("frontend");
        std::fs::create_dir_all(&frontend).unwrap();
        std::fs::write(
            frontend.join("package.json"),
            r#"{ "name": "frontend", "workspaces": ["packages/*"] }"#,
        )
        .unwrap();

        // frontend/packages/sdk/package.json — workspace member
        let sdk = frontend.join("packages").join("sdk");
        std::fs::create_dir_all(&sdk).unwrap();
        std::fs::write(
            sdk.join("package.json"),
            r#"{ "name": "@org/sdk" }"#,
        )
        .unwrap();

        // backend/package.json — no workspaces
        let backend = dir.path().join("backend");
        std::fs::create_dir_all(&backend).unwrap();
        std::fs::write(
            backend.join("package.json"),
            r#"{ "name": "backend" }"#,
        )
        .unwrap();

        let found = find_package_jsons(dir.path());

        // Should discover: sdk, frontend, backend, root (order: workspace
        // members first, then their parents, then root last)
        assert!(
            found.contains(&sdk.join("package.json")),
            "Should discover workspace member sdk/package.json"
        );
        assert!(
            found.contains(&frontend.join("package.json")),
            "Should discover nested workspace root frontend/package.json"
        );
        assert!(
            found.contains(&backend.join("package.json")),
            "Should discover backend/package.json"
        );
        assert_eq!(
            found.last().unwrap(),
            &dir.path().join("package.json"),
            "Root package.json should be last"
        );
    }

    #[test]
    fn test_find_package_jsons_root_with_workspaces_unchanged() {
        // When root has workspaces, existing behaviour should be preserved
        // and subdirectory scanning should NOT happen.
        let dir = tempfile::tempdir().unwrap();

        std::fs::write(
            dir.path().join("package.json"),
            r#"{ "name": "root", "workspaces": ["packages/*"] }"#,
        )
        .unwrap();

        let pkg_a = dir.path().join("packages").join("a");
        std::fs::create_dir_all(&pkg_a).unwrap();
        std::fs::write(
            pkg_a.join("package.json"),
            r#"{ "name": "a" }"#,
        )
        .unwrap();

        // A sibling directory that should NOT be scanned (root has workspaces)
        let other = dir.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(
            other.join("package.json"),
            r#"{ "name": "other" }"#,
        )
        .unwrap();

        let found = find_package_jsons(dir.path());

        assert!(
            found.contains(&pkg_a.join("package.json")),
            "Should discover workspace member packages/a/package.json"
        );
        assert!(
            !found.contains(&other.join("package.json")),
            "Should NOT discover other/package.json when root has workspaces"
        );
        assert_eq!(
            found.last().unwrap(),
            &dir.path().join("package.json"),
            "Root package.json should be last"
        );
    }

    #[test]
    fn test_find_package_jsons_skips_node_modules() {
        let dir = tempfile::tempdir().unwrap();

        // Root: no workspaces
        std::fs::write(
            dir.path().join("package.json"),
            r#"{ "name": "root" }"#,
        )
        .unwrap();

        // node_modules/some-pkg/package.json — must be skipped
        let nm = dir.path().join("node_modules").join("some-pkg");
        std::fs::create_dir_all(&nm).unwrap();
        std::fs::write(
            nm.join("package.json"),
            r#"{ "name": "some-pkg" }"#,
        )
        .unwrap();

        let found = find_package_jsons(dir.path());

        assert!(
            !found.contains(&nm.join("package.json")),
            "Should NOT discover node_modules package.json"
        );
        assert_eq!(found.len(), 1, "Only root package.json should be found");
    }

    #[test]
    fn test_check_dependencies_nested_workspace_root_with_v5_deps() {
        // End-to-end: nested workspace root has PF v5 deps that should be
        // detected by a dep-update rule with upperbound 5.99.99.
        let dir = tempfile::tempdir().unwrap();

        // Root: no workspaces, no PF deps
        std::fs::write(
            dir.path().join("package.json"),
            r#"{ "name": "root", "scripts": {} }"#,
        )
        .unwrap();

        // frontend/package.json: PF v5 deps + workspaces
        let frontend = dir.path().join("frontend");
        std::fs::create_dir_all(&frontend).unwrap();
        std::fs::write(
            frontend.join("package.json"),
            r#"{
  "name": "frontend",
  "workspaces": ["packages/*"],
  "dependencies": {
    "@patternfly/react-core": "^5.4.10",
    "@patternfly/react-icons": "^5.4.2"
  }
}"#,
        )
        .unwrap();

        // frontend/packages/sdk/package.json: PF v6 deps (already migrated)
        let sdk = frontend.join("packages").join("sdk");
        std::fs::create_dir_all(&sdk).unwrap();
        std::fs::write(
            sdk.join("package.json"),
            r#"{
  "name": "@org/sdk",
  "dependencies": {
    "@patternfly/react-core": "^6.2.2"
  }
}"#,
        )
        .unwrap();

        let condition = DependencyCondition {
            name: Some("@patternfly/react-core".into()),
            nameregex: None,
            upperbound: Some("5.99.99".into()),
            lowerbound: None,
        };

        let incidents = check_dependencies(dir.path(), &condition).unwrap();

        // Should find exactly 1 incident: frontend/package.json has ^5.4.10
        // sdk has ^6.2.2 (above upperbound, no match)
        // root has no PF deps (no match)
        assert_eq!(
            incidents.len(),
            1,
            "Expected 1 incident for frontend v5 dep, got {}",
            incidents.len()
        );

        let incident = &incidents[0];
        assert!(
            incident.file_uri.contains("frontend/package.json"),
            "Incident should point to frontend/package.json, got: {}",
            incident.file_uri
        );
        assert_eq!(
            incident
                .variables
                .get("dependencyName")
                .and_then(|v| v.as_str()),
            Some("@patternfly/react-core")
        );
        assert_eq!(
            incident
                .variables
                .get("dependencyVersion")
                .and_then(|v| v.as_str()),
            Some("^5.4.10")
        );
    }
}
