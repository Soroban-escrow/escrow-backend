import { loadConfig } from './config.js';
import { createDb } from './db.js';
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

const app = await buildServer(cfg, store, { db, csrfSecret: cfg.CSRF_SECRET });
await app.listen({ port: cfg.PORT, host: '0.0.0.0' });
