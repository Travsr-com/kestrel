//! Allowed workspace roots: a workspace is mounted only when its resolved path (symlinks and
//! `..` followed) is inside one of the configured roots.

use std::path::{Path, PathBuf};

/// Why a workspace path was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RootsError {
    /// The path is outside every allowed root once symlinks and `..` are resolved.
    #[error("workspace `{path}` is outside the allowed roots (sandbox.allowed_roots)")]
    OutsideAllowedRoots { path: String },
    /// The path does not exist (or cannot be resolved).
    #[error("workspace `{path}` does not exist")]
    NotFound { path: String },
    /// The path is relative.
    #[error("workspace `{path}` must be an absolute path")]
    NotAbsolute { path: String },
    /// The list of allowed roots is empty, so nothing can be mounted.
    #[error(
        "no allowed roots are configured (sandbox.allowed_roots), so no workspace can be mounted"
    )]
    NoRootsConfigured,
    /// Roots are configured but none of them exists, so none can be used.
    #[error("none of the allowed roots (sandbox.allowed_roots) exists")]
    NoUsableRoot,
}

/// Resolves `path` (following symlinks and `..`) and returns the canonical path when it is
/// inside, or equal to, one of `allowed_roots` (which are resolved the same way).
///
/// Checks run in this order: no roots, relative path, no usable root, path missing, outside.
/// A root that does not exist is skipped. Containment is by path component, so `/data/ws` does
/// not allow `/data/ws-evil`.
pub fn resolve_workspace(path: &Path, allowed_roots: &[PathBuf]) -> Result<PathBuf, RootsError> {
    let shown = || path.display().to_string();
    if allowed_roots.is_empty() {
        return Err(RootsError::NoRootsConfigured);
    }
    if !path.is_absolute() {
        return Err(RootsError::NotAbsolute { path: shown() });
    }
    let roots: Vec<PathBuf> = allowed_roots
        .iter()
        .filter_map(|r| r.canonicalize().ok())
        .collect();
    if roots.is_empty() {
        return Err(RootsError::NoUsableRoot);
    }
    let canonical = path
        .canonicalize()
        .map_err(|_| RootsError::NotFound { path: shown() })?;
    if roots.iter().any(|r| canonical.starts_with(r)) {
        Ok(canonical)
    } else {
        Err(RootsError::OutsideAllowedRoots { path: shown() })
    }
}
