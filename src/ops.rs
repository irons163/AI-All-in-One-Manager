//! Account operations: save, switch, sync, login, remove, rename, restore.
//!
//! The rule that keeps switching safe: the live login is always copied to a
//! backup before it is overwritten, and it is folded back into the active
//! account's snapshot first whenever it still belongs to that account (tools
//! refresh their tokens in place, so the live copy is the freshest one).

use crate::artifact::{Artifact, Kind};
use crate::ctx::Ctx;
use crate::provider::Provider;
use crate::store::{self, Loc, Meta};
use crate::util::{self, atomic_write, ensure_private_dir, now_secs};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const KEEP_BACKUPS: usize = 10;
/// How long to wait for a desktop app to quit after asking it to.
const QUIT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, PartialEq)]
enum Relaunch {
    /// Start the app again only if it was running before.
    IfWasRunning,
    /// Start the app afterwards in any case (the user has to act in it).
    Always,
}

/// Run `f` with the provider's desktop app closed, for providers that have
/// one. The app is asked to quit normally; if it is still running after
/// `QUIT_TIMEOUT` (it may be asking the user to confirm), nothing is changed.
/// Returns whether the app was started again afterwards.
fn with_app_closed<T>(
    ctx: &Ctx,
    p: &Provider,
    relaunch: Relaunch,
    f: impl FnOnce() -> Result<T>,
) -> Result<(T, bool)> {
    let Some(app) = &p.app else {
        return f().map(|value| (value, false));
    };
    let was_running = ctx.apps.is_running(&app.bundle_id)?;
    if was_running {
        ctx.apps.quit(&app.bundle_id)?;
        let timeout = ctx
            .var("AIOM_QUIT_TIMEOUT_SECS")
            .and_then(|v| v.parse().ok())
            .map(Duration::from_secs)
            .unwrap_or(QUIT_TIMEOUT);
        let deadline = Instant::now() + timeout;
        while ctx.apps.is_running(&app.bundle_id)? {
            if Instant::now() >= deadline {
                bail!(
                    "{} did not quit within {} seconds; nothing was changed. It may be waiting for you to confirm, or macOS may not allow this program to control it (System Settings > Privacy & Security > Automation)",
                    app.name,
                    timeout.as_secs()
                );
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
    let result = f();
    let start = match relaunch {
        Relaunch::Always => true,
        Relaunch::IfWasRunning => was_running,
    };
    let mut started = false;
    if start {
        match ctx.apps.launch(&app.bundle_id) {
            Ok(()) => started = true,
            Err(err) if result.is_ok() => {
                return Err(err.context(format!(
                    "the change was made, but {} could not be started again",
                    app.name
                )))
            }
            Err(_) => {}
        }
    }
    result.map(|value| (value, started))
}

/// Content of every artifact of a provider, in artifact order.
type Live = Vec<Option<Vec<u8>>>;

fn read_live(ctx: &Ctx, p: &Provider) -> Result<Live> {
    p.artifacts
        .iter()
        .map(|a| {
            a.read_live(ctx)
                .with_context(|| format!("reading {}", a.describe()))
        })
        .collect()
}

fn has_primary(p: &Provider, live: &Live) -> bool {
    p.artifacts
        .iter()
        .zip(live)
        .any(|(a, data)| a.primary && data.is_some())
}

fn snap_put(ctx: &Ctx, loc: &Loc, art: &Artifact, data: Option<&[u8]>) -> Result<()> {
    match &art.kind {
        Kind::Keychain { .. } => match data {
            Some(bytes) => ctx.secrets.set(
                &loc.service,
                &art.id,
                std::str::from_utf8(bytes).context("keychain secret is not valid UTF-8")?,
            ),
            None => ctx.secrets.delete(&loc.service, &art.id).map(|_| ()),
        },
        _ => {
            let path = loc.dir.join(&art.id);
            match data {
                Some(bytes) => atomic_write(&path, bytes),
                None => {
                    if path.is_file() {
                        fs::remove_file(&path)?;
                    }
                    Ok(())
                }
            }
        }
    }
}

fn snap_get(ctx: &Ctx, loc: &Loc, art: &Artifact) -> Result<Option<Vec<u8>>> {
    match &art.kind {
        Kind::Keychain { .. } => Ok(ctx
            .secrets
            .get(&loc.service, &art.id)?
            .map(String::into_bytes)),
        _ => {
            let path = loc.dir.join(&art.id);
            if !path.is_file() {
                return Ok(None);
            }
            Ok(Some(fs::read(&path)?))
        }
    }
}

/// Store `live` at `loc`. Returns the ids of the artifacts that were present.
fn write_snapshot(ctx: &Ctx, p: &Provider, loc: &Loc, live: &Live) -> Result<Vec<String>> {
    ensure_private_dir(&ctx.data_dir)?;
    ensure_private_dir(&loc.dir)?;
    let mut present = Vec::new();
    for (art, data) in p.artifacts.iter().zip(live) {
        snap_put(ctx, loc, art, data.as_deref())?;
        if data.is_some() {
            present.push(art.id.clone());
        }
    }
    Ok(present)
}

/// Make the live login equal to the snapshot at `loc`: artifacts listed in
/// `present` are written, all others are removed. The whole snapshot is read
/// before anything live is touched, so a damaged snapshot changes nothing.
fn apply(ctx: &Ctx, p: &Provider, loc: &Loc, present: &[String]) -> Result<()> {
    let mut staged: Live = Vec::with_capacity(p.artifacts.len());
    for art in &p.artifacts {
        let data = if present.contains(&art.id) {
            Some(snap_get(ctx, loc, art)?.ok_or_else(|| {
                anyhow!(
                    "the saved copy of '{}' is missing from {}",
                    art.id,
                    loc.dir.display()
                )
            })?)
        } else {
            None
        };
        staged.push(data);
    }
    for (art, data) in p.artifacts.iter().zip(&staged) {
        art.write_live(ctx, data.as_deref())
            .with_context(|| format!("updating {}", art.describe()))?;
    }
    Ok(())
}

/// Two identities are compatible unless both are known and differ.
fn compatible(a: &Option<String>, b: &Option<String>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
        _ => true,
    }
}

fn who(identity: &Option<String>) -> &str {
    identity.as_deref().unwrap_or("an unidentified account")
}

#[derive(Serialize, Deserialize)]
struct BackupMeta {
    created_at: u64,
    identity: Option<String>,
    artifacts: Vec<String>,
}

struct Backup {
    loc: Loc,
    present: Vec<String>,
}

fn backup_loc(ctx: &Ctx, p: &Provider, dir_name: &str) -> Loc {
    Loc {
        dir: store::backups_root(ctx, &p.id).join(dir_name),
        // One keychain slot per provider: only the newest backup keeps its
        // keychain artifacts.
        service: format!("aiom-backup:{}", p.id),
    }
}

fn backup(ctx: &Ctx, p: &Provider, live: &Live) -> Result<Backup> {
    let now = now_secs();
    // Several switches can land in the same second; never reuse a directory.
    let base = util::stamp(now);
    let mut dir_name = base.clone();
    let mut attempt = 1;
    while store::backups_root(ctx, &p.id).join(&dir_name).exists() {
        attempt += 1;
        dir_name = format!("{base}-{attempt:02}");
    }
    let loc = backup_loc(ctx, p, &dir_name);
    let present = write_snapshot(ctx, p, &loc, live)?;
    let meta = BackupMeta {
        created_at: now,
        identity: p.live_identity(),
        artifacts: present.clone(),
    };
    atomic_write(
        &loc.dir.join("meta.json"),
        serde_json::to_string_pretty(&meta)?.as_bytes(),
    )?;
    prune_backups(ctx, p);
    Ok(Backup { loc, present })
}

fn backup_dirs(ctx: &Ctx, p: &Provider) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(store::backups_root(ctx, &p.id))
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().join("meta.json").is_file())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Whether there is a backup that `restore` could put back.
pub fn has_backup(ctx: &Ctx, p: &Provider) -> bool {
    !backup_dirs(ctx, p).is_empty()
}

fn prune_backups(ctx: &Ctx, p: &Provider) {
    let names = backup_dirs(ctx, p);
    if names.len() > KEEP_BACKUPS {
        for name in &names[..names.len() - KEEP_BACKUPS] {
            let _ = fs::remove_dir_all(store::backups_root(ctx, &p.id).join(name));
        }
    }
}

/// Fold the live login back into the active account's snapshot, if it still
/// belongs to that account. Returns whether it was synced; otherwise explains
/// why not in `warnings`.
fn sync_back(
    ctx: &Ctx,
    p: &Provider,
    live: &Live,
    active: Option<&str>,
    backup_dir: &std::path::Path,
    warnings: &mut Vec<String>,
) -> Result<bool> {
    let live_id = p.live_identity();
    let Some(name) = active else {
        warnings.push(format!(
            "the previous {} login ({}) was not saved as an account; a copy is in {}",
            p.name,
            who(&live_id),
            backup_dir.display()
        ));
        return Ok(false);
    };
    let Some(mut meta) = store::load_meta(ctx, &p.id, name)? else {
        return Ok(false);
    };
    if !compatible(&live_id, &meta.identity) {
        warnings.push(format!(
            "the live {} login is {} but account '{}' is {}; '{}' was left unchanged and a copy of the live login is in {}",
            p.name,
            who(&live_id),
            name,
            who(&meta.identity),
            name,
            backup_dir.display()
        ));
        return Ok(false);
    }
    meta.artifacts = write_snapshot(ctx, p, &store::profile_loc(ctx, &p.id, name), live)?;
    meta.updated_at = now_secs();
    if live_id.is_some() {
        meta.identity = live_id;
    }
    if let Some(label) = p.live_label() {
        meta.label = Some(label);
    }
    store::save_meta(ctx, &meta)?;
    Ok(true)
}

// ---- public operations -------------------------------------------------------

/// Save the provider's current login as account `name` and make it active.
pub fn save(
    ctx: &Ctx,
    p: &Provider,
    name: &str,
    force: bool,
    note: Option<String>,
) -> Result<Meta> {
    store::validate_name(name)?;
    let existing = store::load_meta(ctx, &p.id, name)?;
    if existing.is_some() && !force {
        bail!(
            "{} account '{name}' already exists (use --force to overwrite it)",
            p.name
        );
    }
    let live = read_live(ctx, p)?;
    if !has_primary(p, &live) {
        bail!(
            "{} is not logged in, so there is nothing to save (looked for: {})",
            p.name,
            p.artifacts
                .iter()
                .filter(|a| a.primary)
                .map(Artifact::describe)
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
    let artifacts = write_snapshot(ctx, p, &store::profile_loc(ctx, &p.id, name), &live)?;
    let now = now_secs();
    let meta = Meta {
        name: name.to_string(),
        provider: p.id.clone(),
        created_at: existing.as_ref().map(|m| m.created_at).unwrap_or(now),
        updated_at: now,
        identity: p.live_identity(),
        label: p
            .live_label()
            .or_else(|| existing.as_ref().and_then(|m| m.label.clone())),
        note: note.or_else(|| existing.and_then(|m| m.note)),
        artifacts,
    };
    store::save_meta(ctx, &meta)?;
    store::set_active(ctx, &p.id, Some(name))?;
    Ok(meta)
}

#[derive(Debug)]
pub struct Adopted {
    pub name: String,
    pub identity: Option<String>,
    pub label: Option<String>,
    /// `true` when a new account was created, `false` when the live login
    /// turned out to be an account that was already saved.
    pub created: bool,
}

/// Whether the saved account holds exactly the credentials that are live now.
/// Used to recognise a login when the provider does not say whose it is.
fn same_credentials(ctx: &Ctx, p: &Provider, meta: &Meta, live: &Live) -> Result<bool> {
    let loc = store::profile_loc(ctx, &p.id, &meta.name);
    for (art, data) in p.artifacts.iter().zip(live) {
        if art.primary && snap_get(ctx, &loc, art)? != *data {
            return Ok(false);
        }
    }
    Ok(true)
}

/// A free account name for a login: the part of the email before the `@`,
/// then the whole email, then numbered variants.
fn fresh_name(accounts: &[Meta], identity: Option<&str>) -> String {
    let clean = |raw: &str| -> String {
        raw.chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@' | '+'))
            .skip_while(|c| !c.is_ascii_alphanumeric())
            .take(56)
            .collect()
    };
    let usable = |name: &str| {
        store::validate_name(name).is_ok() && !accounts.iter().any(|m| m.name == name)
    };
    let mut candidates = Vec::new();
    // Only an email makes a readable name; an opaque id (a UUID) does not.
    if let Some(identity) = identity.filter(|i| i.contains('@')) {
        candidates.push(clean(identity.split('@').next().unwrap_or(identity)));
        candidates.push(clean(identity));
    }
    candidates.push("account".to_string());
    if let Some(name) = candidates.iter().find(|c| usable(c)) {
        return name.clone();
    }
    let base = candidates
        .into_iter()
        .find(|c| store::validate_name(c).is_ok())
        .unwrap_or_else(|| "account".to_string());
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|name| usable(name))
        .expect("an unused numbered name always exists")
}

/// Make sure the live login is saved: if it is not the active account, attach
/// it to the saved account it belongs to, or save it as a new one named after
/// its identity. Returns `None` when there was nothing to do.
pub fn adopt(ctx: &Ctx, p: &Provider) -> Result<Option<Adopted>> {
    // Cheap checks first: this runs on every refresh of the app and must not
    // read keychain secrets unless there really is something to save.
    if !p.logged_in(ctx) {
        return Ok(None);
    }
    let live_id = p.live_identity();
    if let Some(name) = store::active(ctx, &p.id)? {
        if let Some(mut meta) = store::load_meta(ctx, &p.id, &name)? {
            if compatible(&live_id, &meta.identity) {
                // Nothing to save, but an account saved before its label was
                // readable can learn it now. Metadata only; no credentials.
                if let Some(label) = p.live_label().filter(|l| meta.label.as_ref() != Some(l)) {
                    meta.label = Some(label);
                    store::save_meta(ctx, &meta)?;
                }
                return Ok(None);
            }
        }
    }
    let live = read_live(ctx, p)?;
    if !has_primary(p, &live) {
        return Ok(None);
    }

    let accounts = store::list(ctx, &p.id)?;
    let mut existing = None;
    for meta in &accounts {
        let matches = match (&live_id, &meta.identity) {
            (Some(live), Some(saved)) => live.eq_ignore_ascii_case(saved),
            (None, None) => same_credentials(ctx, p, meta, &live)?,
            _ => false,
        };
        if matches {
            existing = Some(meta.clone());
            break;
        }
    }

    if let Some(mut meta) = existing {
        meta.artifacts = write_snapshot(ctx, p, &store::profile_loc(ctx, &p.id, &meta.name), &live)?;
        meta.updated_at = now_secs();
        if let Some(label) = p.live_label() {
            meta.label = Some(label);
        }
        store::save_meta(ctx, &meta)?;
        store::set_active(ctx, &p.id, Some(&meta.name))?;
        return Ok(Some(Adopted {
            name: meta.name,
            identity: meta.identity,
            label: meta.label,
            created: false,
        }));
    }

    let state = store::load_state(ctx)?;
    let pending = state
        .pending
        .get(&p.id)
        .filter(|n| store::validate_name(n).is_ok() && !accounts.iter().any(|m| &m.name == *n))
        .cloned();
    let label = p.live_label();
    let name = pending.unwrap_or_else(|| {
        fresh_name(&accounts, label.as_deref().or(live_id.as_deref()))
    });
    let meta = save(ctx, p, &name, false, None)?;
    if state.pending.contains_key(&p.id) {
        store::set_pending(ctx, &p.id, None)?;
    }
    Ok(Some(Adopted {
        name: meta.name,
        identity: meta.identity,
        label: meta.label,
        created: true,
    }))
}

#[derive(Debug, Default)]
pub struct SwitchReport {
    pub from: Option<String>,
    pub to: String,
    pub identity: Option<String>,
    /// Friendlier name for the target account (an email), when known.
    pub label: Option<String>,
    /// Nothing was changed because the account was already active.
    pub already_active: bool,
    /// The previous account's snapshot was refreshed from the live login.
    pub synced: bool,
    pub backup: Option<PathBuf>,
    pub warnings: Vec<String>,
    /// The provider's desktop app was closed for the switch and started again.
    pub restarted_app: Option<String>,
}

/// Switch the provider's live login to account `name`.
pub fn switch(ctx: &Ctx, p: &Provider, name: &str, force: bool) -> Result<SwitchReport> {
    let target = store::load_meta(ctx, &p.id, name)?.ok_or_else(|| {
        anyhow!(
            "{} has no account named '{name}' (see `aiom list {}`)",
            p.name,
            p.id
        )
    })?;
    let active = store::active(ctx, &p.id)?;
    let mut report = SwitchReport {
        from: active.clone(),
        to: name.to_string(),
        identity: target.identity.clone(),
        label: target.label.clone(),
        ..SwitchReport::default()
    };

    let same = active.as_deref() == Some(name);
    if same && !force && p.logged_in(ctx) && compatible(&p.live_identity(), &target.identity) {
        // The live copy is this account and is at least as fresh as the
        // snapshot; re-applying could roll back a refreshed token. Checked
        // before closing any app, so a no-op switch never interrupts it.
        report.already_active = true;
        return Ok(report);
    }

    let (_, restarted) = with_app_closed(ctx, p, Relaunch::IfWasRunning, || {
        switch_closed(ctx, p, name, &target, active.as_deref(), &mut report)
    })?;
    if restarted {
        report.restarted_app = p.app.as_ref().map(|app| app.name.clone());
    }
    Ok(report)
}

/// The part of `switch` that touches live files; runs with any app closed.
fn switch_closed(
    ctx: &Ctx,
    p: &Provider,
    name: &str,
    target: &Meta,
    active: Option<&str>,
    report: &mut SwitchReport,
) -> Result<()> {
    let same = active == Some(name);
    let live = read_live(ctx, p)?;
    let logged_in = has_primary(p, &live);
    let live_id = p.live_identity();
    if logged_in {
        let made = backup(ctx, p, &live)?;
        if same {
            if !compatible(&live_id, &target.identity) {
                report.warnings.push(format!(
                    "the live {} login was {}, not '{name}' ({}); a copy of it is in {}",
                    p.name,
                    who(&live_id),
                    who(&target.identity),
                    made.loc.dir.display()
                ));
            }
        } else {
            report.synced = sync_back(ctx, p, &live, active, &made.loc.dir, &mut report.warnings)?;
        }
        report.backup = Some(made.loc.dir);
    }

    apply(ctx, p, &store::profile_loc(ctx, &p.id, name), &target.artifacts)?;
    store::set_active(ctx, &p.id, Some(name))?;
    Ok(())
}

/// The account after the active one, wrapping around.
pub fn next_account(ctx: &Ctx, p: &Provider) -> Result<String> {
    let accounts = store::list(ctx, &p.id)?;
    if accounts.is_empty() {
        bail!("{} has no saved accounts", p.name);
    }
    let active = store::active(ctx, &p.id)?;
    let index = match active {
        Some(name) => accounts
            .iter()
            .position(|m| m.name == name)
            .map(|i| (i + 1) % accounts.len())
            .unwrap_or(0),
        None => 0,
    };
    Ok(accounts[index].name.clone())
}

/// Refresh the active account's snapshot from the live login.
pub fn sync(ctx: &Ctx, p: &Provider, force: bool) -> Result<Meta> {
    let name = store::active(ctx, &p.id)?
        .ok_or_else(|| anyhow!("{} has no active account to sync", p.name))?;
    let mut meta = store::load_meta(ctx, &p.id, &name)?
        .ok_or_else(|| anyhow!("account '{name}' disappeared"))?;
    let live = read_live(ctx, p)?;
    if !has_primary(p, &live) {
        bail!("{} is not logged in", p.name);
    }
    let live_id = p.live_identity();
    if !compatible(&live_id, &meta.identity) && !force {
        bail!(
            "the live {} login is {} but the active account '{name}' is {}; save it as its own account with `aiom add {} <name>`, or use --force to overwrite '{name}'",
            p.name,
            who(&live_id),
            who(&meta.identity),
            p.id
        );
    }
    meta.artifacts = write_snapshot(ctx, p, &store::profile_loc(ctx, &p.id, &name), &live)?;
    meta.updated_at = now_secs();
    if live_id.is_some() {
        meta.identity = live_id;
    }
    if let Some(label) = p.live_label() {
        meta.label = Some(label);
    }
    store::save_meta(ctx, &meta)?;
    Ok(meta)
}

/// For an account saved without a label, read the label from its saved
/// snapshot and record it. Only the account's metadata is written; the
/// credentials are read in memory. Returns the label if one was found.
pub fn backfill_label(ctx: &Ctx, p: &Provider, name: &str) -> Result<Option<String>> {
    let Some(mut meta) = store::load_meta(ctx, &p.id, name)? else {
        return Ok(None);
    };
    if meta.label.is_some() || p.label.is_empty() {
        return Ok(meta.label);
    }
    let Some(identity) = meta.identity.clone() else {
        return Ok(None);
    };
    let loc = store::profile_loc(ctx, &p.id, name);
    let label = p.saved_label(&identity, |id| {
        let art = p.artifacts.iter().find(|a| a.id == id)?;
        snap_get(ctx, &loc, art).ok().flatten()
    });
    if let Some(label) = &label {
        meta.label = Some(label.clone());
        store::save_meta(ctx, &meta)?;
    }
    Ok(label)
}

/// One credential file of a saved account, read in memory: the live copy
/// when the live login is this (active) account, since the CLI refreshes
/// tokens in place and the live copy is then the freshest; otherwise the
/// saved snapshot.
pub fn account_artifact(
    ctx: &Ctx,
    p: &Provider,
    meta: &Meta,
    active: Option<&str>,
    artifact: &str,
) -> Result<Option<Vec<u8>>> {
    let art = p
        .artifacts
        .iter()
        .find(|a| a.id == artifact)
        .ok_or_else(|| anyhow!("{} has no credential named '{artifact}'", p.name))?;
    if active == Some(meta.name.as_str())
        && p.logged_in(ctx)
        && compatible(&p.live_identity(), &meta.identity)
    {
        if let Some(data) = art.read_live(ctx)? {
            return Ok(Some(data));
        }
    }
    snap_get(ctx, &store::profile_loc(ctx, &p.id, &meta.name), art)
}

/// Delete a saved account. The live login is not touched.
pub fn remove(ctx: &Ctx, p: &Provider, name: &str) -> Result<()> {
    if store::load_meta(ctx, &p.id, name)?.is_none() {
        bail!("{} has no account named '{name}'", p.name);
    }
    let loc = store::profile_loc(ctx, &p.id, name);
    for art in &p.artifacts {
        if matches!(art.kind, Kind::Keychain { .. }) {
            ctx.secrets.delete(&loc.service, &art.id)?;
        }
    }
    fs::remove_dir_all(&loc.dir).with_context(|| format!("removing {}", loc.dir.display()))?;
    let state = store::load_state(ctx)?;
    if state.active.get(&p.id).map(String::as_str) == Some(name) {
        store::set_active(ctx, &p.id, None)?;
    }
    Ok(())
}

pub fn rename(ctx: &Ctx, p: &Provider, old: &str, new: &str) -> Result<()> {
    store::validate_name(new)?;
    let mut meta = store::load_meta(ctx, &p.id, old)?
        .ok_or_else(|| anyhow!("{} has no account named '{old}'", p.name))?;
    if store::load_meta(ctx, &p.id, new)?.is_some() {
        bail!("{} account '{new}' already exists", p.name);
    }
    let from = store::profile_loc(ctx, &p.id, old);
    let to = store::profile_loc(ctx, &p.id, new);
    for art in &p.artifacts {
        if matches!(art.kind, Kind::Keychain { .. }) {
            if let Some(secret) = ctx.secrets.get(&from.service, &art.id)? {
                ctx.secrets.set(&to.service, &art.id, &secret)?;
                ctx.secrets.delete(&from.service, &art.id)?;
            }
        }
    }
    fs::rename(&from.dir, &to.dir)
        .with_context(|| format!("renaming {}", from.dir.display()))?;
    meta.name = new.to_string();
    store::save_meta(ctx, &meta)?;
    let state = store::load_state(ctx)?;
    if state.active.get(&p.id).map(String::as_str) == Some(old) {
        store::set_active(ctx, &p.id, Some(new))?;
    }
    Ok(())
}

/// Run the provider's own login flow for a new account and save the result.
/// The current login is set aside first and put back if the login fails.
pub fn login(ctx: &Ctx, p: &Provider, name: &str, force: bool) -> Result<(Meta, Vec<String>)> {
    store::validate_name(name)?;
    if store::load_meta(ctx, &p.id, name)?.is_some() && !force {
        bail!(
            "{} account '{name}' already exists (use --force to replace it)",
            p.name
        );
    }
    let cmd = p
        .login
        .as_ref()
        .ok_or_else(|| anyhow!("{} has no login command configured", p.name))?;
    let program = util::find_in_path(&cmd[0], ctx.var("PATH"))
        .ok_or_else(|| anyhow!("'{}' was not found on PATH; is {} installed?", cmd[0], p.name))?;

    // A desktop app sharing this login (the Codex app) would keep using the
    // old one from memory, so it stays closed until the login is done.
    let (result, _) = with_app_closed(ctx, p, Relaunch::IfWasRunning, || {
        login_closed(ctx, p, name, &program, &cmd[1..])
    })?;
    Ok(result)
}

/// The part of `login` that touches live files; runs with any app closed.
fn login_closed(
    ctx: &Ctx,
    p: &Provider,
    name: &str,
    program: &Path,
    args: &[String],
) -> Result<(Meta, Vec<String>)> {
    let mut warnings = Vec::new();
    let live = read_live(ctx, p)?;
    let previous = if has_primary(p, &live) {
        let made = backup(ctx, p, &live)?;
        let active = store::active(ctx, &p.id)?;
        sync_back(ctx, p, &live, active.as_deref(), &made.loc.dir, &mut warnings)?;
        Some(made)
    } else {
        None
    };

    // Log out locally so the CLI starts a fresh login instead of reusing the
    // current one.
    let empty = Loc {
        dir: PathBuf::new(),
        service: String::new(),
    };
    apply(ctx, p, &empty, &[])?;

    let finished = Command::new(program)
        .args(args)
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    let logged_in = has_primary(p, &read_live(ctx, p)?);

    if !finished || !logged_in {
        if let Some(prev) = &previous {
            apply(ctx, p, &prev.loc, &prev.present)?;
        }
        bail!(
            "{} login did not complete{}",
            p.name,
            if previous.is_some() {
                "; the previous login was restored"
            } else {
                ""
            }
        );
    }
    let meta = save(ctx, p, name, true, None)?;
    Ok((meta, warnings))
}

/// Put back the most recent backup of the live login (taken automatically
/// before every switch). Leaves the provider with no active account.
pub fn restore(ctx: &Ctx, p: &Provider) -> Result<(PathBuf, Option<String>)> {
    let latest = backup_dirs(ctx, p)
        .pop()
        .ok_or_else(|| anyhow!("{} has no backups", p.name))?;
    let loc = backup_loc(ctx, p, &latest);
    let meta_path = loc.dir.join("meta.json");
    let meta: BackupMeta = serde_json::from_str(&fs::read_to_string(&meta_path)?)
        .with_context(|| format!("parsing {}", meta_path.display()))?;
    with_app_closed(ctx, p, Relaunch::IfWasRunning, || {
        apply(ctx, p, &loc, &meta.artifacts)?;
        store::set_active(ctx, &p.id, None)
    })?;
    Ok((loc.dir, meta.identity))
}

/// For a provider whose login happens inside its desktop app: close the app,
/// set the current login aside (backed up, and folded into its account),
/// sign the app out locally, and start it again so the user can sign in to
/// another account there. `name`, if given, is used for that account when it
/// is saved (by `adopt`, once the new login appears).
pub fn app_logout(ctx: &Ctx, p: &Provider, name: Option<&str>) -> Result<Vec<String>> {
    let app = p
        .app
        .as_ref()
        .ok_or_else(|| anyhow!("{} is not a desktop app", p.name))?;
    if p.installed_path(ctx).is_none() {
        bail!("{} is not installed", app.name);
    }
    if let Some(name) = name {
        store::validate_name(name)?;
        if store::load_meta(ctx, &p.id, name)?.is_some() {
            bail!("{} account '{name}' already exists", p.name);
        }
    }
    let mut warnings = Vec::new();
    with_app_closed(ctx, p, Relaunch::Always, || {
        let live = read_live(ctx, p)?;
        if has_primary(p, &live) {
            let made = backup(ctx, p, &live)?;
            let active = store::active(ctx, &p.id)?;
            sync_back(ctx, p, &live, active.as_deref(), &made.loc.dir, &mut warnings)?;
        }
        let empty = Loc {
            dir: PathBuf::new(),
            service: String::new(),
        };
        apply(ctx, p, &empty, &[])?;
        store::set_active(ctx, &p.id, None)?;
        store::set_pending(ctx, &p.id, name)
    })?;
    Ok(warnings)
}

pub struct Status {
    /// Path of the provider's CLI or desktop app, if it is installed.
    pub binary: Option<PathBuf>,
    pub logged_in: bool,
    pub live_identity: Option<String>,
    /// Friendlier name for the live account (an email), when known.
    pub live_label: Option<String>,
    pub active: Option<Meta>,
    /// Whether the live login still belongs to the active account.
    pub matches_active: bool,
    pub accounts: Vec<Meta>,
}

pub fn status(ctx: &Ctx, p: &Provider) -> Result<Status> {
    let accounts = store::list(ctx, &p.id)?;
    let active = match store::active(ctx, &p.id)? {
        Some(name) => store::load_meta(ctx, &p.id, &name)?,
        None => None,
    };
    let logged_in = p.logged_in(ctx);
    let live_identity = if logged_in { p.live_identity() } else { None };
    let live_label = if logged_in { p.live_label() } else { None };
    let matches_active = match &active {
        Some(meta) => logged_in && compatible(&live_identity, &meta.identity),
        None => false,
    };
    Ok(Status {
        binary: p.installed_path(ctx),
        logged_in,
        live_identity,
        live_label,
        active,
        matches_active,
        accounts,
    })
}
