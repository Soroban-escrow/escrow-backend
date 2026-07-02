import { afterAll, beforeEach, describe, expect, it } from 'vitest';
import { createDb } from '../src/db.js';
import { healthReport } from '../src/ops/health.js';

const db = createDb(process.env.TEST_DATABASE_URL ?? 'postgresql://skillescrow:skillescrow@localhost:5432/skillescrow_test');
afterAll(() => db.$disconnect());
beforeEach(() => db.$executeRawUnsafe('TRUNCATE "IndexerCursor"'));

describe('healthReport', () => {
  it('is ok with a fresh indexer and reachable RPC', async () => {
    await db.indexerCursor.create({ data: { id: 'bounty_escrow', ledger: 990 } });
    expect(await healthReport({ db, latestLedger: async () => 1000 })).toEqual({ ok: true, db: true, rpc: true, redis: true, indexerLagLedgers: 10 });
  });
  it('is ok before the indexer has a cursor', async () => {
    expect((await healthReport({ db, latestLedger: async () => 1000 })).indexerLagLedgers).toBeNull();
  });
  it('fails when the indexer is far behind, the RPC is down, or Redis is down', async () => {
    await db.indexerCursor.create({ data: { id: 'bounty_escrow', ledger: 100 } });
    expect((await healthReport({ db, latestLedger: async () => 1000 })).ok).toBe(false);
    const rpcDown = await healthReport({ db, latestLedger: async () => { throw new Error('down'); } });
    expect(rpcDown).toMatchObject({ ok: false, rpc: false });
    const redisDown = await healthReport({ db, latestLedger: async () => 100, redisPing: async () => { throw new Error('x'); } });
    expect(redisDown).toMatchObject({ ok: false, redis: false });
  });
});
