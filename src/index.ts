import { loadConfig } from './config.js';
import { createDb } from './db.js';
import { createChain } from './chain/client.js';
import { webhookProducer } from './worker/queue.js';
import { healthReport } from './ops/health.js';
import { buildServer, type DeliveryStore } from './server.js';

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
const app = await buildServer(cfg, store, {
  db, csrfSecret: cfg.CSRF_SECRET,
  chain: { ...chain, getBounty: async (id) => ({ status: (await chain.getBounty(id)).status.tag }) },
}, () => healthReport({ db, latestLedger: async () => (await chain.server.getLatestLedger()).sequence }));
await app.listen({ port: cfg.PORT, host: '0.0.0.0' });
