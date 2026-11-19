import { afterAll, describe, expect, it } from 'vitest';
import { createChain } from '../src/chain/client.js';
import { createDb } from '../src/db.js';
import { CURSOR_ID, pollOnce } from '../src/indexer/indexer.js';

const CONTRACT = 'CCEKBPYCWKNTXIQLXJHVAF7RYGUZZ6MITA7VVJBOK5BHXUO44ELZ4F4V';
const db = createDb(process.env.TEST_DATABASE_URL ?? 'postgresql://skillescrow:skillescrow@localhost:5432/skillescrow_test');
afterAll(() => db.$disconnect());

// Indexes the real smoke-run events on testnet; LIVE_CHAIN=1 to run. Fails once the RPC retention window passes them.
describe.runIf(process.env.LIVE_CHAIN)('indexer against testnet', () => {
  it('DB equals chain, and a second pass changes nothing', async () => {
    await db.$executeRawUnsafe('TRUNCATE "Payout","Dispute","PullRequestBounty","PullRequest","Bounty","FundingIntent","Repo","Installation","IndexerCursor" CASCADE');
    await db.installation.create({ data: { id: 1n, accountLogin: 'o', accountType: 'User' } });
    await db.repo.create({ data: { id: 1n, installationId: 1n, fullName: 'o/r', private: false } });
    const chain = createChain({ rpcUrl: 'https://soroban-testnet.stellar.org', networkPassphrase: 'Test SDF Network ; September 2015', contractId: CONTRACT });
    const latest = (await chain.server.getLatestLedger()).sequence;
    await pollOnce({ db, server: chain.server, contractId: CONTRACT }, latest - 20_000);
    const rows = await db.bounty.findMany({ orderBy: { id: 'asc' } });
    expect(rows.length).toBeGreaterThanOrEqual(2);
    for (const r of rows) {
      const c = await chain.getBounty(r.id);
      expect(r.status).toBe(c.status.tag.toUpperCase());
      expect(r.amount.toString()).toBe(c.amount.toString());
      expect(r.maintainer).toBe(c.maintainer);
    }
    const ser = (x: unknown) => JSON.stringify(x, (_, v) => (typeof v === 'bigint' ? v.toString() : v));
    const before = ser(rows);
    const cur = await db.indexerCursor.findUniqueOrThrow({ where: { id: CURSOR_ID } });
    await pollOnce({ db, server: chain.server, contractId: CONTRACT });
    expect(ser(await db.bounty.findMany({ orderBy: { id: 'asc' } }))).toBe(before);
    expect(cur.cursor).toBeTruthy();
  });
});
