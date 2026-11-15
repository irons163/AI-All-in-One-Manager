//! OS secret storage. On macOS this is the login keychain, driven through
//! the system `security` tool (the same way Claude Code itself reads and
//! writes its credentials), so no native bindings are needed.

use crate::util::hex_encode;
use anyhow::{bail, Context, Result};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};

pub trait SecretStore {
    /// Whether the item exists. Must not require access to the secret itself.
    fn exists(&self, service: &str, account: &str) -> Result<bool>;
    fn get(&self, service: &str, account: &str) -> Result<Option<String>>;
    fn set(&self, service: &str, account: &str, secret: &str) -> Result<()>;
    /// Returns whether an item was deleted.
    fn delete(&self, service: &str, account: &str) -> Result<bool>;
}

/// `security` exits with 44 when the item is not in the keychain.
const NOT_FOUND: i32 = 44;
/// `security -i` reads commands from stdin line by line with a bounded buffer.
const MAX_STDIN_LINE: usize = 4000;

pub struct MacKeychain;

impl MacKeychain {
    fn quote(s: &str) -> String {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }

    /// Preferred path: the secret travels over stdin, so it never shows up in
    /// the process list.
    fn set_via_stdin(service: &str, account: &str, hex: &str) -> Result<()> {
        let line = format!(
            "add-generic-password -U -a {} -s {} -X {}\n",
            Self::quote(account),
            Self::quote(service),
            Self::quote(hex)
        );
        if line.len() > MAX_STDIN_LINE {
            bail!("secret too long for security -i");
        }
        let mut child = Command::new("security")
            .arg("-i")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("running `security -i`")?;
        child
            .stdin
            .take()
            .context("opening stdin of `security`")?
            .write_all(line.as_bytes())?;
        let status = child.wait()?;
        if !status.success() {
            bail!("`security -i` exited with {status}");
        }
        Ok(())
    }

    fn set_via_args(service: &str, account: &str, hex: &str) -> Result<()> {
        let out = Command::new("security")
            .args(["add-generic-password", "-U", "-a", account, "-s", service, "-X", hex])
            .output()
            .context("running `security add-generic-password`")?;
        if !out.status.success() {
            bail!(
                "keychain write failed for '{service}': {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }
}

impl SecretStore for MacKeychain {
    fn exists(&self, service: &str, account: &str) -> Result<bool> {
        let status = Command::new("security")
            .args(["find-generic-password", "-s", service, "-a", account])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("running `security find-generic-password`")?;
        Ok(status.success())
    }

    fn get(&self, service: &str, account: &str) -> Result<Option<String>> {
        let out = Command::new("security")
            .args(["find-generic-password", "-s", service, "-a", account, "-w"])
            .output()
            .context("running `security find-generic-password`")?;
        if out.status.success() {
            let mut secret = String::from_utf8_lossy(&out.stdout).into_owned();
            if secret.ends_with('\n') {
                secret.pop();
            }
            return Ok(Some(secret));
        }
        if out.status.code() == Some(NOT_FOUND) {
            return Ok(None);
        }
        bail!(
            "keychain read failed for '{service}': {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )
    }

    fn set(&self, service: &str, account: &str, secret: &str) -> Result<()> {
        let hex = hex_encode(secret.as_bytes());
        // `security -i` does not reliably report failures through its exit
        // status, so confirm the write and fall back to argv if it did not land.
        if Self::set_via_stdin(service, account, &hex).is_ok()
            && self.get(service, account)?.as_deref() == Some(secret)
        {
            return Ok(());
        }
        Self::set_via_args(service, account, &hex)
    }

    fn delete(&self, service: &str, account: &str) -> Result<bool> {
        let out = Command::new("security")
            .args(["delete-generic-password", "-s", service, "-a", account])
            .output()
            .context("running `security delete-generic-password`")?;
        if out.status.success() {
            return Ok(true);
        }
        if out.status.code() == Some(NOT_FOUND) {
            return Ok(false);
        }
        bail!(
            "keychain delete failed for '{service}': {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )
    }
}

/// Used where there is no supported OS secret store. Nothing is ever present,
/// and writes fail loudly rather than silently dropping a credential.
pub struct NoStore;

impl SecretStore for NoStore {
    fn exists(&self, _service: &str, _account: &str) -> Result<bool> {
        Ok(false)
    }
    fn get(&self, _service: &str, _account: &str) -> Result<Option<String>> {
        Ok(None)
    }
    fn set(&self, service: &str, _account: &str, _secret: &str) -> Result<()> {
        bail!("no OS keychain is available to store '{service}'")
    }
    fn delete(&self, _service: &str, _account: &str) -> Result<bool> {
        Ok(false)
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    /// Creates and deletes throwaway items in the real login keychain, so it
    /// only runs on request: `cargo test -- --ignored mac_keychain`.
    #[test]
    #[ignore = "touches the real macOS login keychain"]
    fn mac_keychain_round_trip() {
        let store = MacKeychain;
        let service = format!("aiom selftest \"{}\"", std::process::id());
        let account = "aiom-selftest";
        let _ = store.delete(&service, account);

        assert!(!store.exists(&service, account).unwrap());
        assert_eq!(store.get(&service, account).unwrap(), None);
        assert!(!store.delete(&service, account).unwrap());

        // Short secret: goes through `security -i` on stdin.
        let short = r#"{"claudeAiOauth":{"accessToken":"sk-test","scopes":["a b"]}}"#;
        store.set(&service, account, short).unwrap();
        assert!(store.exists(&service, account).unwrap());
        assert_eq!(store.get(&service, account).unwrap().as_deref(), Some(short));

        // Long secret: too big for one stdin line, falls back to argv. Also
        // checks that an existing item is updated rather than duplicated.
        let long = format!(r#"{{"token":"{}"}}"#, "x".repeat(6000));
        store.set(&service, account, &long).unwrap();
        assert_eq!(store.get(&service, account).unwrap().as_deref(), Some(long.as_str()));

        assert!(store.delete(&service, account).unwrap());
        assert!(!store.exists(&service, account).unwrap());
    }
}

/// In-memory store for tests.
#[derive(Default)]
pub struct MemoryStore {
    items: RefCell<HashMap<(String, String), String>>,
}

impl SecretStore for MemoryStore {
    fn exists(&self, service: &str, account: &str) -> Result<bool> {
        Ok(self
            .items
            .borrow()
            .contains_key(&(service.to_string(), account.to_string())))
    }
    fn get(&self, service: &str, account: &str) -> Result<Option<String>> {
        Ok(self
            .items
            .borrow()
            .get(&(service.to_string(), account.to_string()))
            .cloned())
    }
    fn set(&self, service: &str, account: &str, secret: &str) -> Result<()> {
        self.items.borrow_mut().insert(
            (service.to_string(), account.to_string()),
            secret.to_string(),
        );
        Ok(())
    }
    fn delete(&self, service: &str, account: &str) -> Result<bool> {
        Ok(self
            .items
            .borrow_mut()
            .remove(&(service.to_string(), account.to_string()))
            .is_some())
    }
}
