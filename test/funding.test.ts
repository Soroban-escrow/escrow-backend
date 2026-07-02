import { Keypair } from '@stellar/stellar-sdk';
import { afterAll, beforeEach, describe, expect, it } from 'vitest';
import { createSession } from '../src/auth/session.js';
import { loadConfig } from '../src/config.js';
import { createDb } from '../src/db.js';
import type { FundingChain } from '../src/routes/funding.js';
import { buildServer } from '../src/server.js';

const db = createDb(process.env.TEST_DATABASE_URL ?? 'postgresql://skillescrow:skillescrow@localhost:5432/skillescrow_test');
afterAll(() => db.$disconnect());
const cfg = loadConfig({ DATABASE_URL: 'x', BOUNTY_CONTRACT_ID: 'CCEKBPYCWKNTXIQLXJHVAF7RYGUZZ6MITA7VVJBOK5BHXUO44ELZ4F4V', SESSION_SECRET: 's'.repeat(32), CSRF_SECRET: 'c'.repeat(32), GITHUB_WEBHOOK_SECRET: 'hooksecret', LOG_LEVEL: 'silent' });

const MAINT = Keypair.random().publicKey();
const WINNER = Keypair.random().publicKey();
let calls: Record<string, unknown[]>;
let chainStatus = 'Open';
let fail: Error | null = null;
const chain: FundingChain = {
  buildFundTx: async (p) => { if (fail) throw fail; (calls.fund ??= []).push(p); return 'FUNDXDR'; },
  buildReleaseTx: async (...a) => { if (fail) throw fail; (calls.release ??= []).push(a); return 'RELXDR'; },
  buildRefundTx: async (...a) => { if (fail) throw fail; (calls.refund ??= []).push(a); return 'REFXDR'; },
  submitSigned: async (x, s) => { if (x === 'BADXDRBADXDR') throw new Error('not a contract call'); (calls.submit ??= []).push([x, s]); return 'HASH'; },
  getBounty: async () => ({ status: chainStatus }),
};
const app = () => buildServer(cfg, { insert: async () => true, enqueue: async () => {} }, { db, csrfSecret: cfg.CSRF_SECRET, chain });

async function login(githubId: bigint, wallet: string | null = null) {
  const u = await db.user.create({ data: { githubId, githubLogin: `u${githubId}`, wallet } });
  const s = await createSession(db, u.id);
  const cookie = `se_session=${s.id}`;
  const a = await app();
  const csrfToken = (await a.inject({ url: '/auth/me', headers: { cookie } })).json().csrfToken as string;
  return { a, h: { cookie, 'x-csrf-token': csrfToken } };
}
const intent = (over: Record<string, unknown> = {}) =>
  db.fundingIntent.create({ data: { id: 'intent-abcdefghij', repoId: 100n, issueNumber: 7, requestedBy: 1n, token: 'CTOK', amount: '500000000', durationDays: 14, commentId: 1n, expiresAt: new Date(Date.now() + 3600e3), ...over } });
const post = (a: Awaited<ReturnType<typeof app>>, url: string, h: object, payload: unknown) => a.inject({ method: 'POST', url, headers: h as Record<string, string>, payload: payload as object });

beforeEach(async () => {
  calls = {}; chainStatus = 'Open'; fail = null;
  await db.$executeRawUnsafe('TRUNCATE "Payout","PullRequest","Bounty","FundingIntent","Repo","Installation","WalletChallenge","Session","User" CASCADE');
  await db.installation.create({ data: { id: 1n, accountLogin: 'o', accountType: 'User' } });
  await db.repo.create({ data: { id: 100n, installationId: 1n, fullName: 'o/r', private: false } });
});

describe('GET /intents/:id', () => {
  it('shows open, expired and used states; 404 for junk', async () => {
    const a = await app();
    await intent();
    expect((await a.inject('/intents/intent-abcdefghij')).json()).toMatchObject({ state: 'open', repo: 'o/r', amount: '500000000', durationDays: 14 });
    await db.fundingIntent.update({ where: { id: 'intent-abcdefghij' }, data: { expiresAt: new Date(0) } });
    expect((await a.inject('/intents/intent-abcdefghij')).json().state).toBe('expired');
    await db.fundingIntent.update({ where: { id: 'intent-abcdefghij' }, data: { usedAt: new Date() } });
    expect((await a.inject('/intents/intent-abcdefghij')).json().state).toBe('used');
    expect((await a.inject('/intents/nope')).statusCode).toBe(404);
    expect((await a.inject('/intents/unknown-intent-id-xx')).statusCode).toBe(404);
  });
});

describe('POST /intents/:id/build', () => {
  it('builds for the requester with the intent terms, not client-supplied ones', async () => {
    await intent();
    const { a, h } = await login(1n);
    const r = await post(a, '/intents/intent-abcdefghij/build', h, { address: MAINT, relayerAllowed: true, amount: '1', token: 'EVIL' });
    expect(r.json()).toEqual({ xdr: 'FUNDXDR' });
    const p = calls.fund![0] as { amount: bigint; token: string; issue: bigint; relayerAllowed: boolean; deadline: bigint };
    expect(p.amount).toBe(500000000n);
    expect(p.token).toBe('CTOK');
    expect(p.issue).toBe(7n);
    expect(p.relayerAllowed).toBe(true);
    expect(Number(p.deadline)).toBeGreaterThan(Date.now() / 1000 + 14 * 86400);
  });
  it('refuses other users, bad addresses, missing CSRF, used and expired links', async () => {
    await intent();
    const other = await login(2n);
    expect((await post(other.a, '/intents/intent-abcdefghij/build', other.h, { address: MAINT, relayerAllowed: true })).json().error.code).toBe('not_requester');
    const { a, h } = await login(1n);
    expect((await post(a, '/intents/intent-abcdefghij/build', h, { address: 'GBAD', relayerAllowed: true })).statusCode).toBe(400);
    expect((await a.inject({ method: 'POST', url: '/intents/intent-abcdefghij/build', headers: { cookie: h.cookie }, payload: { address: MAINT, relayerAllowed: true } })).statusCode).toBe(403);
    await db.fundingIntent.update({ where: { id: 'intent-abcdefghij' }, data: { expiresAt: new Date(0) } });
    expect((await post(a, '/intents/intent-abcdefghij/build', h, { address: MAINT, relayerAllowed: true })).statusCode).toBe(410);
    expect(calls.fund).toBeUndefined();
  });
  it('maps contract errors to stable codes', async () => {
    await intent();
    const { a, h } = await login(1n);
    fail = new Error('simulation failed: HostError: Error(Contract, #5)');
    const r = await post(a, '/intents/intent-abcdefghij/build', h, { address: MAINT, relayerAllowed: true });
    expect(r.statusCode).toBe(422);
    expect(r.json().error.code).toBe('contract_TokenNotAllowed');
  });
});

describe('POST /tx/submit', () => {
  it('relays valid transactions and surfaces rejections', async () => {
    const { a, h } = await login(1n);
    expect((await post(a, '/tx/submit', h, { xdr: 'AAAAAAAAAAAAAAA', address: MAINT })).json()).toEqual({ hash: 'HASH' });
    expect((await post(a, '/tx/submit', h, { xdr: 'BADXDRBADXDR' })).statusCode).toBe(422);
    expect((await post(a, '/tx/submit', h, {})).statusCode).toBe(400);
    expect((await a.inject({ method: 'POST', url: '/tx/submit', payload: { xdr: 'AAAAAAAAAAAAAAA' } })).statusCode).toBe(401);
  });
});

describe('maintainer release and refund', () => {
  beforeEach(async () => {
    await db.bounty.create({ data: { id: 1n, repoId: 100n, issueNumber: 7, issueTitle: '', issueUrl: '', criteria: [], maintainer: MAINT, token: 'C', amount: '1000', feeBps: 300, deadline: new Date(), relayerAllowed: false, status: 'OPEN', fundTx: 't', createdAt: new Date() } });
  });
  it('release: only the maintainer wallet, only when awaiting, only when open on chain', async () => {
    await db.payout.create({ data: { bountyId: 1n, prNumber: 3, winnerGithubId: 9n, winnerAddress: WINNER, status: 'AWAITING_MAINTAINER', idempotencyKey: 'release:1' } });
    const stranger = await login(5n, Keypair.random().publicKey());
    expect((await post(stranger.a, '/bounties/1/release/build', stranger.h, {})).json().error.code).toBe('not_maintainer');
    const { a, h } = await login(6n, MAINT);
    expect((await post(a, '/bounties/1/release/build', h, {})).json()).toEqual({ xdr: 'RELXDR', winner: WINNER });
    expect(calls.release![0]).toEqual([MAINT, 1n, WINNER]);
    chainStatus = 'Released';
    expect((await post(a, '/bounties/1/release/build', h, {})).json().error.code).toBe('not_open');
    chainStatus = 'Open';
    await db.payout.update({ where: { idempotencyKey: 'release:1' }, data: { status: 'QUEUED' } });
    expect((await post(a, '/bounties/1/release/build', h, {})).json().error.code).toBe('nothing_to_release');
  });
  it('refund: maintainer only, chain re-read, contract enforces the window', async () => {
    const { a, h } = await login(6n, MAINT);
    expect((await post(a, '/bounties/1/refund/build', h, {})).json()).toEqual({ xdr: 'REFXDR' });
    fail = new Error('Error(Contract, #12)');
    expect((await post(a, '/bounties/1/refund/build', h, {})).json().error.code).toBe('contract_DeadlineNotReached');
    const nobody = await login(7n);
    expect((await post(nobody.a, '/bounties/1/refund/build', nobody.h, {})).statusCode).toBe(403);
    expect((await post(a, '/bounties/999/refund/build', h, {})).statusCode).toBe(404);
  });
});
