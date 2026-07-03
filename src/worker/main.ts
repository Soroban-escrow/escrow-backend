import { loadConfig, loadDotEnv } from '../config.js';
import { createChain } from '../chain/client.js';
import { createDb } from '../db.js';
import { run as runIndexer } from '../indexer/indexer.js';
import { alertSender, collectAlerts } from '../ops/alerts.js';
import { sendRefundReminders } from '../ops/reminders.js';
import { runRelayer } from '../payouts/loop.js';
import { createRelayer } from '../payouts/relayer.js';
import { anthropicModel } from '../ai/anthropic.js';
import { runPrReview } from '../ai/pipeline.js';
import { createGithubClient, asPort } from '../github/client.js';
import type { GithubPort } from '../github/commands.js';
import { announcePayout, githubIssueCommenter, githubNotifier, refreshBountyCard } from '../github/sync.js';
import { dispatchWebhook } from './dispatch.js';
import { reviewProducer, reviewWorker } from './review-queue.js';
import { webhookWorker } from './queue.js';

loadDotEnv();
const cfg = loadConfig();
const db = createDb(cfg.DATABASE_URL);
const chain = createChain({ rpcUrl: cfg.STELLAR_RPC_URL, networkPassphrase: cfg.STELLAR_NETWORK_PASSPHRASE, contractId: cfg.BOUNTY_CONTRACT_ID });
const log = (msg: string, extra?: unknown) => console.log(JSON.stringify({ msg, extra }));

// Without GitHub App credentials the worker still runs, but jobs that need GitHub fail loudly and the
// queue retries them, rather than silently doing nothing.
const notConfigured = () => { throw new Error('GitHub App is not configured (see docs/GITHUB_APP.md)'); };
const ghClient = cfg.GITHUB_APP_ID && cfg.GITHUB_APP_PRIVATE_KEY
  ? createGithubClient({ appId: cfg.GITHUB_APP_ID, privateKey: cfg.GITHUB_APP_PRIVATE_KEY })
  : null;
const gh: GithubPort = ghClient ? asPort(ghClient) : { getPermission: async () => notConfigured(), postComment: async () => notConfigured() };

const usdc = cfg.TOKEN_USDC_CONTRACT_ID;
const xlm = cfg.TOKEN_XLM_CONTRACT_ID;
const limits = { min: 10_000_000n, max: 5_000_000_000n };
const symbolFor = (t: string) => (t === usdc ? 'USDC' : t === xlm ? 'XLM' : `${t.slice(0, 4)}…`);
// AI review: needs the GitHub App (to read PRs and comment) and an Anthropic key. Off otherwise.
const reviewsOn = Boolean(ghClient && cfg.ANTHROPIC_API_KEY);
const reviews = reviewsOn ? reviewProducer(cfg.REDIS_URL) : null;
if (!reviewsOn) log('AI review disabled: needs the GitHub App and ANTHROPIC_API_KEY');

const deps = {
  db,
  commands: { db, gh, webUrl: cfg.WEB_PUBLIC_URL, ...(reviews ? { requestReview: (id: bigint) => reviews.schedule(id, 0, true) } : {}), minDays: 1, maxDays: 90, tokens: { USDC: { address: usdc, ...limits }, XLM: { address: xlm, ...limits } } },
  pulls: {
    db, webUrl: cfg.WEB_PUBLIC_URL, ...(reviews ? { reviewTrigger: (id: bigint) => void reviews.schedule(id).catch((e) => log('review schedule failed', e instanceof Error ? e.message : String(e))) } : {}),
    chain: { getBounty: async (id: bigint) => { const b = await chain.getBounty(id); return { status: b.status.tag, relayerAllowed: b.relayer_allowed, maintainer: b.maintainer, winner: b.winner ?? null }; } },
    notifier: ghClient ? githubNotifier({ db, gh: ghClient }) : { notify: async (b: bigint, pr: number, m: string) => log('notify skipped (GitHub App not configured)', { b: String(b), pr, m }) },
  },
  log,
};

const ac = new AbortController();
const reviewConsumer = reviewsOn
  ? reviewWorker(cfg.REDIS_URL, async (prId, force) => {
      const outcome = await runPrReview({
        db, gh: ghClient!, model: anthropicModel(cfg.ANTHROPIC_API_KEY!, cfg.AI_MODEL), modelName: cfg.AI_MODEL,
        maxPerPrPerDay: cfg.AI_MAX_REVIEWS_PER_PR_PER_DAY, maxPerRepoPerDay: cfg.AI_MAX_REVIEWS_PER_REPO_PER_DAY, log,
      }, prId, { force });
      log('review', { pr: String(prId), outcome });
    })
  : null;
const relayer = cfg.RELAYER_SECRET
  ? createRelayer({ rpcUrl: cfg.STELLAR_RPC_URL, networkPassphrase: cfg.STELLAR_NETWORK_PASSPHRASE, contractId: cfg.BOUNTY_CONTRACT_ID, secret: cfg.RELAYER_SECRET })
  : null;
if (relayer) void runRelayer(deps.pulls, relayer, ac.signal, 5000, log);
else log('relayer disabled: RELAYER_SECRET not set (testnet/dev only; mainnet needs a KMS signer)');

// Reminders are real issue comments when GitHub is configured; otherwise they fail without being marked as sent.
const reminderTimer = setInterval(() => {
  sendRefundReminders({ db, graceMs: 72 * 3600e3, webUrl: cfg.WEB_PUBLIC_URL, commenter: ghClient ? githubIssueCommenter({ db, gh: ghClient }) : { comment: async () => notConfigured() } })
    .catch((e) => log('reminders skipped', e instanceof Error ? e.message : String(e)));
}, 3600e3);

const send = alertSender(cfg.ALERT_WEBHOOK_URL, log);
const alertTimer = setInterval(async () => {
  try {
    const [balance, latest] = await Promise.all([relayer?.balanceXlm().catch(() => undefined), chain.server.getLatestLedger().then((l) => l.sequence).catch(() => undefined)]);
    await send(await collectAlerts({ db, minRelayerXlm: cfg.RELAYER_MIN_XLM_ALERT, ...(balance !== undefined ? { relayerBalanceXlm: balance } : {}), ...(latest !== undefined ? { latestLedger: latest } : {}) }));
  } catch (e) { log('alert check failed', e instanceof Error ? e.message : String(e)); }
}, 60_000);
const w = webhookWorker(cfg.REDIS_URL, async (job) => log('webhook handled', { id: job.id, event: job.event, result: await dispatchWebhook(deps as never, job.event, job.payload) }));
w.worker.on('failed', (j, e) => log('webhook job failed', { id: j?.id, error: e.message }));
// every chain event for a bounty refreshes its GitHub card (best effort; the chain is the truth)
const onApplied = ghClient
  ? ({ bountyId, kind }: { bountyId: bigint; kind: string }) => {
      const sync = { db, gh: ghClient, webUrl: cfg.WEB_PUBLIC_URL, symbolFor, log };
      const fail = (what: string) => (e: unknown) => log(`${what} failed`, e instanceof Error ? e.message : String(e));
      if (kind === 'released') void announcePayout({ ...sync, explorerTx: (h) => cfg.EXPLORER_TX_URL_TEMPLATE.replace('{hash}', encodeURIComponent(h)) }, bountyId).catch(fail('payout announcement'));
      void refreshBountyCard(sync, bountyId).catch(fail('card refresh'));
    }
  : undefined;
void runIndexer({ db, server: chain.server, contractId: cfg.BOUNTY_CONTRACT_ID, log, ...(onApplied ? { onApplied } : {}) }, ac.signal);
for (const s of ['SIGINT', 'SIGTERM'] as const) process.on(s, () => { ac.abort(); clearInterval(alertTimer); clearInterval(reminderTimer); void Promise.all([w.close(), reviewConsumer?.close(), reviews?.close()]).then(() => process.exit(0)); });
log('worker started');
