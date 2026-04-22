//! Index-based lookups for cross-file data.
//!
//! Provides helper functions that extract data from the [`ReactProjectIndex`]
//! without re-parsing files. Replaces the ad-hoc cross-file resolution
//! (transparency cache, resolver map, etc.) with pre-computed index queries.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ast_index::CachedFile;
use ast_index_react::types::{ReactDefData, ReactProjectIndex, ReactRefData};
use ast_index_typescript::types::TsImportData;

use crate::transparency::WrapperInfo;

/// Type alias for the concrete CachedFile in the React index.
type ReactCachedFile = CachedFile<TsImportData, ReactDefData, ReactRefData>;

/// A pre-built lookup from file path → cached file data.
///
/// Built once per evaluation from the index. Provides O(1) file lookups
/// instead of iterating the index's package DashMap.
pub struct FileIndex {
    files: HashMap<PathBuf, Arc<ReactCachedFile>>,
}

impl FileIndex {
    /// Build a file lookup map from the React project index.
    pub fn new(react_index: &ReactProjectIndex) -> Self {
        let mut files = HashMap::new();
        for entry in react_index.packages().iter() {
            for file in entry.value() {
                files.insert(file.path.clone(), Arc::clone(file));
            }
        }
        Self { files }
    }

    /// Look up a cached file by its absolute path.
    pub fn get(&self, path: &Path) -> Option<&Arc<ReactCachedFile>> {
        self.files.get(path)
    }

    /// Build a transparency set for a file using the index.
    ///
    /// For each import in the file's import map, looks up the imported
    /// component in the index and checks its `ReactDefData.transparency`.
    /// Returns a map from local component name → wrapper info, exactly
    /// like the old `build_transparency_set()` but without any file I/O
    /// or re-parsing.
    ///
    /// `import_map` maps local names to module sources (e.g., `"Button"` → `"./Button"`).
    /// `file_path` is the absolute path of the file being scanned.
    pub fn build_transparency_set(
        &self,
        file_path: &Path,
        import_map: &HashMap<String, String>,
    ) -> HashMap<String, WrapperInfo> {
        let mut result = HashMap::new();

        // Get the current file's data from the index
        let current_file = match self.get(file_path) {
            Some(f) => f,
            None => return result,
        };

        // For each import, find the resolved file and check transparency
        for (local_name, _module_source) in import_map {
            // Find which import declaration brought this name in
            let import_info = self.find_import_for_name(current_file, local_name);

            if let Some((original_name, target_path)) = import_info {
                if let Some(target_file) = self.get(&target_path) {
                    // Look for an exported symbol matching the original name
                    for def in &target_file.symbol_defs {
                        if def.exported && def.name == original_name {
                            if let Some(ref transparency) = def.language_data.transparency {
                                if transparency.is_transparent {
                                    result.insert(
                                        local_name.clone(),
                                        transparency.wraps_in.clone(),
                                    );
                                }
                            }
                            break;
                        }
                    }
                }
            }
        }

        result
    }

    /// Find the original name and resolved file path for an imported name.
    ///
    /// Returns `(original_name, resolved_path)` or None if not found.
    fn find_import_for_name(
        &self,
        file: &ReactCachedFile,
        local_name: &str,
    ) -> Option<(String, PathBuf)> {
        for import in &file.imports {
            // Check if this import declaration includes the local name
            for sym in &import.symbols {
                let local = sym.alias.as_deref().unwrap_or(&sym.name);
                if local == local_name {
                    // Found the import. Resolve to a file path.
                    let original_name = sym.name.clone();

                    // The import source was resolved to a package name during indexing.
                    // raw_specifier has the original relative specifier.
                    let specifier = import
                        .language_data
                        .raw_specifier
                        .as_deref()
                        .unwrap_or(&import.source);

                    // Try to find the target file
                    if let Some(path) = self.resolve_specifier(file, specifier) {
                        return Some((original_name, path));
                    }

                    // If it's a package name (not relative), try finding by package
                    return self
                        .find_export_in_package(&import.source, &original_name)
                        .map(|path| (original_name, path));
                }
            }

            // Handle default imports (no symbols, is_default flag)
            if import.symbols.is_empty() && import.language_data.is_default {
                // Default import: `import Foo from './Foo'`
                // The local name is the default import name
                // But we can't easily match this without more info...
                // Skip for now — default imports are handled below
            }
        }
        None
    }

    /// Resolve an import specifier to a file path in the index.
    fn resolve_specifier(&self, from_file: &ReactCachedFile, specifier: &str) -> Option<PathBuf> {
        // Only handle relative specifiers
        if !specifier.starts_with("./") && !specifier.starts_with("../") {
            return None;
        }

        let parent_dir = from_file.path.parent()?;
        let base = parent_dir.join(specifier);

        // Try exact match
        if self.files.contains_key(&base) {
            return Some(base);
        }

        // Try with extensions
        let extensions = ["ts", "tsx", "js", "jsx", "mts", "mjs"];
        for ext in &extensions {
            let with_ext = base.with_extension(ext);
            if self.files.contains_key(&with_ext) {
                return Some(with_ext);
            }
        }

        // Try as directory with index file
        for ext in &extensions {
            let index = base.join(format!("index.{}", ext));
            if self.files.contains_key(&index) {
                return Some(index);
            }
        }

        None
    }

    /// Look up the object properties of an imported variable.
    ///
    /// Used for spread prop resolution: when `{...modalProps}` is encountered
    /// and `modalProps` is imported, this looks up the variable's
    /// `ReactDefData.object_properties` in the index.
    ///
    /// Returns `Some(properties)` if the symbol was found with non-empty
    /// object properties, `None` otherwise (falls back to file-based resolution).
    pub fn lookup_object_properties(
        &self,
        file_path: &Path,
        local_name: &str,
    ) -> Option<&[String]> {
        let current_file = self.get(file_path)?;
        let (original_name, target_path) =
            self.find_import_for_name(current_file, local_name)?;

        let target_file = self.get(&target_path)?;

        for def in &target_file.symbol_defs {
            if def.exported && def.name == original_name {
                if !def.language_data.object_properties.is_empty() {
                    return Some(&def.language_data.object_properties);
                }
                break;
            }
        }

        None
    }

    /// Find a file in the index that is part of a package and exports a given name.
    fn find_export_in_package(
        &self,
        _package_name: &str,
        _export_name: &str,
    ) -> Option<PathBuf> {
        // For now, this only works with relative imports (project-local files).
        // npm packages are in node_modules and were not indexed.
        // The old code also skips node_modules paths, so this is consistent.
        None
    }
}
