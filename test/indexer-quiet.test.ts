import { afterAll, beforeEach, describe, expect, it } from 'vitest';
import { createDb } from '../src/db.js';
import { CURSOR_ID, cursorLedger, pollOnce } from '../src/indexer/indexer.js';

const db = createDb(process.env.TEST_DATABASE_URL ?? 'postgresql://skillescrow:skillescrow@localhost:5432/skillescrow_test');
afterAll(() => db.$disconnect());
beforeEach(() => db.$executeRawUnsafe('TRUNCATE "IndexerCursor"'));

// a cursor as the real RPC returns it: TOID, ledger in the high 32 bits
const toid = (ledger: number) => `${(BigInt(ledger) << 32n).toString().padStart(19, '0')}-4294967295`;

describe('quiet contract', () => {
  it('advances the stored ledger even when there are no events', async () => {
    const server = { getLatestLedger: async () => ({ sequence: 5_000_000 }), getEvents: async () => ({ events: [], cursor: toid(5_000_000), latestLedger: 5_000_000 }) };
    await pollOnce({ db, server: server as never, contractId: 'C' });
    const c = await db.indexerCursor.findUniqueOrThrow({ where: { id: CURSOR_ID } });
    expect(c.ledger).toBe(5_000_000);
    expect(cursorLedger(c.cursor!)).toBe(5_000_000);
  });
});
