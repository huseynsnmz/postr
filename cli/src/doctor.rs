//! `postr doctor <addr>` — DNS health check for an outbound-sending domain.
//!
//! Resolves MX / SPF (TXT apex) / DKIM (cloudflare._domainkey) / DMARC
//! (_dmarc) / MTA-STS (_mta-sts) and prints a coloured checklist. Exit
//! code 0 if everything *required* passes (MX + SPF); non-zero if a
//! required record is missing or misconfigured.
//!
//! All checks are run concurrently; total wall-clock is the slowest single
//! resolver lookup (~50–200ms typical).

use anyhow::{anyhow, Result};
use hickory_resolver::proto::rr::RData;
use hickory_resolver::TokioResolver;

// ── ANSI helpers ─────────────────────────────────────────────────────────

const RESET: &str = "\x1b[0m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";

fn ok(label: &str, detail: &str, lines: &[String]) {
    println!("  {GREEN}✓{RESET}  {BOLD}{label:<10}{RESET} {detail}");
    for l in lines {
        println!("        {DIM}→ {l}{RESET}");
    }
}

fn fail(label: &str, detail: &str, lines: &[String]) {
    println!("  {RED}✗{RESET}  {BOLD}{label:<10}{RESET} {detail}");
    for l in lines {
        println!("        {DIM}→ {l}{RESET}");
    }
}

fn info(label: &str, detail: &str, lines: &[String]) {
    println!("  {DIM}ⓘ{RESET}  {BOLD}{label:<10}{RESET} {DIM}{detail}{RESET}");
    for l in lines {
        println!("        {DIM}→ {l}{RESET}");
    }
}

// ── Entry point ──────────────────────────────────────────────────────────

pub async fn run(address: &str) -> Result<()> {
    let (_local, domain) = address
        .split_once('@')
        .ok_or_else(|| anyhow!("address must contain '@'"))?;
    if domain.is_empty() {
        return Err(anyhow!("address missing domain"));
    }

    println!();
    println!("{BOLD}DNS health check for {}{RESET}", domain);
    println!("  {DIM}via system resolver — `postr doctor` runs in <1s typically{RESET}");
    println!();

    let resolver = TokioResolver::builder_tokio()
        .map_err(|e| anyhow!("system resolver init failed: {e}"))?
        .build()
        .map_err(|e| anyhow!("system resolver build failed: {e}"))?;

    let (mx, spf, dkim, dmarc, mtasts) = tokio::join!(
        check_mx(&resolver, domain),
        check_spf(&resolver, domain),
        check_dkim(&resolver, domain),
        check_dmarc(&resolver, domain),
        check_mtasts(&resolver, domain),
    );

    println!();
    let critical_ok = matches!(mx, Check::Ok) && matches!(spf, Check::Ok);
    if critical_ok {
        println!("{GREEN}✓ ready to send{RESET}");
    } else {
        println!("{RED}✗ outbound mail will likely be rejected or spam-filtered{RESET}");
    }
    println!();

    // Silence dead-code lint for the unused Info variant on the summary side;
    // it's used by the per-check helpers to convey "neither pass nor fail".
    let _ = (dkim, dmarc, mtasts);

    if !critical_ok {
        std::process::exit(1);
    }
    Ok(())
}

#[derive(Debug)]
enum Check {
    Ok,
    Fail,
    Info,
}

// ── Individual checks ────────────────────────────────────────────────────

async fn check_mx(resolver: &TokioResolver, domain: &str) -> Check {
    let mut targets: Vec<String> = match resolver.mx_lookup(domain).await {
        Ok(answer) => answer
            .answers()
            .iter()
            .filter_map(|r| match &r.data {
                RData::MX(mx) => Some(mx.exchange.to_string()),
                _ => None,
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    targets.sort();
    if targets.is_empty() {
        fail(
            "MX",
            "No MX records for the domain.",
            &["Set Email Routing as the catch-all in the Cloudflare dashboard.".into()],
        );
        return Check::Fail;
    }
    let cf = targets
        .iter()
        .any(|t| t.trim_end_matches('.').ends_with("cloudflare.net"));
    if cf {
        ok("MX", "Cloudflare Email Routing.", &targets);
        Check::Ok
    } else {
        fail("MX", "MX does not point to Cloudflare.", &targets);
        Check::Fail
    }
}

async fn check_spf(resolver: &TokioResolver, domain: &str) -> Check {
    let records = txt_records(resolver, domain).await;
    let spf: Vec<String> = records.into_iter().filter(|t| t.starts_with("v=spf1")).collect();
    if spf.is_empty() {
        fail(
            "SPF",
            "No `v=spf1` TXT record at the apex.",
            &["Recommended: `v=spf1 include:_spf.mx.cloudflare.net ~all`".into()],
        );
        return Check::Fail;
    }
    if spf.len() > 1 {
        fail(
            "SPF",
            "Multiple SPF records — receivers MUST treat this as `permerror`.",
            &spf,
        );
        return Check::Fail;
    }
    let cf = spf[0].contains("_spf.mx.cloudflare.net") || spf[0].contains("cloudflare.net");
    if cf {
        ok("SPF", "Authorises Cloudflare to send for this domain.", &spf);
        Check::Ok
    } else {
        let mut lines = spf;
        lines.push("Add `include:_spf.mx.cloudflare.net` to the existing record.".into());
        fail("SPF", "SPF exists but does not include Cloudflare.", &lines);
        Check::Fail
    }
}

async fn check_dkim(resolver: &TokioResolver, domain: &str) -> Check {
    let selector = format!("cloudflare._domainkey.{domain}");
    let records = txt_records(resolver, &selector).await;
    if records.is_empty() {
        info(
            "DKIM",
            "No `cloudflare._domainkey` selector.",
            &[
                "Cloudflare publishes this automatically when Email Service is enabled and a deploy has run.".into(),
                "If you've deployed and still see no record, re-run `wrangler deploy` and recheck.".into(),
            ],
        );
        return Check::Info;
    }
    ok("DKIM", "Cloudflare DKIM selector published.", &records);
    Check::Ok
}

async fn check_dmarc(resolver: &TokioResolver, domain: &str) -> Check {
    let name = format!("_dmarc.{domain}");
    let records = txt_records(resolver, &name).await;
    let dmarc: Vec<String> = records.into_iter().filter(|t| t.starts_with("v=DMARC1")).collect();
    if dmarc.is_empty() {
        info(
            "DMARC",
            "No `_dmarc` policy.",
            &[
                "Start with monitoring-only: `v=DMARC1; p=none; rua=mailto:dmarc@yourdomain`".into(),
                "Tighten to `p=quarantine` then `p=reject` once aggregate reports look clean.".into(),
            ],
        );
        return Check::Info;
    }
    let policy = dmarc[0]
        .split(';')
        .find_map(|s| s.trim().strip_prefix("p="))
        .unwrap_or("?");
    ok("DMARC", &format!("Policy `p={policy}`."), &dmarc);
    Check::Ok
}

async fn check_mtasts(resolver: &TokioResolver, domain: &str) -> Check {
    let name = format!("_mta-sts.{domain}");
    let records = txt_records(resolver, &name).await;
    if records.is_empty() {
        info("MTA-STS", "Not configured (optional).", &[]);
        return Check::Info;
    }
    ok("MTA-STS", "Configured.", &records);
    Check::Ok
}

// ── Helper: TXT lookup that flattens chunked strings into one logical record ─

async fn txt_records(resolver: &TokioResolver, name: &str) -> Vec<String> {
    let Ok(answer) = resolver.txt_lookup(name).await else {
        return Vec::new();
    };
    answer
        .answers()
        .iter()
        .filter_map(|r| match &r.data {
            // TXT records can be chunked into multiple <character-string>s;
            // concatenate them (no separator) to reconstruct the logical
            // record. SPF/DMARC strings longer than 255 bytes rely on this.
            RData::TXT(txt) => Some(
                txt.txt_data
                    .iter()
                    .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect()
}
