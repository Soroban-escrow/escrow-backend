#![forbid(unsafe_code)]
//! `escrowd`: the SkillEscrow backend as one binary.
//!
//!   escrowd serve    the HTTP API
//!   escrowd worker   indexer, relayer, job queue, reminders, alerts
//!   escrowd all      both in one process (the simple single-host deployment)
//!   escrowd doctor   check configuration and every dependency, with fixes
//!   escrowd migrate  apply database migrations
//!   escrowd reindex  rebuild state from the chain
//!   escrowd eval     score the AI reviewer against known cases (needs ANTHROPIC_API_KEY)

mod doctor;
mod run;
mod runtime;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use escrow_core::config::Config;
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(name = "escrowd", version, about = "SkillEscrow Bounties backend")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the HTTP API
    Serve,
    /// Run the indexer, relayer and job queue
    Worker,
    /// Run the API and the worker in one process
    All,
    /// Apply database migrations and exit
    Migrate,
    /// Check configuration and dependencies; exits non-zero on a hard failure
    Doctor,
    /// Reconcile GitHub App installations and repositories now
    SyncInstallations,
    /// Re-read contract events from a ledger and apply them (safe to repeat)
    Reindex {
        /// first ledger to read (must still be within the RPC's retention window)
        #[arg(long)]
        from_ledger: u32,
    },
    /// Score the AI reviewer against the built-in cases
    Eval,
    /// Probe the local API's /health endpoint; exit 0 when healthy (for container health checks)
    Healthcheck,
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .or_else(|_| tracing_subscriber::EnvFilter::try_new(std::env::var("LOG_LEVEL").unwrap_or_else(|_| "info".into())))
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let b = tracing_subscriber::fmt().with_env_filter(filter).with_target(false);
    if std::env::var("LOG_FORMAT").is_ok_and(|f| f == "json") {
        b.json().init();
    } else {
        b.init();
    }
}

fn load_env_file() {
    match std::env::var("ENV_FILE") {
        Ok(f) => {
            let _ = dotenvy::from_filename(f);
        }
        Err(_) => {
            let _ = dotenvy::dotenv();
        }
    }
}

fn auto_migrate() -> bool {
    !matches!(std::env::var("AUTO_MIGRATE").as_deref(), Ok("false" | "0"))
}

#[tokio::main]
async fn main() -> Result<()> {
    load_env_file();
    let cli = Cli::parse();
    if matches!(cli.cmd, Cmd::Healthcheck) {
        return healthcheck();
    }
    init_logging();
    // fail fast with every problem listed, before touching the network
    let cfg = Config::from_env().map_err(|e| anyhow::anyhow!("{e}"))?;

    match cli.cmd {
        Cmd::Healthcheck => unreachable!("handled before the configuration is loaded"),
        Cmd::Doctor => doctor::run(cfg).await,
        Cmd::Migrate => {
            runtime::connect(cfg, true).await?;
            println!("migrations applied");
            Ok(())
        }
        Cmd::SyncInstallations => {
            let rt = runtime::connect(cfg, auto_migrate()).await?;
            let (i, r) = escrow_engine::sync::sync_installations(&rt.db, rt.gh.as_ref()).await.context("could not sync installations")?;
            println!("synced {i} installation(s) and {r} repositor{}", if r == 1 { "y" } else { "ies" });
            Ok(())
        }
        Cmd::Reindex { from_ledger } => run::reindex(runtime::connect(cfg, auto_migrate()).await?, from_ledger).await,
        Cmd::Eval => run::eval(&cfg).await,
        Cmd::Serve | Cmd::Worker | Cmd::All => {
            let rt = runtime::connect(cfg, auto_migrate()).await?;
            let stop = CancellationToken::new();
            tokio::spawn(run::shutdown_signal(stop.clone()));
            match cli.cmd {
                Cmd::Serve => run::serve(&rt, stop, false).await,
                Cmd::Worker => run::worker(&rt, stop).await,
                _ => {
                    let (a, b) = tokio::join!(run::serve(&rt, stop.clone(), true), run::worker(&rt, stop.clone()));
                    stop.cancel();
                    a.and(b)
                }
            }
        }
    }
}

/// A dependency-free HTTP probe: the runtime image has no curl. Reads PORT from the environment like the server does.
fn healthcheck() -> Result<()> {
    use std::{net::SocketAddr, time::Duration};
    let port: u16 = std::env::var("PORT")
        .unwrap_or_else(|_| "3001".into())
        .parse()
        .context("invalid PORT")?;
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    healthcheck_with_timeouts(addr, Duration::from_secs(3), Duration::from_secs(3), Duration::from_secs(5))
}

fn healthcheck_with_timeouts(
    addr: std::net::SocketAddr,
    connect_timeout: std::time::Duration,
    write_timeout: std::time::Duration,
    read_timeout: std::time::Duration,
) -> Result<()> {
    use std::{
        io::{Read, Write},
        net::TcpStream,
    };
    let mut s = TcpStream::connect_timeout(&addr, connect_timeout)
        .with_context(|| format!("connect failed: could not connect to {addr}"))?;
    s.set_write_timeout(Some(write_timeout)).context("set write timeout failed")?;
    s.set_read_timeout(Some(read_timeout)).context("set read timeout failed")?;
    s.write_all(b"GET /health HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .context("write failed: could not send /health request")?;
    let mut out = String::new();
    s.read_to_string(&mut out).context("read failed: could not read /health response")?;
    if out.starts_with("HTTP/1.0 200") || out.starts_with("HTTP/1.1 200") {
        Ok(())
    } else {
        anyhow::bail!("unhealthy: {}", out.lines().next().unwrap_or("no response"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        time::Duration,
    };

    #[test]
    fn healthcheck_succeeds_on_200_and_reports_step_on_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let t = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 128];
            let _ = s.read(&mut buf);
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}");
        });
        healthcheck_with_timeouts(addr, Duration::from_secs(1), Duration::from_secs(1), Duration::from_secs(1)).unwrap();
        t.join().unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let t = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 128];
            let _ = s.read(&mut buf);
            let _ = s.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 2\r\n\r\n{}");
        });
        let err = healthcheck_with_timeouts(addr, Duration::from_secs(1), Duration::from_secs(1), Duration::from_secs(1)).unwrap_err();
        assert!(format!("{err:#}").contains("unhealthy"), "{err:#}");
        t.join().unwrap();

        // Refused port reports connect failure
        let closed = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let err = healthcheck_with_timeouts(closed, Duration::from_millis(200), Duration::from_secs(1), Duration::from_secs(1)).unwrap_err();
        assert!(format!("{err:#}").contains("connect failed"), "{err:#}");

        // Silent peer reports read failure
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let t = std::thread::spawn(move || {
            let (_s, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(250));
        });
        let err = healthcheck_with_timeouts(addr, Duration::from_secs(1), Duration::from_secs(1), Duration::from_millis(50)).unwrap_err();
        assert!(format!("{err:#}").contains("read failed"), "{err:#}");
        t.join().unwrap();
    }
}
