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

/// The owner a path most likely belongs to: a known or configured root
/// containing it, or a recognizable layout a few levels up. A hint picks
/// which owner to ask; only that owner's own query confirms a root.
pub(super) fn hinted(
    path: &Path,
    env: &Environment,
) -> Option<(Owner, PathBuf)> {
    owner(path, env).or_else(|| {
        path.ancestors().take(4).find_map(|root| {
            shape_owner(root).map(|owner| (owner, root.to_owned()))
        })
    })
}

// Enough to name the caches inside a wrapper such as ~/.cache without
// walking every file of every cache in it on the preview worker.
const CONTAINED_DEPTH: usize = 3;
const CONTAINED_DIRECTORIES: usize = 20_000;
const CONTAINED_LISTED: usize = 8;

/// Recognized caches strictly inside `path`, for the disclosure shown
/// before deleting it directly: their owners are bypassed too. Bounded,
/// and fails closed so a disclosure is never silently incomplete.
pub(super) fn contained(
    path: &Path,
    env: &Environment,
) -> Result<Vec<String>, String> {
    let mut found = Vec::new();
    let mut pending = vec![(path.to_owned(), 0)];
    let mut visited = 0;
    while let Some((directory, depth)) = pending.pop() {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if directory == path => {
                // A file or a vanished path contains nothing.
                if error.kind() == std::io::ErrorKind::NotFound
                    || error.kind() == std::io::ErrorKind::NotADirectory
                    || path.is_file()
                {
                    return Ok(found);
                }
                return Err(format!("Cannot inspect contents: {error}"));
            }
            Err(error) => {
                return Err(format!(
                    "Cannot inspect {}: {error}",
                    directory.display()
                ));
            }
        };
        for entry in entries {
            let entry = entry.map_err(|error| error.to_string())?;
            let child = entry.path();
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            visited += 1;
            if visited > CONTAINED_DIRECTORIES {
                return Err("Too many folders to list the caches inside; inspect a smaller selection".into());
            }
            let label = owner(&child, env)
                .filter(|(_, root)| *root == child)
                .map(|(owner, _)| owner.label().to_owned())
                .or_else(|| {
                    shape_owner(&child).map(|owner| owner.label().to_owned())
                })
                .or_else(|| {
                    child
                        .join("CACHEDIR.TAG")
                        .is_file()
                        .then(|| "tagged cache".to_owned())
                });
            if let Some(label) = label {
                if found.len() < CONTAINED_LISTED {
                    found.push(format!("{label} at {}", child.display()));
                }
                // Its own contents belong to it; no need to look inside.
                continue;
            }
            if depth + 1 < CONTAINED_DEPTH {
                pending.push((child, depth + 1));
            }
        }
    }
    found.sort();
    Ok(found)
}
