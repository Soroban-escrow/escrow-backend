import { Keypair } from '@stellar/stellar-sdk';
import { afterAll, beforeEach, describe, expect, it } from 'vitest';
import { createSession } from '../src/auth/session.js';
import { loadConfig } from '../src/config.js';
import { createDb } from '../src/db.js';
import { buildServer } from '../src/server.js';

const db = createDb(process.env.TEST_DATABASE_URL ?? 'postgresql://skillescrow:skillescrow@localhost:5432/skillescrow_test');
afterAll(() => db.$disconnect());
const cfg = loadConfig({ DATABASE_URL: 'x', BOUNTY_CONTRACT_ID: 'CCEKBPYCWKNTXIQLXJHVAF7RYGUZZ6MITA7VVJBOK5BHXUO44ELZ4F4V', SESSION_SECRET: 's'.repeat(32), CSRF_SECRET: 'c'.repeat(32), GITHUB_WEBHOOK_SECRET: 'hooksecret', LOG_LEVEL: 'silent' });

const ARB = Keypair.random().publicKey();
const WIN = Keypair.random().publicKey();
let calls: unknown[][]; let fail: Error | null = null;
const chain = {
  buildFundTx: async () => 'x', buildReleaseTx: async () => 'x', buildRefundTx: async () => 'x', submitSigned: async () => 'h', getBounty: async () => ({ status: 'Open' }),
  buildOpenDisputeTx: async (...a: unknown[]) => { if (fail) throw fail; calls.push(['open', ...a]); return 'OPENXDR'; },
  buildResolveTx: async (...a: unknown[]) => { if (fail) throw fail; calls.push(['resolve', ...a]); return 'RESXDR'; },
};

async function login(githubId: bigint, isAdmin: boolean) {
  const u = await db.user.create({ data: { githubId, githubLogin: `u${githubId}`, isAdmin } });
  const cookie = `se_session=${(await createSession(db, u.id)).id}`;
  const a = await buildServer(cfg, { insert: async () => true, enqueue: async () => {} }, { db, csrfSecret: cfg.CSRF_SECRET, chain });
  const csrf = (await a.inject({ url: '/auth/me', headers: { cookie } })).json().csrfToken as string;
  return { a, h: { cookie, 'x-csrf-token': csrf } };
}
let dispute: string;
beforeEach(async () => {
  calls = []; fail = null;
  await db.$executeRawUnsafe('TRUNCATE "AuditLog","Payout","Dispute","PullRequest","Bounty","Repo","Installation","Session","User" CASCADE');
  await db.installation.create({ data: { id: 1n, accountLogin: 'o', accountType: 'User' } });
  await db.repo.create({ data: { id: 100n, installationId: 1n, fullName: 'o/r', private: false } });
  await db.bounty.create({ data: { id: 1n, repoId: 100n, issueNumber: 7, issueTitle: 'T', issueUrl: '', criteria: [], maintainer: 'GM', token: 'C', amount: '1000', feeBps: 300, deadline: new Date(), relayerAllowed: true, status: 'OPEN', fundTx: 't', createdAt: new Date() } });
  dispute = (await db.dispute.create({ data: { bountyId: 1n, openedByGithubId: 9n, reason: 'unfair', evidence: {}, status: 'requested' } })).id;
});

describe('admin guard', () => {
  it('every admin route refuses non-admins and anonymous users', async () => {
    const { a, h } = await login(1n, false);
    for (const [m, u] of [['GET', '/admin/disputes'], ['GET', '/admin/payouts'], ['GET', '/admin/status'], ['POST', `/admin/disputes/${dispute}/reject`], ['POST', `/admin/disputes/${dispute}/open-onchain`], ['POST', `/admin/disputes/${dispute}/resolve/build`], ['POST', '/admin/payouts/abcdefghijk/retry']] as const) {
      const r = await a.inject({ method: m, url: u, headers: h, payload: m === 'POST' ? {} : undefined });
      expect(r.statusCode, `${m} ${u}`).toBe(403);
    }
    expect((await a.inject('/admin/disputes')).statusCode).toBe(401);
  });
});

describe('disputes', () => {
  it('lists the queue oldest first and flags overdue ones', async () => {
    const { a, h } = await login(2n, true);
    await db.dispute.update({ where: { id: dispute }, data: { createdAt: new Date(Date.now() - 40 * 3600e3) } });
    const r = (await a.inject({ url: '/admin/disputes', headers: h })).json();
    expect(r).toHaveLength(1);
    expect(r[0].overdue).toBe(true);
    expect(r[0].bounty.repo.fullName).toBe('o/r');
  });
  it('open on chain, resolve and reject follow the state machine and write audit rows', async () => {
    const { a, h } = await login(2n, true);
    const post = (u: string, p: object) => a.inject({ method: 'POST', url: u, headers: h, payload: p });
    expect((await post(`/admin/disputes/${dispute}/resolve/build`, { arbiterAddress: ARB, winnerBps: 0 })).json().error.code).toBe('wrong_state');
    expect((await post(`/admin/disputes/${dispute}/open-onchain`, { arbiterAddress: 'bad' })).statusCode).toBe(400);
    expect((await post(`/admin/disputes/${dispute}/open-onchain`, { arbiterAddress: ARB })).json()).toEqual({ xdr: 'OPENXDR' });
    await db.dispute.update({ where: { id: dispute }, data: { status: 'opened_onchain' } });
    expect((await post(`/admin/disputes/${dispute}/resolve/build`, { arbiterAddress: ARB, winnerBps: 7000 })).json().error.code).toBe('winner_required');
    expect((await post(`/admin/disputes/${dispute}/resolve/build`, { arbiterAddress: ARB, winnerBps: 7000, winner: WIN, note: '70/30' })).json()).toEqual({ xdr: 'RESXDR' });
    expect(calls.at(-1)).toEqual(['resolve', ARB, 1n, WIN, 7000]);
    await post(`/admin/disputes/${dispute}/resolve/build`, { arbiterAddress: ARB, winnerBps: 0, winner: WIN });
    expect(calls.at(-1)).toEqual(['resolve', ARB, 1n, undefined, 0]);
    expect((await db.auditLog.findMany()).map((x) => x.action)).toContain('dispute.resolve.build');
  });
  it('reject needs a note, only from requested, and is audited', async () => {
    const { a, h } = await login(2n, true);
    expect((await a.inject({ method: 'POST', url: `/admin/disputes/${dispute}/reject`, headers: h, payload: {} })).statusCode).toBe(400);
    expect((await a.inject({ method: 'POST', url: `/admin/disputes/${dispute}/reject`, headers: h, payload: { note: 'no merit' } })).json()).toEqual({ ok: true });
    expect((await db.dispute.findUniqueOrThrow({ where: { id: dispute } })).status).toBe('rejected');
    expect((await a.inject({ method: 'POST', url: `/admin/disputes/${dispute}/reject`, headers: h, payload: { note: 'again' } })).statusCode).toBe(409);
    expect(await db.auditLog.count({ where: { action: 'dispute.rejected' } })).toBe(1);
  });
  it('maps chain errors and rejects malformed ids', async () => {
    const { a, h } = await login(2n, true);
    fail = new Error('Error(Contract, #3)');
    const r = await a.inject({ method: 'POST', url: `/admin/disputes/${dispute}/open-onchain`, headers: h, payload: { arbiterAddress: ARB } });
    expect(r.json().error.code).toBe('contract_Unauthorized');
    expect((await a.inject({ method: 'POST', url: '/admin/disputes/NOPE!/reject', headers: h, payload: { note: 'x' } })).statusCode).toBe(404);
  });
});

describe('payouts and status', () => {
  it('retries only failed payouts without a submitted hash', async () => {
    const { a, h } = await login(2n, true);
    const mk = (key: string, over: object) => db.payout.create({ data: { bountyId: 1n, prNumber: 1, winnerGithubId: 9n, status: 'FAILED', idempotencyKey: key, ...over } });
    const ok = await mk('release:1', {});
    expect((await a.inject({ method: 'POST', url: `/admin/payouts/${ok.id}/retry`, headers: h, payload: {} })).json()).toEqual({ ok: true });
    expect((await db.payout.findUniqueOrThrow({ where: { id: ok.id } })).status).toBe('QUEUED');
    expect((await a.inject({ method: 'POST', url: `/admin/payouts/${ok.id}/retry`, headers: h, payload: {} })).statusCode).toBe(409);
    const withHash = await mk('release:2', { txHash: 'H' });
    expect((await a.inject({ method: 'POST', url: `/admin/payouts/${withHash.id}/retry`, headers: h, payload: {} })).json().error.code).toBe('has_tx_hash');
    expect((await a.inject({ url: '/admin/payouts', headers: h })).json()).toHaveLength(1);
  });
  it('reports status counts', async () => {
    const { a, h } = await login(2n, true);
    expect((await a.inject({ url: '/admin/status', headers: h })).json()).toMatchObject({ failedPayouts: 0, openDisputes: 1 });
  });
});
