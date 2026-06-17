use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use postr::api::ApiClient;
use postr::config::{self, Config};

#[derive(Parser)]
#[command(name = "postr", version, about = "postr — Cloudflare-hosted email TUI")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Log in to a Cloudflare Worker
    Login {
        /// Worker base URL e.g. https://mail-worker.example.workers.dev
        url: String,
        /// Bearer token (omit to prompt securely on stdin)
        #[arg(long)]
        token: Option<String>,
    },
    /// Clear stored token + config
    Logout,
    /// Print the currently logged-in identity
    Whoami,
    /// Manage mailboxes on the worker
    #[command(subcommand)]
    Mailbox(MailboxCmd),
    /// Populate a mailbox with curated demo data (idempotent). Useful for
    /// screenshots and quick walkthroughs.
    DemoSeed {
        /// Mailbox address to seed (must already exist on the worker)
        address: String,
    },
    /// Check the DNS records for an outbound-sending domain. Validates
    /// MX, SPF, DKIM, DMARC, and MTA-STS — does not require login.
    Doctor {
        /// Mailbox address (we resolve the domain from the right side)
        address: String,
    },
    /// Launch the TUI (default)
    Tui,
}

#[derive(Subcommand)]
enum MailboxCmd {
    /// Create a mailbox on the worker (one-time bootstrap per address).
    /// Becomes the default mailbox if none is set yet.
    Add {
        /// Email address to receive mail at, e.g. me@yourdomain.com
        address: String,
        /// Personal name attached to outbound `From:` headers
        #[arg(long)]
        name: Option<String>,
        /// Short alias for `/switch <alias>` in the TUI (e.g. "work")
        #[arg(long)]
        alias: Option<String>,
    },
    /// Update mailbox metadata. Currently the display name and alias are
    /// mutable; the address itself isn't.
    Update {
        /// Email address of the mailbox to update
        address: String,
        /// New display name. Use `--clear-name` to remove it instead.
        #[arg(long, conflicts_with = "clear_name")]
        name: Option<String>,
        /// Clear the display name so outbound mail uses the bare address.
        #[arg(long)]
        clear_name: bool,
        /// New alias for `/switch` lookups. Use `--clear-alias` to remove.
        #[arg(long, conflicts_with = "clear_alias")]
        alias: Option<String>,
        /// Clear the alias.
        #[arg(long)]
        clear_alias: bool,
    },
    /// List all mailboxes known to the worker.
    List,
    /// Remove a mailbox marker (the DO's stored mail is preserved).
    Remove {
        /// Email address to remove
        address: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    match cli.cmd.unwrap_or(Cmd::Tui) {
        Cmd::Login { url, token } => login(url, token).await,
        Cmd::Logout => logout(),
        Cmd::Whoami => whoami().await,
        Cmd::Mailbox(MailboxCmd::Add {
            address,
            name,
            alias,
        }) => mailbox_add(address, name, alias).await,
        Cmd::Mailbox(MailboxCmd::Update {
            address,
            name,
            clear_name,
            alias,
            clear_alias,
        }) => mailbox_update(address, name, clear_name, alias, clear_alias).await,
        Cmd::Mailbox(MailboxCmd::List) => mailbox_list().await,
        Cmd::Mailbox(MailboxCmd::Remove { address }) => mailbox_remove(address).await,
        Cmd::DemoSeed { address } => demo_seed(address).await,
        Cmd::Doctor { address } => postr::doctor::run(&address).await,
        Cmd::Tui => tui_entry().await,
    }
}

async fn tui_entry() -> Result<()> {
    // The TUI launches even without a saved session — `/login` inside the
    // TUI walks the user through authenticating. An empty cfg + empty token
    // means the welcome screen prompts for login on first paint; once
    // `/login` completes, the App hot-swaps the client and loads the inbox.
    let cfg = Config::load()?.unwrap_or_default();
    let token = config::load_token()?.unwrap_or_default();
    let client = std::sync::Arc::new(ApiClient::new(&cfg.worker_base_url, &token)?);
    postr::tui::app::run(client, cfg).await
}

async fn login(url: String, token_arg: Option<String>) -> Result<()> {
    let url = url.trim_end_matches('/').to_string();
    let token = match token_arg {
        Some(t) => t,
        None => rpassword::prompt_password("CLI token: ")?,
    };
    if token.trim().is_empty() {
        return Err(anyhow!("empty token"));
    }

    let client = ApiClient::new(&url, &token)?;
    let me = client.me().await.context("calling /cli/me")?;

    let cfg = Config {
        worker_base_url: url.clone(),
        email: Some(me.email.clone()),
        default_mailbox_id: me.mailboxes.first().map(|m| m.id.clone()),
        token: Some(token),
    };
    cfg.save()?;

    println!("Logged in to {}", url);
    println!("  email: {}", me.email);
    println!("  mailboxes: {}", me.mailboxes.len());
    for m in &me.mailboxes {
        println!("    {}", format_name(m));
    }
    Ok(())
}

fn logout() -> Result<()> {
    config::delete_token()?;
    Config::clear()?;
    println!("Logged out.");
    Ok(())
}

fn require_session() -> Result<(Config, ApiClient)> {
    let Some(cfg) = Config::load()? else {
        return Err(anyhow!("not logged in — run `postr login <url>` first"));
    };
    let Some(token) = config::load_token()? else {
        return Err(anyhow!("not logged in — run `postr login <url>` first"));
    };
    let client = ApiClient::new(&cfg.worker_base_url, &token)?;
    Ok((cfg, client))
}

fn format_name(mb: &postr::api::types::CliMailbox) -> String {
    let alias = mb
        .alias
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|a| format!(" [{a}]"))
        .unwrap_or_default();
    match mb.display_name.as_deref().filter(|s| !s.is_empty()) {
        Some(n) => format!("{} <{}>{alias}", n, mb.address),
        None => format!("{}{alias}", mb.address),
    }
}

async fn mailbox_add(address: String, name: Option<String>, alias: Option<String>) -> Result<()> {
    let (mut cfg, client) = require_session()?;
    let mb = client
        .create_mailbox(&address, name.as_deref(), alias.as_deref())
        .await
        .context("creating mailbox")?;
    println!("Mailbox: {}", format_name(&mb));
    if cfg.default_mailbox_id.is_none() {
        cfg.default_mailbox_id = Some(mb.id.clone());
        if cfg.email.as_deref().is_none_or(str::is_empty) {
            cfg.email = Some(mb.address.clone());
        }
        cfg.save()?;
        println!("Set as default mailbox.");
    }
    Ok(())
}

async fn mailbox_update(
    address: String,
    name: Option<String>,
    clear_name: bool,
    alias: Option<String>,
    clear_alias: bool,
) -> Result<()> {
    let (_, client) = require_session()?;
    let display_payload = match (name, clear_name) {
        (None, false) => None,
        (None, true) => Some(None),
        (Some(n), _) => Some(Some(n)),
    };
    let alias_payload = match (alias, clear_alias) {
        (None, false) => None,
        (None, true) => Some(None),
        (Some(a), _) => Some(Some(a)),
    };
    if display_payload.is_none() && alias_payload.is_none() {
        return Err(anyhow!(
            "nothing to update — pass --name/--clear-name or --alias/--clear-alias"
        ));
    }
    let display_ref = display_payload.as_ref().map(|inner| inner.as_deref());
    let alias_ref = alias_payload.as_ref().map(|inner| inner.as_deref());
    let mb = client
        .update_mailbox(&address, display_ref, alias_ref)
        .await
        .context("updating mailbox")?;
    println!("Updated: {}", format_name(&mb));
    Ok(())
}

async fn mailbox_list() -> Result<()> {
    let (_, client) = require_session()?;
    let me = client.me().await.context("calling /cli/me")?;
    if me.mailboxes.is_empty() {
        println!("No mailboxes.");
        return Ok(());
    }
    for mb in &me.mailboxes {
        println!("  {}", format_name(mb));
    }
    Ok(())
}

async fn demo_seed(address: String) -> Result<()> {
    let (_, client) = require_session()?;
    let n = client
        .seed_demo(&address)
        .await
        .context("seeding demo data")?;
    println!("Seeded {n} demo emails into {address}.");
    Ok(())
}

async fn mailbox_remove(address: String) -> Result<()> {
    let (mut cfg, client) = require_session()?;
    client
        .delete_mailbox(&address)
        .await
        .context("removing mailbox")?;
    println!("Removed {}.", address);
    if cfg.default_mailbox_id.as_deref() == Some(address.as_str()) {
        cfg.default_mailbox_id = None;
        cfg.save()?;
        println!("Cleared default mailbox.");
    }
    Ok(())
}

async fn whoami() -> Result<()> {
    let Some(cfg) = Config::load()? else {
        return Err(anyhow!("not logged in — run `postr login <url>`"));
    };
    let Some(token) = config::load_token()? else {
        return Err(anyhow!(
            "not logged in — token missing from config; run `postr login <url>`"
        ));
    };
    let client = ApiClient::new(&cfg.worker_base_url, &token)?;
    let me = client.me().await?;
    println!("URL:       {}", cfg.worker_base_url);
    println!("Email:     {}", me.email);
    println!("Mailboxes:");
    for m in &me.mailboxes {
        println!("  {}", format_name(m));
    }
    Ok(())
}
