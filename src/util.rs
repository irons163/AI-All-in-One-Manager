//! Small dependency-free helpers: atomic writes, time formatting, JSON paths.

use anyhow::{Context, Result};
use serde_json::{Map, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `2026-11-14 22:30 UTC`
pub fn fmt_time(secs: u64) -> String {
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        rem / 3600,
        (rem % 3600) / 60
    )
}

/// `20261114-223005`, sortable and safe as a directory name.
pub fn stamp(secs: u64) -> String {
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = yoe as i64 + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

#[cfg(unix)]
pub fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting permissions on {}", path.display()))
}

#[cfg(not(unix))]
pub fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

/// Create a directory (and parents) that only the owner can enter.
pub fn ensure_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    set_mode(path, 0o700)
}

/// Write `data` to `path` via a temp file and rename, so readers never see a
/// half-written credential file. Keeps the existing file's permissions, and
/// defaults to 0600 for new files. Symlinks are followed rather than replaced.
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let target: PathBuf = if path.is_symlink() {
        fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    } else {
        path.to_path_buf()
    };
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let existing_perms = fs::metadata(&target).ok().map(|m| m.permissions());
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let tmp = target.with_file_name(format!(".{name}.aiom-tmp-{}", std::process::id()));

    let write = || -> Result<()> {
        let mut file = fs::File::create(&tmp)?;
        match &existing_perms {
            Some(perms) => fs::set_permissions(&tmp, perms.clone())?,
            None => set_mode(&tmp, 0o600)?,
        }
        file.write_all(data)?;
        file.sync_all().ok();
        Ok(())
    };
    if let Err(err) = write() {
        let _ = fs::remove_file(&tmp);
        return Err(err).with_context(|| format!("writing {}", target.display()));
    }
    fs::rename(&tmp, &target).with_context(|| format!("replacing {}", target.display()))
}

pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Decode base64url (also accepts standard base64), with or without padding.
pub fn b64url_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for c in input.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' => break,
            _ => return None,
        };
        buf = (buf << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Read one claim out of a JWT's payload without verifying the signature.
/// Only used to show which account a token belongs to.
pub fn jwt_claim(token: &str, claim: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let json: Value = serde_json::from_slice(&b64url_decode(payload)?).ok()?;
    json_get(&json, claim)?.as_str().map(str::to_string)
}

fn split_last(path: &str) -> (Option<&str>, &str) {
    match path.rsplit_once('.') {
        Some((parents, last)) => (Some(parents), last),
        None => (None, path),
    }
}

/// Look up a dotted path such as `security.auth` in a JSON value.
pub fn json_get<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = root;
    for seg in path.split('.') {
        cur = cur.as_object()?.get(seg)?;
    }
    Some(cur)
}

/// Set a dotted path, creating intermediate objects as needed.
pub fn json_set(root: &mut Value, path: &str, new: Value) {
    let (parents, last) = split_last(path);
    let mut cur = root;
    if let Some(parents) = parents {
        for seg in parents.split('.') {
            if !cur.is_object() {
                *cur = Value::Object(Map::new());
            }
            cur = cur
                .as_object_mut()
                .expect("just ensured object")
                .entry(seg.to_string())
                .or_insert_with(|| Value::Object(Map::new()));
        }
    }
    if !cur.is_object() {
        *cur = Value::Object(Map::new());
    }
    cur.as_object_mut()
        .expect("just ensured object")
        .insert(last.to_string(), new);
}

/// Remove a dotted path. Returns whether something was removed. Keeps the
/// order of the remaining keys.
pub fn json_remove(root: &mut Value, path: &str) -> bool {
    let (parents, last) = split_last(path);
    let mut cur = root;
    if let Some(parents) = parents {
        for seg in parents.split('.') {
            match cur.as_object_mut().and_then(|m| m.get_mut(seg)) {
                Some(next) => cur = next,
                None => return false,
            }
        }
    }
    match cur.as_object_mut() {
        Some(map) => map.shift_remove(last).is_some(),
        None => false,
    }
}

/// Find an executable on a `PATH`-style list.
pub fn find_in_path(binary: &str, path_var: Option<&str>) -> Option<PathBuf> {
    if binary.contains('/') {
        let p = PathBuf::from(binary);
        return p.is_file().then_some(p);
    }
    let path_var = path_var?;
    std::env::split_paths(path_var)
        .map(|dir| dir.join(binary))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn formats_known_dates() {
        assert_eq!(fmt_time(0), "1970-01-01 00:00 UTC");
        // 2026-11-14 22:30:05 UTC
        assert_eq!(fmt_time(1_794_695_405), "2026-11-14 22:30 UTC");
        assert_eq!(stamp(1_794_695_405), "20261114-223005");
        // leap day
        assert_eq!(fmt_time(1_709_164_800), "2024-02-29 00:00 UTC");
    }

    #[test]
    fn decodes_jwt_claims() {
        let token = "eyJhbGciOiJub25lIn0.eyJlbWFpbCI6ImFAYi5jIn0.sig";
        assert_eq!(jwt_claim(token, "email").as_deref(), Some("a@b.c"));
        assert_eq!(jwt_claim(token, "missing"), None);
        assert_eq!(jwt_claim("not-a-jwt", "email"), None);
    }

    #[test]
    fn json_paths_round_trip() {
        let mut v = json!({"a": 1, "b": {"c": 2, "d": 3}, "e": 4});
        assert_eq!(json_get(&v, "b.c"), Some(&json!(2)));
        assert_eq!(json_get(&v, "b.x"), None);

        json_set(&mut v, "b.c", json!("new"));
        json_set(&mut v, "x.y.z", json!(true));
        assert_eq!(json_get(&v, "b.c"), Some(&json!("new")));
        assert_eq!(json_get(&v, "x.y.z"), Some(&json!(true)));

        assert!(json_remove(&mut v, "a"));
        assert!(!json_remove(&mut v, "a"));
        assert!(!json_remove(&mut v, "nope.deeper"));
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["b", "e", "x"], "removal keeps key order");
    }

    #[test]
    fn atomic_write_creates_private_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/secret.json");
        atomic_write(&path, b"one").unwrap();
        atomic_write(&path, b"two").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("aiom-tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }
}
