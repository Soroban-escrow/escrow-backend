import Fastify, { type FastifyInstance } from 'fastify';
import helmet from '@fastify/helmet';
import rateLimit from '@fastify/rate-limit';
import type { Config } from './config.js';
import { verifySignature } from './github/signature.js';

/** Minimal storage the webhook route needs; the real one is Prisma, tests pass a fake. */
export interface DeliveryStore {
  /** returns false if the delivery id was already seen */
  insert(id: string, event: string): Promise<boolean>;
  enqueue(id: string, event: string, payload: unknown): Promise<void>;
}

export async function buildServer(cfg: Config, store: DeliveryStore): Promise<FastifyInstance> {
  const app = Fastify({ logger: { level: cfg.LOG_LEVEL, redact: ['req.headers.authorization', 'req.headers.cookie'] } });
  await app.register(helmet);
  await app.register(rateLimit, { max: 300, timeWindow: '1 minute' });

  // keep the raw body for signature verification
  app.addContentTypeParser('application/json', { parseAs: 'buffer' }, (_req, body, done) => done(null, body));

  app.get('/health', async () => ({ ok: true }));

  app.post('/github/webhooks', async (req, reply) => {
    const raw = req.body as Buffer;
    if (!verifySignature(cfg.GITHUB_WEBHOOK_SECRET, raw, req.headers['x-hub-signature-256'] as string | undefined)) {
      return reply.code(401).send({ error: { code: 'bad_signature', message: 'invalid signature' } });
    }
    const id = req.headers['x-github-delivery'];
    const event = req.headers['x-github-event'];
    if (typeof id !== 'string' || typeof event !== 'string') {
      return reply.code(400).send({ error: { code: 'bad_request', message: 'missing delivery headers' } });
    }
    let payload: unknown;
    try {
      payload = JSON.parse(raw.toString('utf8'));
    } catch {
      return reply.code(400).send({ error: { code: 'bad_json', message: 'invalid JSON' } });
    }
    if (!(await store.insert(id, event))) return reply.code(200).send({ duplicate: true });
    await store.enqueue(id, event, payload);
    return reply.code(202).send({ queued: true });
  });

  return app;
}
