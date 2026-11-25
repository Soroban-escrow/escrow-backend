import { randomUUID } from 'node:crypto';
import { describe, expect, it } from 'vitest';
import { reviewProducer, reviewWorker } from '../src/worker/review-queue.js';

const REDIS = process.env.TEST_REDIS_URL ?? 'redis://localhost:6379/1';
const wait = async (cond: () => boolean, ms = 6000) => { for (let i = 0; i < ms / 50 && !cond(); i++) await new Promise((r) => setTimeout(r, 50)); };

describe('review queue (real Redis)', () => {
  it('debounces: pushes within the quiet period produce one review', async () => {
    const prId = BigInt(Date.now()) * 1000n + BigInt(Math.floor(Math.random() * 999));
    const ran: [bigint, boolean][] = [];
    const prod = reviewProducer(REDIS);
    const w = reviewWorker(REDIS, async (id, force) => void ran.push([id, force]));
    try {
      await prod.schedule(prId, 400);
      await new Promise((r) => setTimeout(r, 150));
      await prod.schedule(prId, 400); // a newer push resets the timer
      await new Promise((r) => setTimeout(r, 150));
      await prod.schedule(prId, 400);
      await wait(() => ran.length > 0);
      await new Promise((r) => setTimeout(r, 600));
      expect(ran).toEqual([[prId, false]]);
    } finally { await w.close(); await prod.close(); }
  });
  it('a forced request runs immediately with force=true', async () => {
    const prId = BigInt(Date.now()) * 1000n + BigInt(Math.floor(Math.random() * 999)) + 1n;
    const ran: [bigint, boolean][] = [];
    const prod = reviewProducer(REDIS);
    const w = reviewWorker(REDIS, async (id, force) => void ran.push([id, force]));
    try {
      await prod.schedule(prId, 60_000); // a long debounce is pending...
      await prod.schedule(prId, 0, true); // ...and a maintainer forces it now
      await wait(() => ran.length > 0);
      expect(ran).toEqual([[prId, true]]);
    } finally { await w.close(); await prod.close(); }
  });
});
