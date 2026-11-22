//! On-disk layout of aiom's own data:
//!
//! ```text
//! ~/.aiom/
//!   state.json                      which account is active per provider
//!   providers.toml                  optional user-defined providers
//!   profiles/<provider>/<account>/  meta.json + one file per artifact
//!   backups/<provider>/<stamp>/     live login as it was before a switch
//! ```
//!
//! Keychain artifacts are not written to disk; their snapshots are kept in
//! the keychain under a service name derived from the snapshot location.

use crate::ctx::Ctx;
use crate::util::{atomic_write, ensure_private_dir};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Meta {
    pub name: String,
    pub provider: String,
    pub created_at: u64,
    pub updated_at: u64,
    /// Who the snapshot belongs to (usually an email), when the provider says.
    #[serde(default)]
    pub identity: Option<String>,
    /// A friendlier name to show for that identity (an email), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    /// Ids of the artifacts that were present when the snapshot was taken.
    /// Artifacts not listed here are removed from the live login on switch.
    #[serde(default)]
    pub artifacts: Vec<String>,
}

impl Meta {
    /// What to show for this account's owner: the label if known, else the identity.
    pub fn shown(&self) -> Option<&str> {
        self.label.as_deref().or(self.identity.as_deref())
    }
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct State {
    #[serde(default)]
    pub active: BTreeMap<String, String>,
    /// Name the user chose for the next account of a provider, used when a
    /// login that happens elsewhere (inside a desktop app) is saved.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pending: BTreeMap<String, String>,
}

/// Where one snapshot lives: a directory for file-like artifacts and a
/// keychain service name for keychain artifacts.
pub struct Loc {
    pub dir: PathBuf,
    pub service: String,
}

pub fn validate_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let first_ok = chars
        .next()
        .map(|c| c.is_ascii_alphanumeric())
        .unwrap_or(false);
    let rest_ok = chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@' | '+'));
    if !first_ok || !rest_ok || name.len() > 64 {
        bail!(
            "invalid account name '{name}': use 1-64 characters of letters, digits, '.', '_', '-', '@', '+', starting with a letter or digit"
        );
    }
    Ok(())
}

pub fn profiles_root(ctx: &Ctx, provider: &str) -> PathBuf {
    ctx.data_dir.join("profiles").join(provider)
}

pub fn profile_loc(ctx: &Ctx, provider: &str, name: &str) -> Loc {
    Loc {
        dir: profiles_root(ctx, provider).join(name),
        service: format!("aiom:{provider}:{name}"),
    }
}

pub fn backups_root(ctx: &Ctx, provider: &str) -> PathBuf {
    ctx.data_dir.join("backups").join(provider)
}

pub fn load_meta(ctx: &Ctx, provider: &str, name: &str) -> Result<Option<Meta>> {
    let path = profile_loc(ctx, provider, name).dir.join("meta.json");
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let meta = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(meta))
}

pub fn save_meta(ctx: &Ctx, meta: &Meta) -> Result<()> {
    let loc = profile_loc(ctx, &meta.provider, &meta.name);
    ensure_private_dir(&ctx.data_dir)?;
    ensure_private_dir(&loc.dir)?;
    atomic_write(
        &loc.dir.join("meta.json"),
        serde_json::to_string_pretty(meta)?.as_bytes(),
    )
}

/// Saved accounts of a provider, sorted by name.
pub fn list(ctx: &Ctx, provider: &str) -> Result<Vec<Meta>> {
    let root = profiles_root(ctx, provider);
    let mut metas = Vec::new();
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(_) => return Ok(metas),
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(meta) = load_meta(ctx, provider, &name)? {
            metas.push(meta);
        }
    }
    metas.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(metas)
}

fn state_path(ctx: &Ctx) -> PathBuf {
    ctx.data_dir.join("state.json")
}

pub fn load_state(ctx: &Ctx) -> Result<State> {
    let path = state_path(ctx);
    if !path.is_file() {
        return Ok(State::default());
    }
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

pub fn save_state(ctx: &Ctx, state: &State) -> Result<()> {
    ensure_private_dir(&ctx.data_dir)?;
    atomic_write(
        &state_path(ctx),
        serde_json::to_string_pretty(state)?.as_bytes(),
    )
}

/// The active account of a provider, if it still exists.
pub fn active(ctx: &Ctx, provider: &str) -> Result<Option<String>> {
    let state = load_state(ctx)?;
    match state.active.get(provider) {
        Some(name) if load_meta(ctx, provider, name)?.is_some() => Ok(Some(name.clone())),
        _ => Ok(None),
    }
}

pub fn set_pending(ctx: &Ctx, provider: &str, name: Option<&str>) -> Result<()> {
    let mut state = load_state(ctx)?;
    match name {
        Some(name) => {
            state.pending.insert(provider.to_string(), name.to_string());
        }
        None => {
            state.pending.remove(provider);
        }
    }
    save_state(ctx, &state)
}

pub fn set_active(ctx: &Ctx, provider: &str, name: Option<&str>) -> Result<()> {
    let mut state = load_state(ctx)?;
    match name {
        Some(name) => {
            state.active.insert(provider.to_string(), name.to_string());
        }
        None => {
            state.active.remove(provider);
        }
    }
    save_state(ctx, &state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_names() {
        for good in ["work", "me@corp.com", "a", "Team-2_x+y"] {
            assert!(validate_name(good).is_ok(), "{good}");
        }
        for bad in ["", ".hidden", "a/b", "a b", "../x", "-flag"] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
        assert!(validate_name(&"x".repeat(64)).is_ok());
        assert!(validate_name(&"x".repeat(65)).is_err());
    }

    #[test]
    fn state_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::for_test(dir.path(), false);
        assert_eq!(active(&ctx, "codex").unwrap(), None);
        let meta = Meta {
            name: "work".into(),
            provider: "codex".into(),
            created_at: 1,
            updated_at: 2,
            identity: Some("a@b.c".into()),
            label: None,
            note: None,
            artifacts: vec!["auth.json".into()],
        };
        save_meta(&ctx, &meta).unwrap();
        set_active(&ctx, "codex", Some("work")).unwrap();
        assert_eq!(active(&ctx, "codex").unwrap().as_deref(), Some("work"));
        assert_eq!(list(&ctx, "codex").unwrap().len(), 1);

        set_active(&ctx, "codex", Some("gone")).unwrap();
        assert_eq!(active(&ctx, "codex").unwrap(), None, "dangling active is ignored");
    }
}
