//! An artifact is one piece of a provider's login state: a whole file, a few
//! keys inside a shared JSON file, a whole directory, or an item in the OS
//! keychain.

use crate::ctx::Ctx;
use crate::util::{atomic_write, json_get, json_remove, json_set, set_mode};
use anyhow::{bail, Context, Result};
use serde_json::{Map, Value};
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone)]
pub enum Kind {
    /// The whole file belongs to the login (e.g. `~/.codex/auth.json`).
    File { path: PathBuf },
    /// Only some keys of a JSON file belong to the login; the rest of the
    /// file is left alone (e.g. `oauthAccount` inside `~/.claude.json`).
    /// Keys are dotted paths. The snapshot is a JSON object of key -> value.
    JsonKeys { path: PathBuf, keys: Vec<String> },
    /// A whole directory tree (e.g. an app's local web storage). The snapshot
    /// is a single archive of every file in it.
    Dir { path: PathBuf },
    /// A generic-password item in the OS keychain.
    Keychain { service: String, account: String },
}

#[derive(Debug, Clone)]
pub struct Artifact {
    /// Stable name of this artifact inside a snapshot directory.
    pub id: String,
    pub kind: Kind,
    /// A provider counts as logged in when any primary artifact is present.
    pub primary: bool,
}

impl Artifact {
    pub fn file(id: &str, path: PathBuf, primary: bool) -> Self {
        Artifact {
            id: id.to_string(),
            kind: Kind::File { path },
            primary,
        }
    }

    pub fn json_keys(id: &str, path: PathBuf, keys: &[&str], primary: bool) -> Self {
        Artifact {
            id: id.to_string(),
            kind: Kind::JsonKeys {
                path,
                keys: keys.iter().map(|k| k.to_string()).collect(),
            },
            primary,
        }
    }

    pub fn dir(id: &str, path: PathBuf, primary: bool) -> Self {
        Artifact {
            id: id.to_string(),
            kind: Kind::Dir { path },
            primary,
        }
    }

    pub fn keychain(id: &str, service: &str, account: &str, primary: bool) -> Self {
        Artifact {
            id: id.to_string(),
            kind: Kind::Keychain {
                service: service.to_string(),
                account: account.to_string(),
            },
            primary,
        }
    }

    pub fn describe(&self) -> String {
        match &self.kind {
            Kind::File { path } => path.display().to_string(),
            Kind::JsonKeys { path, keys } => format!("{} [{}]", path.display(), keys.join(", ")),
            Kind::Dir { path } => format!("{}/", path.display()),
            Kind::Keychain { service, account } => format!("keychain: {service} ({account})"),
        }
    }

    /// Cheap presence check that never reads a keychain secret.
    pub fn exists_live(&self, ctx: &Ctx) -> bool {
        match &self.kind {
            Kind::File { path } => path.is_file(),
            Kind::JsonKeys { .. } => matches!(self.read_live(ctx), Ok(Some(_))),
            Kind::Dir { path } => path.is_dir(),
            Kind::Keychain { service, account } => {
                ctx.secrets.exists(service, account).unwrap_or(false)
            }
        }
    }

    /// The artifact's current content, or `None` if it is absent.
    pub fn read_live(&self, ctx: &Ctx) -> Result<Option<Vec<u8>>> {
        match &self.kind {
            Kind::File { path } => {
                if !path.is_file() {
                    return Ok(None);
                }
                read_stable(path)
                    .map(Some)
                    .with_context(|| format!("reading {}", path.display()))
            }
            Kind::JsonKeys { path, keys } => {
                let Some((root, _)) = load_json(path)? else {
                    return Ok(None);
                };
                let mut snap = Map::new();
                for key in keys {
                    if let Some(value) = json_get(&root, key) {
                        snap.insert(key.clone(), value.clone());
                    }
                }
                if snap.is_empty() {
                    return Ok(None);
                }
                Ok(Some(serde_json::to_vec_pretty(&Value::Object(snap))?))
            }
            Kind::Dir { path } => {
                if !path.is_dir() {
                    return Ok(None);
                }
                pack_dir(path)
                    .map(Some)
                    .with_context(|| format!("reading {}", path.display()))
            }
            Kind::Keychain { service, account } => Ok(ctx
                .secrets
                .get(service, account)?
                .map(String::into_bytes)),
        }
    }

    /// Replace the artifact's content, or remove it when `data` is `None`.
    pub fn write_live(&self, ctx: &Ctx, data: Option<&[u8]>) -> Result<()> {
        match &self.kind {
            Kind::File { path } => {
                match data {
                    Some(bytes) => atomic_write(path, bytes)?,
                    None => {
                        if path.is_file() {
                            fs::remove_file(path)
                                .with_context(|| format!("removing {}", path.display()))?;
                        }
                    }
                }
                remove_sqlite_sidecars(path)
            }
            Kind::JsonKeys { path, keys } => {
                let snap: Map<String, Value> = match data {
                    Some(bytes) => serde_json::from_slice(bytes)
                        .with_context(|| format!("snapshot '{}' is not a JSON object", self.id))?,
                    None => Map::new(),
                };
                let (mut root, style) = match load_json(path)? {
                    Some(loaded) => loaded,
                    None if path.exists() => return Ok(()), // unparseable: leave it alone
                    None if snap.is_empty() => return Ok(()), // nothing to add, nothing to remove
                    None => (Value::Object(Map::new()), Style::default()),
                };
                let mut changed = false;
                for key in keys {
                    match snap.get(key) {
                        Some(value) => {
                            if json_get(&root, key) != Some(value) {
                                json_set(&mut root, key, value.clone());
                                changed = true;
                            }
                        }
                        None => changed |= json_remove(&mut root, key),
                    }
                }
                if !changed {
                    return Ok(());
                }
                atomic_write(path, style.render(&root)?.as_bytes())
            }
            Kind::Dir { path } => match data {
                Some(bytes) => replace_dir(path, bytes)
                    .with_context(|| format!("replacing {}", path.display())),
                None => remove_dir(path)
                    .with_context(|| format!("removing {}", path.display())),
            },
            Kind::Keychain { service, account } => match data {
                Some(bytes) => {
                    let secret = std::str::from_utf8(bytes)
                        .with_context(|| format!("snapshot '{}' is not valid UTF-8", self.id))?;
                    ctx.secrets.set(service, account, secret)
                }
                None => ctx.secrets.delete(service, account).map(|_| ()),
            },
        }
    }
}

// ---- files that another program may be writing ---------------------------------

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn non_empty(path: &Path) -> bool {
    fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false)
}

/// Read a file that a running app may be writing. For an SQLite database in
/// rollback-journal mode, a non-empty `-journal` means a write is in
/// progress, so wait for it to finish and make sure nothing changed while
/// reading.
fn read_stable(path: &Path) -> Result<Vec<u8>> {
    let journal = sidecar(path, "-journal");
    let modified = |p: &Path| fs::metadata(p).and_then(|m| m.modified()).ok();
    for _ in 0..40 {
        if non_empty(&journal) {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        let before = modified(path);
        let data = fs::read(path)?;
        if !non_empty(&journal) && modified(path) == before {
            return Ok(data);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!("it kept changing while being read; try again in a moment")
}

/// After an SQLite database file is replaced, its old journal, write-ahead
/// log and shared-memory files belong to a different database. SQLite would
/// replay them into the new one and corrupt it, so remove them.
fn remove_sqlite_sidecars(path: &Path) -> Result<()> {
    for suffix in ["-journal", "-wal", "-shm"] {
        let side = sidecar(path, suffix);
        if side.is_file() {
            fs::remove_file(&side).with_context(|| format!("removing {}", side.display()))?;
        }
    }
    Ok(())
}

// ---- directory snapshots --------------------------------------------------------

const DIR_MAGIC: &[u8] = b"AIOMDIR1";

/// Serialise a directory tree into one byte string: the magic header, then
/// for each entry in sorted order a kind byte (`d` or `f`), the relative path
/// and, for files, the content. Symbolic links are skipped. Files that
/// disappear while reading (an app cleaning up) are skipped too.
pub fn pack_dir(root: &Path) -> Result<Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<u8>) -> Result<()> {
        let mut entries: Vec<_> = match fs::read_dir(dir) {
            Ok(entries) => entries.flatten().collect(),
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err).with_context(|| format!("listing {}", dir.display())),
        };
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else { continue };
            let rel = path.strip_prefix(root).expect("walked from root");
            let rel = rel
                .to_str()
                .with_context(|| format!("{} is not a UTF-8 path", path.display()))?;
            if meta.is_dir() {
                push_entry(out, b'd', rel, None);
                walk(root, &path, out)?;
            } else if meta.is_file() {
                match fs::read(&path) {
                    Ok(data) => push_entry(out, b'f', rel, Some(&data)),
                    Err(err) if err.kind() == ErrorKind::NotFound => {}
                    Err(err) => {
                        return Err(err).with_context(|| format!("reading {}", path.display()))
                    }
                }
            }
        }
        Ok(())
    }
    let mut out = DIR_MAGIC.to_vec();
    walk(root, root, &mut out)?;
    Ok(out)
}

fn push_entry(out: &mut Vec<u8>, kind: u8, rel: &str, data: Option<&[u8]>) {
    out.push(kind);
    out.extend_from_slice(&(rel.len() as u32).to_be_bytes());
    out.extend_from_slice(rel.as_bytes());
    if let Some(data) = data {
        out.extend_from_slice(&(data.len() as u64).to_be_bytes());
        out.extend_from_slice(data);
    }
}

/// Recreate a tree produced by `pack_dir` under `dest`, which must not exist.
/// Rejects any path that would land outside `dest`.
pub fn unpack_dir(bytes: &[u8], dest: &Path) -> Result<()> {
    fn take<'a>(bytes: &'a [u8], at: &mut usize, n: usize) -> Result<&'a [u8]> {
        let end = at.checked_add(n).filter(|end| *end <= bytes.len());
        let Some(end) = end else { bail!("directory snapshot is truncated") };
        let slice = &bytes[*at..end];
        *at = end;
        Ok(slice)
    }
    if !bytes.starts_with(DIR_MAGIC) {
        bail!("not a directory snapshot");
    }
    fs::create_dir(dest).with_context(|| format!("creating {}", dest.display()))?;
    set_mode(dest, 0o700)?;
    let mut at = DIR_MAGIC.len();
    while at < bytes.len() {
        let kind = take(bytes, &mut at, 1)?[0];
        let len = u32::from_be_bytes(take(bytes, &mut at, 4)?.try_into().unwrap()) as usize;
        let rel = std::str::from_utf8(take(bytes, &mut at, len)?)
            .context("directory snapshot has a non-UTF-8 path")?;
        let rel_path = Path::new(rel);
        let safe = !rel.is_empty()
            && rel_path
                .components()
                .all(|c| matches!(c, Component::Normal(_)));
        if !safe {
            bail!("directory snapshot has an unsafe path: {rel:?}");
        }
        let target = dest.join(rel_path);
        match kind {
            b'd' => {
                fs::create_dir_all(&target)?;
                set_mode(&target, 0o700)?;
            }
            b'f' => {
                let size = u64::from_be_bytes(take(bytes, &mut at, 8)?.try_into().unwrap());
                let data = take(bytes, &mut at, usize::try_from(size)?)?;
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&target, data)?;
                set_mode(&target, 0o600)?;
            }
            other => bail!("directory snapshot has an unknown entry kind {other}"),
        }
    }
    Ok(())
}

/// The files in a `pack_dir` snapshot, as (relative path, content), read in
/// memory. `None` if the bytes are not a well-formed snapshot.
pub fn packed_files(bytes: &[u8]) -> Option<Vec<(String, &[u8])>> {
    if !bytes.starts_with(DIR_MAGIC) {
        return None;
    }
    let mut files = Vec::new();
    let mut at = DIR_MAGIC.len();
    let take = |n: usize, at: &mut usize| -> Option<&[u8]> {
        let end = at.checked_add(n).filter(|end| *end <= bytes.len())?;
        let slice = &bytes[*at..end];
        *at = end;
        Some(slice)
    };
    while at < bytes.len() {
        let kind = take(1, &mut at)?[0];
        let len = u32::from_be_bytes(take(4, &mut at)?.try_into().ok()?) as usize;
        let rel = std::str::from_utf8(take(len, &mut at)?).ok()?.to_string();
        match kind {
            b'd' => {}
            b'f' => {
                let size = u64::from_be_bytes(take(8, &mut at)?.try_into().ok()?);
                files.push((rel, take(usize::try_from(size).ok()?, &mut at)?));
            }
            _ => return None,
        }
    }
    Some(files)
}

fn temp_sibling(path: &Path, tag: &str) -> Result<PathBuf> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("{} has no usable name", path.display()))?;
    Ok(path.with_file_name(format!(".{name}.aiom-{tag}-{}", std::process::id())))
}

/// Swap `path` for the tree in `bytes`. The new tree is built next to the
/// old one first, so a bad snapshot leaves the current directory untouched.
fn replace_dir(path: &Path, bytes: &[u8]) -> Result<()> {
    if path.exists() && !path.is_dir() {
        bail!("{} exists but is not a directory", path.display());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let fresh = temp_sibling(path, "new")?;
    if fresh.exists() {
        fs::remove_dir_all(&fresh)?;
    }
    if let Err(err) = unpack_dir(bytes, &fresh) {
        let _ = fs::remove_dir_all(&fresh);
        return Err(err);
    }
    if path.is_dir() {
        let old = temp_sibling(path, "old")?;
        if old.exists() {
            fs::remove_dir_all(&old)?;
        }
        fs::rename(path, &old)?;
        if let Err(err) = fs::rename(&fresh, path) {
            let _ = fs::rename(&old, path);
            let _ = fs::remove_dir_all(&fresh);
            return Err(err.into());
        }
        fs::remove_dir_all(&old)?;
    } else {
        fs::rename(&fresh, path)?;
    }
    Ok(())
}

fn remove_dir(path: &Path) -> Result<()> {
    if !path.is_dir() {
        return Ok(());
    }
    // Move it out of the way first so the name is free at once.
    let old = temp_sibling(path, "old")?;
    if old.exists() {
        fs::remove_dir_all(&old)?;
    }
    fs::rename(path, &old)?;
    fs::remove_dir_all(&old)?;
    Ok(())
}

/// How a JSON file was formatted on disk, so it can be written back the same way.
struct Style {
    pretty: bool,
    trailing_newline: bool,
}

impl Default for Style {
    fn default() -> Self {
        Style {
            pretty: true,
            trailing_newline: false,
        }
    }
}

impl Style {
    fn render(&self, value: &Value) -> Result<String> {
        let mut out = if self.pretty {
            serde_json::to_string_pretty(value)?
        } else {
            serde_json::to_string(value)?
        };
        if self.trailing_newline {
            out.push('\n');
        }
        Ok(out)
    }
}

/// Load a JSON file. `None` means it is missing, or present but not plain
/// JSON (some tools allow comments in their settings); callers then leave the
/// file untouched instead of risking it.
fn load_json(path: &PathBuf) -> Result<Option<(Value, Style)>> {
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    match serde_json::from_str::<Value>(&text) {
        Ok(value) => {
            let style = Style {
                pretty: text.trim_end().contains('\n'),
                trailing_newline: text.ends_with('\n'),
            };
            Ok(Some((value, style)))
        }
        Err(err) => {
            eprintln!(
                "warning: {} is not plain JSON ({err}); leaving it untouched",
                path.display()
            );
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_keys_only_touch_their_keys() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::for_test(dir.path(), false);
        let path = dir.path().join("config.json");
        fs::write(
            &path,
            "{\n  \"first\": 1,\n  \"oauthAccount\": {\n    \"emailAddress\": \"a@x.io\"\n  },\n  \"last\": [1, 2]\n}\n",
        )
        .unwrap();
        let art = Artifact::json_keys("cfg", path.clone(), &["oauthAccount", "absent.key"], false);

        let snap = art.read_live(&ctx).unwrap().expect("key present");
        let parsed: Value = serde_json::from_slice(&snap).unwrap();
        assert_eq!(parsed, json!({"oauthAccount": {"emailAddress": "a@x.io"}}));

        let other = serde_json::to_vec(&json!({"oauthAccount": {"emailAddress": "b@x.io"}})).unwrap();
        art.write_live(&ctx, Some(&other)).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let root: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(root["oauthAccount"]["emailAddress"], "b@x.io");
        assert_eq!(root["first"], 1);
        let keys: Vec<&String> = root.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["first", "oauthAccount", "last"]);
        assert!(text.ends_with("}\n"), "keeps trailing newline");

        art.write_live(&ctx, None).unwrap();
        let root: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(root.get("oauthAccount").is_none());
        assert_eq!(root["last"], json!([1, 2]));
        assert!(art.read_live(&ctx).unwrap().is_none());
    }

    #[test]
    fn json_keys_leave_unparseable_files_alone() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::for_test(dir.path(), false);
        let path = dir.path().join("settings.json");
        let original = "{ // comment\n \"a\": 1 }";
        fs::write(&path, original).unwrap();
        let art = Artifact::json_keys("s", path.clone(), &["a"], false);
        assert!(art.read_live(&ctx).unwrap().is_none());
        art.write_live(&ctx, Some(b"{\"a\": 2}")).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn dir_artifact_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = Ctx::for_test(tmp.path(), false);
        let dir = tmp.path().join("Local Storage");
        fs::create_dir_all(dir.join("leveldb/sub")).unwrap();
        fs::create_dir_all(dir.join("empty")).unwrap();
        fs::write(dir.join("leveldb/CURRENT"), "MANIFEST-1\n").unwrap();
        fs::write(dir.join("leveldb/sub/000003.log"), [0u8, 1, 2, 255]).unwrap();
        let art = Artifact::dir("ls", dir.clone(), false);
        assert!(art.exists_live(&ctx));

        let a = art.read_live(&ctx).unwrap().unwrap();
        assert_eq!(a, art.read_live(&ctx).unwrap().unwrap(), "packing is deterministic");

        // Change the tree, then put the first version back.
        fs::remove_dir_all(dir.join("leveldb/sub")).unwrap();
        fs::write(dir.join("leveldb/CURRENT"), "MANIFEST-9\n").unwrap();
        fs::write(dir.join("extra.txt"), "b").unwrap();
        art.write_live(&ctx, Some(&a)).unwrap();
        assert_eq!(fs::read(dir.join("leveldb/sub/000003.log")).unwrap(), [0, 1, 2, 255]);
        assert_eq!(fs::read_to_string(dir.join("leveldb/CURRENT")).unwrap(), "MANIFEST-1\n");
        assert!(dir.join("empty").is_dir());
        assert!(!dir.join("extra.txt").exists());
        assert_eq!(art.read_live(&ctx).unwrap().unwrap(), a);
        let listed = packed_files(&a).unwrap();
        let names: Vec<&str> = listed.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["leveldb/CURRENT", "leveldb/sub/000003.log"]);
        assert_eq!(listed[1].1, [0, 1, 2, 255]);
        assert!(packed_files(b"not a snapshot").is_none());
        let leftovers = fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("aiom-"))
            .count();
        assert_eq!(leftovers, 0);

        art.write_live(&ctx, None).unwrap();
        assert!(!dir.exists());
        assert!(art.read_live(&ctx).unwrap().is_none());
    }

    #[test]
    fn unpack_rejects_bad_snapshots() {
        let tmp = tempfile::tempdir().unwrap();
        let mut evil = DIR_MAGIC.to_vec();
        push_entry(&mut evil, b'f', "../escape", Some(b"x"));
        assert!(unpack_dir(&evil, &tmp.path().join("a")).is_err());
        assert!(!tmp.path().join("escape").exists());

        let mut absolute = DIR_MAGIC.to_vec();
        push_entry(&mut absolute, b'f', "/etc/x", Some(b"x"));
        assert!(unpack_dir(&absolute, &tmp.path().join("b")).is_err());

        let mut truncated = DIR_MAGIC.to_vec();
        push_entry(&mut truncated, b'f', "file", Some(b"hello"));
        truncated.truncate(truncated.len() - 2);
        assert!(unpack_dir(&truncated, &tmp.path().join("c")).is_err());

        // A failed replace leaves the live directory as it was.
        let ctx = Ctx::for_test(tmp.path(), false);
        let live = tmp.path().join("live");
        fs::create_dir_all(&live).unwrap();
        fs::write(live.join("keep"), "1").unwrap();
        let art = Artifact::dir("d", live.clone(), false);
        assert!(art.write_live(&ctx, Some(&truncated)).is_err());
        assert_eq!(fs::read_to_string(live.join("keep")).unwrap(), "1");
    }

    #[test]
    fn replacing_an_sqlite_file_drops_its_old_journal() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = Ctx::for_test(tmp.path(), false);
        let db = tmp.path().join("Cookies");
        fs::write(&db, "db-a").unwrap();
        let art = Artifact::file("Cookies", db.clone(), false);
        let a = art.read_live(&ctx).unwrap().unwrap();

        fs::write(&db, "db-b").unwrap();
        fs::write(tmp.path().join("Cookies-journal"), "hot journal of b").unwrap();
        fs::write(tmp.path().join("Cookies-wal"), "wal of b").unwrap();
        art.write_live(&ctx, Some(&a)).unwrap();
        assert_eq!(fs::read_to_string(&db).unwrap(), "db-a");
        assert!(!tmp.path().join("Cookies-journal").exists());
        assert!(!tmp.path().join("Cookies-wal").exists());
    }

    #[test]
    fn keychain_artifact_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::for_test(dir.path(), true);
        let art = Artifact::keychain("kc", "Some Service", "tester", true);
        assert!(!art.exists_live(&ctx));
        art.write_live(&ctx, Some(b"{\"token\":1}")).unwrap();
        assert!(art.exists_live(&ctx));
        assert_eq!(art.read_live(&ctx).unwrap().unwrap(), b"{\"token\":1}");
        art.write_live(&ctx, None).unwrap();
        assert!(art.read_live(&ctx).unwrap().is_none());
    }
}
