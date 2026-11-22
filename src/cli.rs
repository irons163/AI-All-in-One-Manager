//! Command-line interface.

use crate::ctx::Ctx;
use crate::ops::{self, SwitchReport};
use crate::provider::{self, Provider};
use crate::store::{self, Meta};
use crate::util::fmt_time;
use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use std::io::{self, BufRead, IsTerminal, Write};

#[derive(Parser)]
#[command(
    name = "aiom",
    version,
    about = "All-in-One account manager for AI coding CLIs (Claude, Codex, Gemini, Grok, OpenCode, ...)",
    after_help = "Typical use:\n  aiom add codex work        save the current Codex login as 'work'\n  aiom login codex personal  log in to a second account and save it\n  aiom use codex work        switch back\n  aiom status                see every provider at a glance"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Cmd>,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Show every provider: installed CLI, live login, active account
    Status,
    /// List providers and the credential locations aiom manages for each
    Providers,
    /// List saved accounts
    #[command(visible_alias = "ls")]
    List {
        /// Only this provider
        provider: Option<String>,
    },
    /// Save the provider's current login as a named account
    #[command(visible_alias = "save")]
    Add {
        provider: String,
        /// Name for the account, e.g. work or me@example.com
        name: String,
        /// Overwrite an existing account of the same name
        #[arg(short, long)]
        force: bool,
        /// Free-text note shown in listings
        #[arg(long)]
        note: Option<String>,
    },
    /// Save every live login that is not saved yet, named after its identity
    #[command(visible_alias = "import")]
    Adopt {
        /// Only this provider
        provider: Option<String>,
    },
    /// Switch a provider to a saved account
    #[command(visible_alias = "switch")]
    Use {
        provider: String,
        /// Account to switch to (asks when omitted)
        name: Option<String>,
        /// Re-apply the saved login even if the account is already active
        #[arg(short, long)]
        force: bool,
    },
    /// Switch to the next saved account, wrapping around
    Next { provider: String },
    /// Print the active account
    Current {
        /// Only this provider
        provider: Option<String>,
    },
    /// Refresh the active account's saved copy from the live login
    Sync {
        /// Only this provider (default: every provider with an active account)
        provider: Option<String>,
        /// Overwrite the active account even if the live login is a different user
        #[arg(short, long)]
        force: bool,
    },
    /// Log in to another account through the provider's own CLI, then save it
    Login {
        provider: String,
        /// Name for the new account
        name: String,
        /// Replace an existing account of the same name
        #[arg(short, long)]
        force: bool,
    },
    /// Delete a saved account (the live login is not touched)
    #[command(visible_alias = "rm")]
    Remove {
        provider: String,
        name: String,
        /// Do not ask for confirmation
        #[arg(short, long)]
        yes: bool,
    },
    /// Rename a saved account
    Rename {
        provider: String,
        old: String,
        new: String,
    },
    /// Put back the login that was live before the last switch
    Restore { provider: String },
    /// Print where aiom keeps its data
    Path,
}

struct Paint {
    on: bool,
}

impl Paint {
    fn new() -> Self {
        Paint {
            on: io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        }
    }
    fn wrap(&self, code: &str, text: &str) -> String {
        if self.on {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }
    fn bold(&self, t: &str) -> String {
        self.wrap("1", t)
    }
    fn dim(&self, t: &str) -> String {
        self.wrap("2", t)
    }
    fn green(&self, t: &str) -> String {
        self.wrap("32", t)
    }
    fn yellow(&self, t: &str) -> String {
        self.wrap("33", t)
    }
}

/// One table cell: plain text (used for width) and an optional colour.
type Cell = (String, Option<&'static str>);

fn cell(text: impl Into<String>) -> Cell {
    (text.into(), None)
}

fn print_table(paint: &Paint, headers: &[&str], rows: &[Vec<Cell>]) {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, (text, _)) in row.iter().enumerate() {
            widths[i] = widths[i].max(text.chars().count());
        }
    }
    let pad = |text: &str, width: usize| {
        let len = text.chars().count();
        format!("{text}{}", " ".repeat(width.saturating_sub(len)))
    };
    let header: Vec<String> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| paint.bold(&pad(h, widths[i])))
        .collect();
    println!("{}", header.join("  ").trim_end());
    for row in rows {
        let line: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, (text, colour))| {
                let padded = pad(text, widths[i]);
                match colour {
                    Some(code) => paint.wrap(code, &padded),
                    None => padded,
                }
            })
            .collect();
        println!("{}", line.join("  ").trim_end());
    }
}

fn selected<'a>(providers: &'a [Provider], query: &Option<String>) -> Result<Vec<&'a Provider>> {
    match query {
        Some(q) => Ok(vec![provider::find(providers, q)?]),
        None => Ok(providers.iter().collect()),
    }
}

fn cmd_status(ctx: &Ctx, providers: &[Provider], paint: &Paint) -> Result<()> {
    let mut rows = Vec::new();
    for p in providers {
        let st = ops::status(ctx, p)?;
        let cli = match (&st.binary, &p.binary) {
            (Some(_), _) => ("installed".to_string(), None),
            (None, Some(_)) => ("not found".to_string(), Some("2")),
            (None, None) => ("-".to_string(), Some("2")),
        };
        let login = if st.logged_in {
            cell(st.live_identity.clone().unwrap_or_else(|| "logged in".into()))
        } else {
            ("not logged in".to_string(), Some("2"))
        };
        let active = match &st.active {
            Some(meta) if st.matches_active => (meta.name.clone(), Some("32")),
            Some(meta) if st.logged_in => (format!("{} (live login differs)", meta.name), Some("33")),
            Some(meta) => (format!("{} (logged out)", meta.name), Some("33")),
            None if st.logged_in => ("(not saved)".to_string(), Some("33")),
            None => ("-".to_string(), Some("2")),
        };
        rows.push(vec![
            cell(p.id.clone()),
            cli,
            login,
            active,
            cell(st.accounts.len().to_string()),
        ]);
    }
    print_table(
        paint,
        &["PROVIDER", "CLI", "LIVE LOGIN", "ACTIVE ACCOUNT", "SAVED"],
        &rows,
    );
    Ok(())
}

fn cmd_providers(ctx: &Ctx, providers: &[Provider], paint: &Paint) {
    for p in providers {
        let origin = if p.builtin { "" } else { " (custom)" };
        println!("{}  {}{}", paint.bold(&p.id), p.name, paint.dim(origin));
        for art in &p.artifacts {
            let mark = if art.exists_live(ctx) { "present" } else { "absent" };
            let role = if art.primary { "credential" } else { "extra" };
            println!("    {:<10} {:<8} {}", role, mark, ctx.tilde(&art.describe()));
        }
        if let Some(cmd) = &p.login {
            println!("    {:<10} {}", "login", cmd.join(" "));
        }
    }
    println!(
        "\n{}",
        paint.dim(&format!(
            "Add or override providers in {}",
            ctx.tilde(&provider::custom_file(ctx).display().to_string())
        ))
    );
}

fn account_rows(metas: &[Meta], active: Option<&str>) -> Vec<Vec<Cell>> {
    metas
        .iter()
        .map(|m| {
            let is_active = active == Some(m.name.as_str());
            vec![
                ((if is_active { "*" } else { " " }).to_string(), Some("32")),
                (m.name.clone(), is_active.then_some("32")),
                cell(m.identity.clone().unwrap_or_else(|| "-".into())),
                cell(fmt_time(m.updated_at)),
                cell(m.note.clone().unwrap_or_default()),
            ]
        })
        .collect()
}

fn cmd_list(ctx: &Ctx, providers: &[&Provider], paint: &Paint, explicit: bool) -> Result<()> {
    let mut shown = false;
    for p in providers {
        let metas = store::list(ctx, &p.id)?;
        if metas.is_empty() && !explicit {
            continue;
        }
        if shown {
            println!();
        }
        shown = true;
        println!("{} {}", paint.bold(&p.id), paint.dim(&format!("({})", p.name)));
        if metas.is_empty() {
            println!("  no saved accounts; save the current login with `aiom add {} <name>`", p.id);
            continue;
        }
        let active = store::active(ctx, &p.id)?;
        print_table(
            paint,
            &["", "ACCOUNT", "IDENTITY", "SAVED", "NOTE"],
            &account_rows(&metas, active.as_deref()),
        );
    }
    if !shown {
        println!("No saved accounts yet. Log in with a CLI as usual, then run `aiom add <provider> <name>`.");
    }
    Ok(())
}

fn print_switch(ctx: &Ctx, p: &Provider, report: &SwitchReport, paint: &Paint) {
    for warning in &report.warnings {
        eprintln!("{} {}", paint.yellow("warning:"), ctx.tilde(warning));
    }
    let identity = report
        .identity
        .as_ref()
        .map(|i| format!(" ({i})"))
        .unwrap_or_default();
    if report.already_active {
        println!("{} is already on '{}'{identity}", p.name, report.to);
        return;
    }
    println!(
        "{} {} is now on '{}'{identity}",
        paint.green("ok:"),
        p.name,
        report.to
    );
    if report.synced {
        if let Some(from) = &report.from {
            println!("    saved the latest tokens of '{from}' first");
        }
    }
    match &report.restarted_app {
        Some(app) => println!("    {app} was closed for the switch and has been started again"),
        None if p.app.is_some() => {}
        None => println!("    restart any running {} session to pick up the change", p.name),
    }
}

/// `aiom login` for a provider whose login happens inside its desktop app:
/// sign the app out, wait for the user to sign in there, then save.
fn login_in_app(ctx: &Ctx, p: &Provider, name: &str, paint: &Paint) -> Result<()> {
    let app = p.app.as_ref().expect("checked by caller");
    println!(
        "Closing {} to sign it out; the current login is backed up first.",
        app.name
    );
    for warning in ops::app_logout(ctx, p, Some(name))? {
        eprintln!("{} {}", paint.yellow("warning:"), ctx.tilde(&warning));
    }
    println!(
        "{} is open again. Sign in to the other account there; this waits up to 15 minutes (Ctrl+C to stop).",
        app.name
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15 * 60);
    while !p.logged_in(ctx) {
        if std::time::Instant::now() >= deadline {
            bail!(
                "no sign-in yet; after signing in to {}, run `aiom adopt {}` to save it as '{name}'",
                app.name,
                p.id
            );
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    // Give the app a moment to finish writing everything that belongs to the login.
    std::thread::sleep(std::time::Duration::from_secs(5));
    match ops::adopt(ctx, p)? {
        Some(adopted) => println!(
            "{} signed in and saved {} account '{}'",
            paint.green("ok:"),
            p.name,
            adopted.name
        ),
        None => println!("{} the new login is already a saved account", paint.green("ok:")),
    }
    Ok(())
}

fn pick_account(ctx: &Ctx, p: &Provider, paint: &Paint) -> Result<String> {
    let metas = store::list(ctx, &p.id)?;
    if metas.is_empty() {
        bail!(
            "{} has no saved accounts; save the current login with `aiom add {} <name>`",
            p.name,
            p.id
        );
    }
    if !io::stdin().is_terminal() {
        bail!("no account name given (run `aiom list {}` to see them)", p.id);
    }
    let active = store::active(ctx, &p.id)?;
    for (i, m) in metas.iter().enumerate() {
        let mark = if active.as_deref() == Some(m.name.as_str()) {
            paint.green("*")
        } else {
            " ".to_string()
        };
        println!(
            "  {mark} {}) {}  {}",
            i + 1,
            m.name,
            paint.dim(m.identity.as_deref().unwrap_or(""))
        );
    }
    print!("Switch {} to (number or name): ", p.name);
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    let answer = line.trim();
    if answer.is_empty() {
        bail!("cancelled");
    }
    if let Ok(n) = answer.parse::<usize>() {
        if (1..=metas.len()).contains(&n) {
            return Ok(metas[n - 1].name.clone());
        }
    }
    match metas.iter().find(|m| m.name == answer) {
        Some(m) => Ok(m.name.clone()),
        None => bail!("no account '{answer}'"),
    }
}

fn confirm(question: &str) -> Result<bool> {
    if !io::stdin().is_terminal() {
        bail!("refusing to continue without confirmation (pass --yes)");
    }
    print!("{question} [y/N] ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

pub fn run(cli: Cli, ctx: &Ctx) -> Result<()> {
    let paint = Paint::new();
    let providers = provider::load_all(ctx)?;
    let get = |query: &str| provider::find(&providers, query);

    match cli.command.unwrap_or(Cmd::Status) {
        Cmd::Status => cmd_status(ctx, &providers, &paint)?,
        Cmd::Providers => cmd_providers(ctx, &providers, &paint),
        Cmd::List { provider } => {
            cmd_list(ctx, &selected(&providers, &provider)?, &paint, provider.is_some())?
        }
        Cmd::Add {
            provider,
            name,
            force,
            note,
        } => {
            let p = get(&provider)?;
            let meta = ops::save(ctx, p, &name, force, note)?;
            let identity = meta
                .identity
                .map(|i| format!(" ({i})"))
                .unwrap_or_default();
            println!(
                "{} saved the current {} login as '{}'{identity}",
                paint.green("ok:"),
                p.name,
                meta.name
            );
        }
        Cmd::Adopt { provider } => {
            let mut any = false;
            for p in selected(&providers, &provider)? {
                if let Some(adopted) = ops::adopt(ctx, p)? {
                    any = true;
                    let identity = adopted
                        .identity
                        .map(|i| format!(" ({i})"))
                        .unwrap_or_default();
                    let what = if adopted.created {
                        "saved the live login as"
                    } else {
                        "the live login is the saved account"
                    };
                    println!(
                        "{} {}: {what} '{}'{identity}",
                        paint.green("ok:"),
                        p.name,
                        adopted.name
                    );
                }
            }
            if !any {
                println!("Nothing to save: every live login is already an account.");
            }
        }
        Cmd::Use {
            provider,
            name,
            force,
        } => {
            let p = get(&provider)?;
            let name = match name {
                Some(name) => name,
                None => pick_account(ctx, p, &paint)?,
            };
            let report = ops::switch(ctx, p, &name, force)?;
            print_switch(ctx, p, &report, &paint);
        }
        Cmd::Next { provider } => {
            let p = get(&provider)?;
            let name = ops::next_account(ctx, p)?;
            let report = ops::switch(ctx, p, &name, false)?;
            print_switch(ctx, p, &report, &paint);
        }
        Cmd::Current { provider } => {
            let explicit = provider.is_some();
            for p in selected(&providers, &provider)? {
                match store::active(ctx, &p.id)? {
                    Some(name) if explicit => println!("{name}"),
                    Some(name) => println!("{}: {name}", p.id),
                    None if explicit => bail!("{} has no active account", p.name),
                    None => {}
                }
            }
        }
        Cmd::Sync { provider, force } => {
            let explicit = provider.is_some();
            for p in selected(&providers, &provider)? {
                if !explicit && store::active(ctx, &p.id)?.is_none() {
                    continue;
                }
                match ops::sync(ctx, p, force) {
                    Ok(meta) => println!(
                        "{} refreshed {} account '{}'",
                        paint.green("ok:"),
                        p.name,
                        meta.name
                    ),
                    Err(err) if explicit => return Err(err),
                    Err(err) => eprintln!("{} {}: {err:#}", paint.yellow("skipped"), p.id),
                }
            }
        }
        Cmd::Login {
            provider,
            name,
            force,
        } => {
            let p = get(&provider)?;
            if p.app.is_some() && p.login.is_none() {
                if force {
                    bail!("--force is not supported for {}; rename or remove the existing account first", p.name);
                }
                return login_in_app(ctx, p, &name, &paint);
            }
            let (meta, warnings) = ops::login(ctx, p, &name, force)?;
            for warning in &warnings {
                eprintln!("{} {}", paint.yellow("warning:"), ctx.tilde(warning));
            }
            let identity = meta
                .identity
                .map(|i| format!(" ({i})"))
                .unwrap_or_default();
            println!(
                "{} logged in and saved {} account '{}'{identity}",
                paint.green("ok:"),
                p.name,
                meta.name
            );
        }
        Cmd::Remove {
            provider,
            name,
            yes,
        } => {
            let p = get(&provider)?;
            if !yes && !confirm(&format!("Delete {} account '{name}'?", p.name))? {
                bail!("cancelled");
            }
            ops::remove(ctx, p, &name)?;
            println!("{} removed {} account '{name}'", paint.green("ok:"), p.name);
        }
        Cmd::Rename { provider, old, new } => {
            let p = get(&provider)?;
            ops::rename(ctx, p, &old, &new)?;
            println!(
                "{} renamed {} account '{old}' to '{new}'",
                paint.green("ok:"),
                p.name
            );
        }
        Cmd::Restore { provider } => {
            let p = get(&provider)?;
            let (dir, identity) = ops::restore(ctx, p)?;
            let identity = identity.map(|i| format!(" ({i})")).unwrap_or_default();
            println!(
                "{} restored the {} login from {}{identity}",
                paint.green("ok:"),
                p.name,
                ctx.tilde(&dir.display().to_string())
            );
        }
        Cmd::Path => println!("{}", ctx.data_dir.display()),
    }
    Ok(())
}
