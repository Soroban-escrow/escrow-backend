import { createHmac } from 'node:crypto';
import { describe, expect, it } from 'vitest';
import { loadConfig } from '../src/config.js';
import { buildServer, type DeliveryStore } from '../src/server.js';

const env = {
  DATABASE_URL: 'postgresql://x',
  BOUNTY_CONTRACT_ID: 'CCEKBPYCWKNTXIQLXJHVAF7RYGUZZ6MITA7VVJBOK5BHXUO44ELZ4F4V',
  SESSION_SECRET: 's'.repeat(32),
  CSRF_SECRET: 'c'.repeat(32),
  GITHUB_WEBHOOK_SECRET: 'hooksecret',
  LOG_LEVEL: 'silent',
};
const sign = (body: string) => 'sha256=' + createHmac('sha256', 'hooksecret').update(body).digest('hex');

function fake() {
  const seen = new Set<string>();
  const queued: string[] = [];
  const store: DeliveryStore = {
    insert: async (id) => (seen.has(id) ? false : (seen.add(id), true)),
    enqueue: async (id) => void queued.push(id),
  };
  return { store, queued };
}

const post = (app: Awaited<ReturnType<typeof buildServer>>, body: string, headers: Record<string, string>) =>
  app.inject({ method: 'POST', url: '/github/webhooks', payload: body, headers: { 'content-type': 'application/json', ...headers } });

describe('config', () => {
  it('fails fast on missing vars', () => {
    expect(() => loadConfig({})).toThrow(/DATABASE_URL/);
    expect(loadConfig(env).PORT).toBe(3001);
  });
});

describe('webhook', () => {
  const body = JSON.stringify({ action: 'opened' });
  it('accepts a valid signature and queues', async () => {
    const { store, queued } = fake();
    const app = await buildServer(loadConfig(env), store);
    const r = await post(app, body, { 'x-hub-signature-256': sign(body), 'x-github-delivery': 'd1', 'x-github-event': 'issues' });
    expect(r.statusCode).toBe(202);
    expect(queued).toEqual(['d1']);
  });
  it('rejects invalid and missing signatures', async () => {
    const { store, queued } = fake();
    const app = await buildServer(loadConfig(env), store);
    const h = { 'x-github-delivery': 'd1', 'x-github-event': 'issues' };
    expect((await post(app, body, { ...h, 'x-hub-signature-256': sign('other') })).statusCode).toBe(401);
    expect((await post(app, body, h)).statusCode).toBe(401);
    expect((await post(app, body, { ...h, 'x-hub-signature-256': 'sha256=zz' })).statusCode).toBe(401);
    expect(queued).toEqual([]);
  });
  it('ignores duplicate deliveries', async () => {
    const { store, queued } = fake();
    const app = await buildServer(loadConfig(env), store);
    const h = { 'x-hub-signature-256': sign(body), 'x-github-delivery': 'd1', 'x-github-event': 'issues' };
    expect((await post(app, body, h)).statusCode).toBe(202);
    expect((await post(app, body, h)).statusCode).toBe(200);
    expect(queued).toEqual(['d1']);
  });
  it('health', async () => {
    const app = await buildServer(loadConfig(env), fake().store);
    expect((await app.inject('/health')).statusCode).toBe(200);
  });
});
