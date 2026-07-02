import { loadConfig } from './config.js';
import { createDb } from './db.js';
import { createChain } from './chain/client.js';
import { buildServer, type DeliveryStore } from './server.js';

const cfg = loadConfig();
const db = createDb(cfg.DATABASE_URL);

// Real queue wiring (BullMQ) is a follow-up: deliveries are recorded, and jobs are only logged for now.
const store: DeliveryStore = {
  insert: async (id, event) => {
    try { await db.webhookDelivery.create({ data: { id, event } }); return true; }
    catch (e) { if ((e as { code?: string }).code === 'P2002') return false; throw e; }
  },
  enqueue: async (id, event) => { console.log(JSON.stringify({ msg: 'webhook queued (no consumer yet)', id, event })); },
};

const chain = createChain({ rpcUrl: cfg.STELLAR_RPC_URL, networkPassphrase: cfg.STELLAR_NETWORK_PASSPHRASE, contractId: cfg.BOUNTY_CONTRACT_ID });
const app = await buildServer(cfg, store, {
  db, csrfSecret: cfg.CSRF_SECRET,
  chain: { ...chain, getBounty: async (id) => ({ status: (await chain.getBounty(id)).status.tag }) },
});
await app.listen({ port: cfg.PORT, host: '0.0.0.0' });
