use crate::runtime::Runtime;
use anyhow::{Context, Result};
use escrow_ai::{AnthropicModel, eval::run_eval};
use escrow_chain::ChainReader;
use escrow_core::config::Config;
use escrow_engine::{indexer, sync::sync_installations, worker};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Cancels `stop` on Ctrl-C or SIGTERM so every loop finishes its current unit of work and exits.
pub async fn shutdown_signal(stop: CancellationToken) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
    stop.cancel();
}

pub async fn serve(rt: &Runtime, stop: CancellationToken) -> Result<()> {
    let addr = format!("0.0.0.0:{}", rt.cfg.port);
    let listener = tokio::net::TcpListener::bind(&addr).await.with_context(|| format!("could not listen on {addr}"))?;
    tracing::info!(%addr, network = ?rt.cfg.network, github = rt.github_configured, "API listening");
    escrow_api::serve(listener, rt.api_state(), async move { stop.cancelled().await }).await.context("API server failed")
}

pub async fn worker(rt: &Runtime, stop: CancellationToken) -> Result<()> {
    let deps = rt.worker_deps();
    if !rt.github_configured {
        tracing::warn!("GitHub App is not configured: webhook jobs, cards and comments will wait for it (see docs/GITHUB_APP.md)");
    } else if let Err(e) = sync_installations(&rt.db, rt.gh.as_ref()).await {
        tracing::warn!(error = %e, "could not reconcile installations at start-up");
    }
    if rt.cfg.anthropic_api_key.is_some() && !rt.github_configured {
        tracing::warn!("ANTHROPIC_API_KEY is set but AI review needs the GitHub App too; reviews are off");
    }
    if !rt.ai_enabled {
        tracing::info!("AI review is off (needs the GitHub App and ANTHROPIC_API_KEY)");
    }

    let mut tasks = tokio::task::JoinSet::new();
    let interval = Duration::from_secs(rt.cfg.indexer_interval_secs.max(1));
    tasks.spawn(indexer::run(rt.indexer_deps(None), stop.clone(), interval));

    match &rt.relayer {
        Some(relayer) => {
            use escrow_chain::ReleaseSigner;
            // a relayer key the contract does not recognise can only fail; say so instead of burning attempts
            let registered = rt.chain.get_config().await.map(|c| c.relayer).ok();
            if registered.as_deref().is_some_and(|r| r != relayer.address()) {
                tracing::error!(configured = %relayer.address(), on_chain = ?registered, "RELAYER_SECRET is not the relayer registered in the contract; automatic payouts are OFF");
            } else {
                tracing::info!(relayer = %relayer.address(), "relayer on");
                let (d, r, s) = (rt.payout_deps(), relayer.clone(), stop.clone());
                tasks.spawn(async move { escrow_payouts::run_relayer(d, r.as_ref(), s, Duration::from_secs(5)).await });
            }
        }
        None => tracing::info!("relayer off: no RELAYER_SECRET (testnet/dev only; mainnet needs a KMS signer)"),
    }

    tasks.spawn(worker::run_queue(deps.clone(), stop.clone(), rt.cfg.worker_concurrency, Duration::from_millis(500)));
    tasks.spawn(worker::run_periodic(deps, stop.clone()));
    tracing::info!("worker running");
    while tasks.join_next().await.is_some() {}
    Ok(())
}

pub async fn reindex(rt: Runtime, from_ledger: u32) -> Result<()> {
    escrow_db::webhooks::delete_cursor(&rt.db.pool, indexer::CURSOR_ID).await?;
    let deps = rt.indexer_deps(Some(from_ledger));
    let r = indexer::poll_once(&deps).await.context("reindex failed")?;
    println!(
        "read {} event(s) from ledger {from_ledger}: {} applied, {} anomalies{}",
        r.events_seen,
        r.applied,
        r.anomalies,
        if r.cursor_reset { " (cursor was outside the RPC's retention window)" } else { "" }
    );
    Ok(())
}

pub async fn eval(cfg: &Config) -> Result<()> {
    let key = cfg.anthropic_api_key.as_deref().context("ANTHROPIC_API_KEY is not set")?;
    let model = AnthropicModel::new(key, &cfg.ai_model);
    println!("evaluating {} against the built-in cases...\n", cfg.ai_model);
    let results = run_eval(&model).await;
    let mut failed = 0;
    for r in &results {
        let mark = if r.passed() { "pass" } else { "FAIL" };
        println!(
            "{mark}  {:<40} {:<16} score {:>3}  flags {:?}",
            r.name,
            r.verdict.as_str(),
            r.score,
            r.flags.iter().map(|f| f.as_str()).collect::<Vec<_>>()
        );
        for f in &r.failures {
            println!("        - {f}");
        }
        failed += usize::from(!r.passed());
    }
    println!("\n{}/{} passed", results.len() - failed, results.len());
    if failed > 0 {
        anyhow::bail!("{failed} case(s) failed");
    }
    Ok(())
}
