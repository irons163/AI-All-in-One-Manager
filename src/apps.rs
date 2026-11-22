//! Quitting and starting desktop apps whose login aiom swaps. A running app
//! keeps its login in memory and writes it back on exit, so its files can
//! only be replaced while it is closed.

use anyhow::{bail, Context, Result};
use std::cell::{Cell, RefCell};
use std::process::{Command, Stdio};

pub trait AppRunner {
    fn is_running(&self, bundle_id: &str) -> Result<bool>;
    /// Ask the app to quit the normal way (like Cmd+Q). Returns at once;
    /// callers wait for `is_running` to turn false.
    fn quit(&self, bundle_id: &str) -> Result<()>;
    fn launch(&self, bundle_id: &str) -> Result<()>;
    /// What happened, for tests.
    fn log(&self) -> Vec<String> {
        Vec::new()
    }
}

/// macOS: AppleScript for "is it running" and "quit", `open` to start.
pub struct MacApps;

fn osascript(lines: &[&str]) -> Result<String> {
    let mut cmd = Command::new("osascript");
    for line in lines {
        cmd.arg("-e").arg(line);
    }
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .context("running osascript")?;
    if !out.status.success() {
        bail!(
            "osascript failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn applescript_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

impl AppRunner for MacApps {
    fn is_running(&self, bundle_id: &str) -> Result<bool> {
        // `is running` never launches the app, unlike most other commands.
        let answer = osascript(&[&format!(
            "application id {} is running",
            applescript_string(bundle_id)
        )])?;
        Ok(answer == "true")
    }

    fn quit(&self, bundle_id: &str) -> Result<()> {
        // Do not wait for the reply: the app may first ask the user to
        // confirm, and that must not block aiom.
        osascript(&[
            "ignoring application responses",
            &format!("tell application id {} to quit", applescript_string(bundle_id)),
            "end ignoring",
        ])
        .map(|_| ())
    }

    fn launch(&self, bundle_id: &str) -> Result<()> {
        let status = Command::new("open")
            .args(["-b", bundle_id])
            .status()
            .context("running open")?;
        if !status.success() {
            bail!("could not start {bundle_id}");
        }
        Ok(())
    }
}

/// Used where aiom must not control apps (other systems, `AIOM_NO_APP_CONTROL`).
/// Reports every app as not running and does nothing.
pub struct NoApps;

impl AppRunner for NoApps {
    fn is_running(&self, _bundle_id: &str) -> Result<bool> {
        Ok(false)
    }
    fn quit(&self, _bundle_id: &str) -> Result<()> {
        Ok(())
    }
    fn launch(&self, _bundle_id: &str) -> Result<()> {
        Ok(())
    }
}

/// In-memory stand-in for tests.
#[derive(Default)]
pub struct FakeApps {
    pub running: Cell<bool>,
    /// Simulate an app that ignores the quit request.
    pub refuse_quit: Cell<bool>,
    events: RefCell<Vec<String>>,
}

impl AppRunner for FakeApps {
    fn is_running(&self, _bundle_id: &str) -> Result<bool> {
        Ok(self.running.get())
    }
    fn quit(&self, bundle_id: &str) -> Result<()> {
        self.events.borrow_mut().push(format!("quit {bundle_id}"));
        if !self.refuse_quit.get() {
            self.running.set(false);
        }
        Ok(())
    }
    fn launch(&self, bundle_id: &str) -> Result<()> {
        self.events.borrow_mut().push(format!("launch {bundle_id}"));
        self.running.set(true);
        Ok(())
    }
    fn log(&self) -> Vec<String> {
        self.events.borrow().clone()
    }
}
