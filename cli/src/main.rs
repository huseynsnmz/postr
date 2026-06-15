use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use postr::api::ApiClient;
use postr::config::{keyring as kr, Config};

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
    /// Create a mailbox on the worker (one-time bootstrap per address).
    /// Becomes the default mailbox if none is set yet.
    AddMailbox {
        /// Email address to receive mail at, e.g. me@yourdomain.com
        address: String,
    },
    /// Launch the TUI (default)
    Tui,
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
        Cmd::AddMailbox { address } => add_mailbox(address).await,
        Cmd::Tui => tui_entry().await,
    }
}

async fn tui_entry() -> Result<()> {
    let Some(cfg) = Config::load()? else {
        eprintln!("Not logged in. Run `postr login <url>` first.");
        std::process::exit(1);
    };
    let Some(token) = kr::load_token()? else {
        eprintln!("Not logged in. Run `postr login <url>` first.");
        std::process::exit(1);
    };
    if cfg.default_mailbox_id.is_none() {
        return Err(anyhow!(
            "no default mailbox in config — run `postr login` again"
        ));
    }
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

    kr::save_token(&token)?;
    let cfg = Config {
        worker_base_url: url.clone(),
        email: Some(me.email.clone()),
        default_mailbox_id: me.mailboxes.first().map(|m| m.id.clone()),
    };
    cfg.save()?;

    println!("Logged in to {}", url);
    println!("  email: {}", me.email);
    println!("  mailboxes: {}", me.mailboxes.len());
    for m in &me.mailboxes {
        println!("    {} ({})", m.address, m.id);
    }
    Ok(())
}

fn logout() -> Result<()> {
    kr::delete_token()?;
    Config::clear()?;
    println!("Logged out.");
    Ok(())
}

async fn add_mailbox(address: String) -> Result<()> {
    let Some(mut cfg) = Config::load()? else {
        return Err(anyhow!("not logged in — run `postr login <url>` first"));
    };
    let Some(token) = kr::load_token()? else {
        return Err(anyhow!("not logged in — run `postr login <url>` first"));
    };
    let client = ApiClient::new(&cfg.worker_base_url, &token)?;
    let mb = client.create_mailbox(&address).await?;
    println!("Mailbox: {} ({})", mb.address, mb.id);
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

async fn whoami() -> Result<()> {
    let Some(cfg) = Config::load()? else {
        return Err(anyhow!("not logged in — run `postr login <url>`"));
    };
    let Some(token) = kr::load_token()? else {
        return Err(anyhow!(
            "not logged in — token missing from keyring; run `postr login <url>`"
        ));
    };
    let client = ApiClient::new(&cfg.worker_base_url, &token)?;
    let me = client.me().await?;
    println!("URL:       {}", cfg.worker_base_url);
    println!("Email:     {}", me.email);
    println!("Mailboxes:");
    for m in &me.mailboxes {
        println!("  {} ({})", m.address, m.id);
    }
    Ok(())
}
