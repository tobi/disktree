//! Cheap inspection hints. The owner's own query must confirm a root before
//! a command can act on it. These checks never authorize deletion.

use std::path::{Path, PathBuf};

use super::{Environment, Owner};

pub(super) fn owner(
    path: &Path,
    env: &Environment,
) -> Option<(Owner, PathBuf)> {
    let cache = env
        .variables
        .get("XDG_CACHE_HOME")
        .map_or_else(|| env.home.join(".cache"), PathBuf::from);
    let library = env.home.join("Library/Caches");
    let mut roots = vec![
        (Owner::Npm, env.home.join(".npm/_cacache")),
        (Owner::Npx, env.home.join(".npm/_npx")),
        (Owner::Uv, cache.join("uv")),
        (Owner::Pip, cache.join("pip")),
        (Owner::Pip, library.join("pip")),
        (Owner::Cypress, cache.join("Cypress")),
        (Owner::Cypress, library.join("Cypress")),
        (Owner::Pnpm, env.home.join(".local/share/pnpm/store")),
        (Owner::Pnpm, env.home.join("Library/pnpm/store")),
        (Owner::Homebrew, library.join("Homebrew")),
        (Owner::Homebrew, cache.join("Homebrew")),
        (Owner::Playwright, cache.join("ms-playwright")),
        (Owner::Playwright, library.join("ms-playwright")),
        (Owner::Puppeteer, cache.join("puppeteer")),
    ];
    if let Some(local) = env.variables.get("LOCALAPPDATA") {
        let local = PathBuf::from(local);
        roots.extend([
            (Owner::Npm, local.join("npm-cache/_cacache")),
            (Owner::Npx, local.join("npm-cache/_npx")),
            (Owner::Uv, local.join("uv/cache")),
            (Owner::Pip, local.join("pip/Cache")),
            (Owner::Playwright, local.join("ms-playwright")),
        ]);
    }
    for (key, owner) in [
        ("UV_CACHE_DIR", Owner::Uv),
        ("PIP_CACHE_DIR", Owner::Pip),
        ("CYPRESS_CACHE_FOLDER", Owner::Cypress),
        ("HOMEBREW_CACHE", Owner::Homebrew),
        ("PLAYWRIGHT_BROWSERS_PATH", Owner::Playwright),
        ("PUPPETEER_CACHE_DIR", Owner::Puppeteer),
    ] {
        if let Some(value) = env.variables.get(key) {
            let root = PathBuf::from(value);
            if root.is_absolute() {
                roots.push((owner, root));
            }
        }
    }
    for key in ["npm_config_cache", "NPM_CONFIG_CACHE"] {
        if let Some(root) = env.variables.get(key) {
            roots.push((Owner::Npm, PathBuf::from(root).join("_cacache")));
            roots.push((Owner::Npx, PathBuf::from(root).join("_npx")));
        }
    }
    roots
        .into_iter()
        .find(|(_, root)| path.starts_with(root))
        .or_else(|| {
            path.ancestors().find_map(|root| {
                let name = root.file_name()?.to_str()?;
                let owner = match name {
                    "_cacache" => Owner::Npm,
                    "_npx" => Owner::Npx,
                    ".pnpm-store" => Owner::Pnpm,
                    "ms-playwright" => Owner::Playwright,
                    _ => return None,
                };
                Some((owner, root.to_owned()))
            })
        })
}

pub(super) fn shape_owner(root: &Path) -> Option<Owner> {
    if root.join("wheels").is_dir()
        && (root.join("http-v2").is_dir() || root.join("http").is_dir())
    {
        return Some(Owner::Pip);
    }
    // Versioned stores and binary caches can be relocated to arbitrary
    // names. Read only a bounded immediate listing for recognition; the
    // installed owner must still verify the exact configured root.
    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.take(32).flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_str()?;
        if name.strip_prefix('v').is_some_and(|version| {
            !version.is_empty()
                && version.bytes().all(|byte| byte.is_ascii_digit())
        }) && path.join("files").is_dir()
        {
            return Some(Owner::Pnpm);
        }
        if name.split('.').count() == 3
            && name
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'.')
            && (path.join("Cypress.app").is_dir()
                || path.join("Cypress/Cypress").is_file())
        {
            return Some(Owner::Cypress);
        }
    }
    None
}

/// Suitable for review and for raw-removal preflight. An ancestor named
/// cache is a reason to inspect, never evidence that raw deletion is safe.
pub fn inspection_reason(path: &Path) -> Option<String> {
    let env = Environment::current().ok()?;
    inspection_with(path, &env)
}

pub(super) fn inspection_with(
    path: &Path,
    env: &Environment,
) -> Option<String> {
    if let Some((owner, _)) = owner(path, env) {
        return Some(format!(
            "{} requires owner-managed cleanup; no raw deletion fallback",
            owner.label()
        ));
    }
    path.ancestors().find_map(|ancestor| {
        let name = ancestor.file_name()?.to_str()?.to_ascii_lowercase();
        matches!(name.as_str(), "cache" | "caches" | ".cache" | ".npm" | "npm-cache" | ".pnpm-store" | "codex-runtimes")
            .then(|| "Cache/runtime state requires inspection and an owner cleanup preview; a cache name or regenerable classification does not authorize raw deletion".into())
    }).or_else(|| {
        // An opaque cache tag is a refusal hint for relocated caches too;
        // it is never enough evidence to build an owner cleanup command.
        path.ancestors().find_map(|ancestor| {
            (ancestor.join("CACHEDIR.TAG").is_file()
                || shape_owner(ancestor).is_some())
                .then(|| "Opaque cache/store layout requires inspection and owner cleanup; no raw deletion fallback".into())
        })
    })
}
