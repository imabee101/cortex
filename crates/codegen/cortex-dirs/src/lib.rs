//! Home-directory resolution generally: USERPROFILE-first `home_dir`, plus
//! cortex-home (`$CORTEX_HOME` or `<home>/.cortex`). Shared by `cortex-config`
//! and `cortex-fast-worktree`.
//!
//! Which function to call:
//! - [`cortex_home`]: the usual choice, a cached, created path to build on.
//! - [`user_cortex_home`]: `None` instead of a cwd fallback when no home resolves.
//! - [`default_cortex_home`]: the `<home>/.cortex` default, ignoring `$CORTEX_HOME`, so callers can detect an override.
//! - [`resolve_cortex_home`]: a fresh, uncached resolve.
//! - [`resolve_cortex_home_with_source`]: [`resolve_cortex_home`] plus where the path came from.
//! - [`home_dir`]: the home directory itself, for sibling dot dirs (`~/.claude`, `~/.agents`, ...).
//!
//! TODO: collapse these getters by threading the path through config as an
//! explicit value.

#![deny(clippy::indexing_slicing)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Where a resolved cortex home came from, so "why did cortex pick this
/// directory?" is answerable in diagnostics without re-reading the
/// environment at the asking site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CortexHomeSource {
    /// A non-empty `$CORTEX_HOME` override.
    EnvOverride,
    /// `<home>/.cortex` derived from the home directory.
    HomeDefault,
}

/// The user's home directory via [`std::env::home_dir`]: `HOME` on Unix, `USERPROFILE` on Windows.
/// Not `dirs::home_dir()`: on Windows `dirs` ignores a redirected `USERPROFILE`.
/// Every home-anchored path must come from this one function.
#[allow(deprecated, clippy::disallowed_methods)] // the one sanctioned std::env::home_dir call
pub fn home_dir() -> Option<PathBuf> {
    std::env::home_dir()
}

/// `<home>/.cortex`, canonicalized via `dunce` (not `std::fs::canonicalize`,
/// which yields Windows `\\?\` verbatim paths).
fn cortex_home_in(home: &Path) -> PathBuf {
    dunce::canonicalize(home)
        .unwrap_or_else(|_| home.to_path_buf())
        .join(".cortex")
}

/// `$CORTEX_HOME` verbatim when non-empty, else `<home>/.cortex`.
/// Used as-is (not canonicalized) so literal prefix checks and symlink guards still see original components.
fn resolve_cortex_home_from(
    cortex_home_env: Option<&OsStr>,
    os_home: Option<&Path>,
) -> Option<(PathBuf, CortexHomeSource)> {
    if let Some(env) = cortex_home_env.filter(|env| !env.is_empty()) {
        return Some((PathBuf::from(env), CortexHomeSource::EnvOverride));
    }
    os_home.map(|home| (cortex_home_in(home), CortexHomeSource::HomeDefault))
}

/// Resolve the cortex home from the environment (fresh, no cache); `None` if neither resolves.
pub fn resolve_cortex_home() -> Option<PathBuf> {
    resolve_cortex_home_with_source().map(|(home, _)| home)
}

/// [`resolve_cortex_home`] plus the [`CortexHomeSource`] the path came from.
pub fn resolve_cortex_home_with_source() -> Option<(PathBuf, CortexHomeSource)> {
    resolve_cortex_home_from(
        std::env::var_os("CORTEX_HOME").as_deref(),
        home_dir().as_deref(),
    )
}

/// The default `<home>/.cortex`, used when `$CORTEX_HOME` is unset.
pub fn default_cortex_home() -> PathBuf {
    cortex_home_in(&home_dir().unwrap_or_else(|| PathBuf::from(".")))
}

/// The cortex home, created if missing and cached for the process; falls back to
/// [`default_cortex_home`] when neither `$CORTEX_HOME` nor a home resolves.
pub fn cortex_home() -> PathBuf {
    static CORTEX_HOME: OnceLock<PathBuf> = OnceLock::new();
    CORTEX_HOME
        .get_or_init(|| {
            let home = resolve_cortex_home().unwrap_or_else(default_cortex_home);
            if let Err(err) = std::fs::create_dir_all(&home) {
                tracing::warn!(path = %home.display(), %err, "failed to create cortex home");
            }
            home
        })
        .clone()
}

/// Like [`cortex_home`], but `None` when no home resolves (no cwd fallback).
pub fn user_cortex_home() -> Option<PathBuf> {
    resolve_cortex_home().is_some().then(cortex_home)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::ffi::OsString;

    #[test]
    fn env_wins_over_os_home() {
        let resolved =
            resolve_cortex_home_from(Some(OsStr::new("/custom/home")), Some(Path::new("/home/u")));
        assert_eq!(
            resolved,
            Some((PathBuf::from("/custom/home"), CortexHomeSource::EnvOverride))
        );
    }

    #[test]
    fn env_used_verbatim_even_when_it_exists() {
        // A real, existing dir whose canonical form differs (macOS symlinks
        // `/var` -> `/private/var`): the env value must come back unchanged.
        let tmp = tempfile::tempdir().unwrap();
        let resolved = resolve_cortex_home_from(Some(tmp.path().as_os_str()), None);
        assert_eq!(
            resolved,
            Some((tmp.path().to_path_buf(), CortexHomeSource::EnvOverride))
        );
    }

    #[test]
    fn empty_env_falls_through_to_os_home() {
        let tmp = tempfile::tempdir().unwrap();
        let resolved = resolve_cortex_home_from(Some(&OsString::new()), Some(tmp.path()));
        assert_eq!(
            resolved,
            Some((
                dunce::canonicalize(tmp.path()).unwrap().join(".cortex"),
                CortexHomeSource::HomeDefault
            ))
        );
    }

    #[test]
    fn default_cortex_home_has_no_verbatim_prefix() {
        // The reason we canonicalize via dunce: std::fs::canonicalize yields
        // `\\?\` verbatim paths on Windows that break git and byte-exact
        // comparisons. No-op assertion on Unix.
        let home = default_cortex_home();
        assert!(!home.to_string_lossy().starts_with(r"\\?\"));
        assert!(home.ends_with(".cortex"));
    }

    #[test]
    fn none_when_nothing_resolves() {
        assert_eq!(
            resolve_cortex_home_from(/* cortex_home_env */ None, /* os_home */ None),
            None
        );
    }
}
