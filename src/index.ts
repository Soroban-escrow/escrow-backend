import { loadConfig, loadDotEnv } from './config.js';
import { createDb } from './db.js';
import { createChain } from './chain/client.js';
import { webhookProducer } from './worker/queue.js';
import { healthReport } from './ops/health.js';
import { createGithubClient } from './github/client.js';
import { githubNotifier } from './github/sync.js';
import { onWalletLinked } from './payouts/payout.js';
import { buildServer, type DeliveryStore } from './server.js';

loadDotEnv();
const cfg = loadConfig();
const db = createDb(cfg.DATABASE_URL);

const producer = webhookProducer(cfg.REDIS_URL);
const store: DeliveryStore = {
  insert: async (id, event) => {
    try { await db.webhookDelivery.create({ data: { id, event } }); return true; }
    catch (e) { if ((e as { code?: string }).code === 'P2002') return false; throw e; }
  },
  enqueue: (id, event, payload) => producer.enqueue(id, event, payload),
};

const chain = createChain({ rpcUrl: cfg.STELLAR_RPC_URL, networkPassphrase: cfg.STELLAR_NETWORK_PASSPHRASE, contractId: cfg.BOUNTY_CONTRACT_ID });
// a late wallet link must continue payouts that were waiting for it (BUILD_SPEC 11.3)
const gh = cfg.GITHUB_APP_ID && cfg.GITHUB_APP_PRIVATE_KEY ? createGithubClient({ appId: cfg.GITHUB_APP_ID, privateKey: cfg.GITHUB_APP_PRIVATE_KEY }) : null;
const payoutDeps = {
  db, webUrl: cfg.WEB_PUBLIC_URL,
  chain: { getBounty: async (id: bigint) => { const b = await chain.getBounty(id); return { status: b.status.tag, relayerAllowed: b.relayer_allowed, maintainer: b.maintainer, winner: b.winner ?? null }; } },
  notifier: gh ? githubNotifier({ db, gh }) : { notify: async () => {} },
};

const app = await buildServer(cfg, store, {
  db, csrfSecret: cfg.CSRF_SECRET, onWalletLinked: (githubId) => onWalletLinked(payoutDeps, githubId),
  ...(cfg.GITHUB_CLIENT_ID && cfg.GITHUB_CLIENT_SECRET ? { login: { db, clientId: cfg.GITHUB_CLIENT_ID, clientSecret: cfg.GITHUB_CLIENT_SECRET, callbackUrl: `${cfg.API_PUBLIC_URL}/auth/callback`, webUrl: cfg.WEB_PUBLIC_URL, secret: cfg.SESSION_SECRET, adminGithubIds: (cfg.ADMIN_GITHUB_IDS ?? '').split(',').filter(Boolean).map(BigInt) } } : {}),
  chain: { ...chain, getBounty: async (id) => ({ status: (await chain.getBounty(id)).status.tag }) },
}, () => healthReport({ db, latestLedger: async () => (await chain.server.getLatestLedger()).sequence }));
await app.listen({ port: cfg.PORT, host: '0.0.0.0' });
