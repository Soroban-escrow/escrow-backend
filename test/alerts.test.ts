import { afterAll, beforeEach, describe, expect, it } from 'vitest';
import { createDb } from '../src/db.js';
import { alertSender, collectAlerts } from '../src/ops/alerts.js';
import { loadConfig } from '../src/config.js';

const db = createDb(process.env.TEST_DATABASE_URL ?? 'postgresql://skillescrow:skillescrow@localhost:5432/skillescrow_test');
afterAll(() => db.$disconnect());
const NOW = new Date('2026-11-02T12:00:00Z');

beforeEach(async () => {
  await db.$executeRawUnsafe('TRUNCATE "Payout","Dispute","Bounty","Repo","Installation","IndexerCursor" CASCADE');
  await db.installation.create({ data: { id: 1n, accountLogin: 'o', accountType: 'User' } });
  await db.repo.create({ data: { id: 100n, installationId: 1n, fullName: 'o/r', private: false } });
});
const bounty = (id: bigint, deadline: Date) => db.bounty.create({ data: { id, repoId: 100n, issueNumber: Number(id), issueTitle: '', issueUrl: '', criteria: [], maintainer: 'GM', token: 'C', amount: '1', feeBps: 300, deadline, relayerAllowed: true, status: 'OPEN', fundTx: 't', createdAt: NOW } });

describe('collectAlerts', () => {
  it('is quiet when healthy', async () => {
    expect(await collectAlerts({ db, now: () => NOW, minRelayerXlm: 50, relayerBalanceXlm: 500 })).toEqual([]);
  });
  it('flags low relayer balance, failed payouts, indexer lag and old disputes', async () => {
    await bounty(1n, new Date(NOW.getTime() + 30 * 864e5));
    await db.payout.create({ data: { bountyId: 1n, prNumber: 1, winnerGithubId: 9n, status: 'FAILED', idempotencyKey: 'release:1' } });
    await db.indexerCursor.create({ data: { id: 'bounty_escrow', ledger: 1000 } });
    await db.dispute.create({ data: { bountyId: 1n, openedByGithubId: 9n, reason: 'x', evidence: {}, status: 'requested', createdAt: new Date(NOW.getTime() - 40 * 3600e3) } });
    const a = await collectAlerts({ db, now: () => NOW, minRelayerXlm: 50, relayerBalanceXlm: 3, latestLedger: 1200 });
    expect(a.join('|')).toMatch(/Relayer balance low/);
    expect(a.join('|')).toMatch(/1 payout\(s\) FAILED/);
    expect(a.join('|')).toMatch(/200 ledgers behind/);
    expect(a.join('|')).toMatch(/older than 36 h/);
  });
  it('flags a merged-but-unpaid PR when the refund window is near, but not when far away', async () => {
    await bounty(1n, new Date(NOW.getTime() + 24 * 3600e3));
    await bounty(2n, new Date(NOW.getTime() + 20 * 864e5));
    for (const id of [1n, 2n]) await db.payout.create({ data: { bountyId: id, prNumber: 1, winnerGithubId: 9n, status: 'PENDING_WALLET', idempotencyKey: `release:${id}` } });
    const a = await collectAlerts({ db, now: () => NOW, minRelayerXlm: 50 });
    expect(a).toHaveLength(1);
    expect(a[0]).toContain('Bounty 1');
  });
});

describe('alertSender', () => {
  it('posts once per message per cooldown', async () => {
    const posts: string[] = [];
    const f = (async (_u: string, init: { body: string }) => (posts.push(init.body), new Response())) as unknown as typeof fetch;
    const send = alertSender('https://hook.test', () => {}, 1000, f);
    await send(['a', 'b'], 0); await send(['a'], 500); await send(['a'], 1500);
    expect(posts).toHaveLength(3);
  });
});

describe('config', () => {
  const base = { DATABASE_URL: 'x', BOUNTY_CONTRACT_ID: 'CCEKBPYCWKNTXIQLXJHVAF7RYGUZZ6MITA7VVJBOK5BHXUO44ELZ4F4V', SESSION_SECRET: 's'.repeat(32), CSRF_SECRET: 'c'.repeat(32), GITHUB_WEBHOOK_SECRET: 'hooksecret' };
  it('refuses an env-held relayer secret on mainnet', () => {
    const secret = 'S' + 'A'.repeat(55);
    expect(loadConfig({ ...base, RELAYER_SECRET: secret }).RELAYER_SECRET).toBe(secret);
    expect(() => loadConfig({ ...base, STELLAR_NETWORK: 'mainnet', RELAYER_SECRET: secret })).toThrow(/mainnet/);
    expect(() => loadConfig({ ...base, RELAYER_SECRET: 'nope' })).toThrow();
  });
});
