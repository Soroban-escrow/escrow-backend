import { afterAll, beforeEach, describe, expect, it } from 'vitest';
import { createDb } from '../src/db.js';
import { backoffMs, relayerPass } from '../src/payouts/loop.js';
import type { PayoutDeps } from '../src/payouts/payout.js';
import type { RelayerPort } from '../src/payouts/ports.js';

const db = createDb(process.env.TEST_DATABASE_URL ?? 'postgresql://skillescrow:skillescrow@localhost:5432/skillescrow_test');
afterAll(() => db.$disconnect());
const deps = (): PayoutDeps => ({ db, webUrl: 'x', chain: { getBounty: async () => ({ status: 'Open', relayerAllowed: true, maintainer: 'GM' }) }, notifier: { notify: async () => {} } });

beforeEach(async () => {
  await db.$executeRawUnsafe('TRUNCATE "Payout","Bounty","Repo","Installation" CASCADE');
  await db.installation.create({ data: { id: 1n, accountLogin: 'o', accountType: 'User' } });
  await db.repo.create({ data: { id: 100n, installationId: 1n, fullName: 'o/r', private: false } });
  for (const id of [1n, 2n]) {
    await db.bounty.create({ data: { id, repoId: 100n, issueNumber: Number(id), issueTitle: '', issueUrl: '', criteria: [], maintainer: 'GM', token: 'C', amount: '1', feeBps: 300, deadline: new Date(), relayerAllowed: true, status: 'OPEN', fundTx: 't', createdAt: new Date() } });
    await db.payout.create({ data: { bountyId: id, prNumber: 1, winnerGithubId: 9n, winnerAddress: 'GW', status: 'QUEUED', idempotencyKey: `release:${id}` } });
  }
});

describe('relayerPass', () => {
  it('processes payouts one at a time, in order', async () => {
    let active = 0, maxActive = 0; const order: bigint[] = [];
    const r: RelayerPort = {
      submitRelease: async (id) => { active++; maxActive = Math.max(maxActive, active); order.push(id); await new Promise((x) => setTimeout(x, 20)); active--; return `H${id}`; },
      waitForTx: async () => 'SUCCESS',
    };
    expect(await relayerPass(deps(), r, new Map())).toBe(2);
    expect(order).toEqual([1n, 2n]);
    expect(maxActive).toBe(1);
    expect(await db.payout.count({ where: { status: 'CONFIRMED' } })).toBe(2);
    expect(await relayerPass(deps(), r, new Map())).toBe(0); // nothing left to do
  });
  it('backs off a transiently failing payout without blocking the others', async () => {
    let t = 1_000_000; const retryAt = new Map<string, number>();
    const r: RelayerPort = { submitRelease: async (id) => { if (id === 1n) throw new Error('rpc down'); return 'H2'; }, waitForTx: async () => 'SUCCESS' };
    expect(await relayerPass(deps(), r, retryAt, () => t)).toBe(2);
    expect((await db.payout.findUniqueOrThrow({ where: { idempotencyKey: 'release:2' } })).status).toBe('CONFIRMED');
    expect(retryAt.size).toBe(1);
    expect(await relayerPass(deps(), r, retryAt, () => t + 500)).toBe(0); // still backing off
    t += backoffMs(2) + 1;
    expect(await relayerPass(deps(), r, retryAt, () => t)).toBe(1);
  });
  it('backoff grows and is capped', () => {
    expect(backoffMs(1)).toBe(2000);
    expect(backoffMs(3)).toBe(8000);
    expect(backoffMs(30)).toBe(300_000);
  });
});
