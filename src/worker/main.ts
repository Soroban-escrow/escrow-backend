import { loadConfig } from '../config.js';
import { createChain } from '../chain/client.js';
import { createDb } from '../db.js';
import { run as runIndexer } from '../indexer/indexer.js';
import { alertSender, collectAlerts } from '../ops/alerts.js';
import { sendRefundReminders } from '../ops/reminders.js';
import { runRelayer } from '../payouts/loop.js';
import { createRelayer } from '../payouts/relayer.js';
import type { GithubPort } from '../github/commands.js';
import { dispatchWebhook } from './dispatch.js';
import { webhookWorker } from './queue.js';

const cfg = loadConfig();
const db = createDb(cfg.DATABASE_URL);
const chain = createChain({ rpcUrl: cfg.STELLAR_RPC_URL, networkPassphrase: cfg.STELLAR_NETWORK_PASSPHRASE, contractId: cfg.BOUNTY_CONTRACT_ID });
const log = (msg: string, extra?: unknown) => console.log(JSON.stringify({ msg, extra }));

// The real GitHub client needs the registered GitHub App (docs/GITHUB_APP.md). Until then jobs that
// need GitHub fail loudly and are retried by the queue, rather than silently doing nothing.
const gh: GithubPort = {
  getPermission: async () => { throw new Error('GitHub App is not configured'); },
  postComment: async () => { throw new Error('GitHub App is not configured'); },
};

const usdc = cfg.TOKEN_USDC_CONTRACT_ID;
const xlm = cfg.TOKEN_XLM_CONTRACT_ID;
const limits = { min: 10_000_000n, max: 5_000_000_000n };
const deps = {
  db,
  commands: { db, gh, webUrl: cfg.WEB_PUBLIC_URL, minDays: 1, maxDays: 90, tokens: { USDC: { address: usdc, ...limits }, XLM: { address: xlm, ...limits } } },
  pulls: {
    db, webUrl: cfg.WEB_PUBLIC_URL,
    chain: { getBounty: async (id: bigint) => { const b = await chain.getBounty(id); return { status: b.status.tag, relayerAllowed: b.relayer_allowed, maintainer: b.maintainer }; } },
    notifier: { notify: async (b: bigint, pr: number, m: string) => log('notify (no GitHub client yet)', { b: String(b), pr, m }) },
  },
  log,
};

const ac = new AbortController();
const relayer = cfg.RELAYER_SECRET
  ? createRelayer({ rpcUrl: cfg.STELLAR_RPC_URL, networkPassphrase: cfg.STELLAR_NETWORK_PASSPHRASE, contractId: cfg.BOUNTY_CONTRACT_ID, secret: cfg.RELAYER_SECRET })
  : null;
if (relayer) void runRelayer(deps.pulls, relayer, ac.signal, 5000, log);
else log('relayer disabled: RELAYER_SECRET not set (testnet/dev only; mainnet needs a KMS signer)');

// Reminders become real issue comments once the GitHub client exists; until then they only log
// (and are NOT marked as sent, so they go out after the App is registered).
const reminderTimer = setInterval(() => {
  sendRefundReminders({ db, graceMs: 72 * 3600e3, webUrl: cfg.WEB_PUBLIC_URL, commenter: { comment: async () => { throw new Error('GitHub App is not configured'); } } })
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
void runIndexer({ db, server: chain.server, contractId: cfg.BOUNTY_CONTRACT_ID, log }, ac.signal);
for (const s of ['SIGINT', 'SIGTERM'] as const) process.on(s, () => { ac.abort(); clearInterval(alertTimer); clearInterval(reminderTimer); void w.close().then(() => process.exit(0)); });
log('worker started');
