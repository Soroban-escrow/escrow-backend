import { Keypair } from '@stellar/stellar-sdk';
import { afterAll, beforeEach, describe, expect, it } from 'vitest';
import { sep53Hash } from '../src/auth/wallet.js';
import { createSession } from '../src/auth/session.js';
import { loadConfig } from '../src/config.js';
import { createDb } from '../src/db.js';
import { buildServer } from '../src/server.js';

const db = createDb(process.env.TEST_DATABASE_URL ?? 'postgresql://skillescrow:skillescrow@localhost:5432/skillescrow_test');
afterAll(() => db.$disconnect());
const cfg = loadConfig({ DATABASE_URL: 'x', BOUNTY_CONTRACT_ID: 'CCEKBPYCWKNTXIQLXJHVAF7RYGUZZ6MITA7VVJBOK5BHXUO44ELZ4F4V', SESSION_SECRET: 's'.repeat(32), CSRF_SECRET: 'c'.repeat(32), GITHUB_WEBHOOK_SECRET: 'hooksecret', LOG_LEVEL: 'silent' });

let linked: bigint[];
const app = async () =>
  buildServer(cfg, { insert: async () => true, enqueue: async () => {} }, { db, csrfSecret: cfg.CSRF_SECRET, onWalletLinked: async (g) => void linked.push(g) });

async function login(login = 'alice', githubId = 1n) {
  const u = await db.user.create({ data: { githubId, githubLogin: login } });
  const s = await createSession(db, u.id);
  const cookie = `se_session=${s.id}`;
  return { u, cookie };
}
const csrf = async (a: Awaited<ReturnType<typeof app>>, cookie: string) =>
  (await a.inject({ url: '/auth/me', headers: { cookie } })).json().csrfToken as string;
const bounty = (id: bigint, over: Record<string, unknown> = {}) =>
  db.bounty.create({ data: { id, repoId: 100n, issueNumber: Number(id), issueTitle: `Issue ${id}`, issueUrl: '', criteria: [], maintainer: 'GM', token: 'C', amount: String(Number(id) * 1000), feeBps: 300, deadline: new Date(2030, 0, Number(id)), relayerAllowed: true, status: 'OPEN', fundTx: 't', createdAt: new Date(), ...over } });

beforeEach(async () => {
  linked = [];
  await db.$executeRawUnsafe('TRUNCATE "Payout","PullRequestBounty","PullRequest","Bounty","Repo","Installation","WalletChallenge","Session","User" CASCADE');
  await db.installation.create({ data: { id: 1n, accountLogin: 'o', accountType: 'User' } });
  await db.repo.create({ data: { id: 100n, installationId: 1n, fullName: 'o/Repo', private: false } });
});

describe('auth and CSRF', () => {
  it('401 without a session; invalid ids rejected', async () => {
    const a = await app();
    expect((await a.inject('/auth/me')).statusCode).toBe(401);
    expect((await a.inject({ url: '/auth/me', headers: { cookie: 'se_session=nothex' } })).statusCode).toBe(401);
  });
  it('returns the profile and a CSRF token; expired sessions are refused', async () => {
    const a = await app();
    const { u, cookie } = await login();
    const me = (await a.inject({ url: '/auth/me', headers: { cookie } })).json();
    expect(me.login).toBe('alice');
    expect(me.csrfToken).toMatch(/^[0-9a-f]{64}$/);
    await db.session.updateMany({ where: { userId: u.id }, data: { expiresAt: new Date(0) } });
    expect((await a.inject({ url: '/auth/me', headers: { cookie } })).statusCode).toBe(401);
  });
  it('mutations need the CSRF token', async () => {
    const a = await app();
    const { cookie } = await login();
    const body = { address: Keypair.random().publicKey() };
    expect((await a.inject({ method: 'POST', url: '/wallet/challenge', headers: { cookie }, payload: body })).statusCode).toBe(403);
    expect((await a.inject({ method: 'POST', url: '/wallet/challenge', headers: { cookie, 'x-csrf-token': 'bad' }, payload: body })).statusCode).toBe(403);
    expect((await a.inject({ method: 'DELETE', url: '/wallet', headers: { cookie } })).statusCode).toBe(403);
    expect((await a.inject({ method: 'POST', url: '/auth/logout', headers: { cookie } })).statusCode).toBe(403);
  });
  it('logout destroys the session', async () => {
    const a = await app();
    const { cookie } = await login();
    const t = await csrf(a, cookie);
    expect((await a.inject({ method: 'POST', url: '/auth/logout', headers: { cookie, 'x-csrf-token': t } })).statusCode).toBe(200);
    expect((await a.inject({ url: '/auth/me', headers: { cookie } })).statusCode).toBe(401);
  });
});

describe('wallet over HTTP', () => {
  it('challenge then verify links the wallet and triggers the payout sweep', async () => {
    const a = await app();
    const { u, cookie } = await login();
    const h = { cookie, 'x-csrf-token': await csrf(a, cookie) };
    const kp = Keypair.random();
    const c = (await a.inject({ method: 'POST', url: '/wallet/challenge', headers: h, payload: { address: kp.publicKey() } })).json();
    const sig = kp.sign(sep53Hash(c.message)).toString('base64');
    const v = await a.inject({ method: 'POST', url: '/wallet/verify', headers: h, payload: { challengeId: c.challengeId, signature: sig } });
    expect(v.statusCode).toBe(200);
    expect((await db.user.findUniqueOrThrow({ where: { id: u.id } })).wallet).toBe(kp.publicKey());
    expect(linked).toEqual([1n]);
    const again = await a.inject({ method: 'POST', url: '/wallet/verify', headers: h, payload: { challengeId: c.challengeId, signature: sig } });
    expect(again.statusCode).toBe(409);
    expect(again.json().error.code).toBe('challenge_used');
  });
  it('bad input gives the standard error shape', async () => {
    const a = await app();
    const { cookie } = await login();
    const h = { cookie, 'x-csrf-token': await csrf(a, cookie) };
    const r = await a.inject({ method: 'POST', url: '/wallet/challenge', headers: h, payload: { address: 'nope' } });
    expect(r.statusCode).toBe(400);
    expect(r.json()).toEqual({ error: { code: 'bad_address', message: 'invalid Stellar address' } });
    expect((await a.inject({ method: 'POST', url: '/wallet/verify', headers: h, payload: {} })).statusCode).toBe(400);
  });
});

describe('public reads', () => {
  it('lists open bounties with sorting, filters and cursor paging', async () => {
    const a = await app();
    for (const i of [1n, 2n, 3n]) await bounty(i);
    await bounty(4n, { status: 'RELEASED' });
    const newest = (await a.inject('/bounties?limit=2')).json();
    expect(newest.items.map((x: { id: string }) => x.id)).toEqual(['3', '2']);
    expect(newest.items[0].repo.fullName).toBe('o/Repo');
    const next = (await a.inject(`/bounties?limit=2&cursor=${newest.nextCursor}`)).json();
    expect(next.items.map((x: { id: string }) => x.id)).toEqual(['1']);
    expect(next.nextCursor).toBeNull();
    expect((await a.inject('/bounties?sort=highest')).json().items[0].id).toBe('3');
    expect((await a.inject('/bounties?sort=ending')).json().items[0].id).toBe('1');
    expect((await a.inject('/bounties?minAmount=2500')).json().items).toHaveLength(1);
    expect((await a.inject('/bounties?q=issue%202')).json().items).toHaveLength(1);
    expect((await a.inject('/bounties?status=RELEASED')).json().items).toHaveLength(1);
    expect((await a.inject('/bounties?status=bogus')).statusCode).toBe(400);
  });
  it('bounty detail: 404s and a 200 with PRs and payouts', async () => {
    const a = await app();
    await bounty(1n);
    expect((await a.inject('/bounties/999')).statusCode).toBe(404);
    expect((await a.inject('/bounties/abc')).statusCode).toBe(404);
    const r = await a.inject('/bounties/1');
    expect(r.statusCode).toBe(200);
    expect(r.json().amount).toBe('1000');
  });
  it('earnings are private; the public profile shows only confirmed payouts', async () => {
    const a = await app();
    const { u, cookie } = await login();
    await bounty(1n); await bounty(2n);
    await db.payout.create({ data: { bountyId: 1n, prNumber: 1, winnerGithubId: u.githubId, status: 'CONFIRMED', idempotencyKey: 'release:1', txHash: 'H' } });
    await db.payout.create({ data: { bountyId: 2n, prNumber: 1, winnerGithubId: u.githubId, status: 'QUEUED', idempotencyKey: 'release:2' } });
    expect((await a.inject('/me/earnings')).statusCode).toBe(401);
    expect((await a.inject({ url: '/me/earnings', headers: { cookie } })).json()).toHaveLength(2);
    const pub = (await a.inject('/users/ALICE')).json();
    expect(pub.paid).toHaveLength(1);
    expect(pub.paid[0].txHash).toBe('H');
    expect((await a.inject('/users/nobody')).statusCode).toBe(404);
  });
});

describe('per-user rate limits', () => {
  it('limits one user without affecting another', async () => {
    const a = await app();
    const one = await login('rl1', 11n);
    const two = await login('rl2', 12n);
    const h1 = { cookie: one.cookie, 'x-csrf-token': await csrf(a, one.cookie) };
    const h2 = { cookie: two.cookie, 'x-csrf-token': await csrf(a, two.cookie) };
    const hit = (h: object) => a.inject({ method: 'POST', url: '/wallet/challenge', headers: h as Record<string, string>, payload: { address: 'nope' } });
    const codes: number[] = [];
    for (let i = 0; i < 22; i++) codes.push((await hit(h1)).statusCode);
    expect(codes.slice(0, 20).every((c) => c === 400)).toBe(true);
    expect(codes.slice(20)).toEqual([429, 429]);
    expect((await hit(h2)).statusCode).toBe(400); // same IP, different user: not limited
  });
});
