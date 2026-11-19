import { createHash } from 'node:crypto';
import { afterAll, beforeEach, describe, expect, it } from 'vitest';
import { finishLogin, LoginError, safeNext, startLogin, type LoginDeps } from '../src/auth/github-login.js';
import { createDb } from '../src/db.js';
import { loadConfig } from '../src/config.js';
import { buildServer } from '../src/server.js';

const db = createDb(process.env.TEST_DATABASE_URL ?? 'postgresql://skillescrow:skillescrow@localhost:5432/skillescrow_test');
afterAll(() => db.$disconnect());

let exchanged: Record<string, string>[]; let ghUser: object | null; let tokenReply: object;
const fetchImpl = (async (url: string, init: { body?: string }) => {
  if (url.includes('/login/oauth/access_token')) { exchanged.push(JSON.parse(init.body!)); return { ok: true, json: async () => tokenReply } as Response; }
  if (url === 'https://api.github.com/user') return ghUser ? ({ ok: true, json: async () => ghUser } as Response) : ({ ok: false, status: 500, json: async () => ({}) } as Response);
  throw new Error(`unexpected fetch ${url}`);
}) as unknown as typeof fetch;

let clock = 1_800_000_000_000;
const deps = (over: Partial<LoginDeps> = {}): LoginDeps => ({
  db, clientId: 'Iv1.test', clientSecret: 'shh', callbackUrl: 'http://localhost:3001/auth/callback', webUrl: 'http://localhost:3000',
  secret: 's'.repeat(32), adminGithubIds: [], fetchImpl, now: () => clock, ...over,
});
const code = async (p: Promise<unknown>) => p.then(() => 'ok', (e: LoginError) => e.code);

beforeEach(async () => {
  exchanged = []; clock = 1_800_000_000_000;
  ghUser = { id: 55, login: 'maint', avatar_url: 'https://a/b.png' }; tokenReply = { access_token: 'gho_x' };
  await db.$executeRawUnsafe('TRUNCATE "Session","User" CASCADE');
});

describe('login flow', () => {
  it('sends PKCE and state to GitHub, then completes with the matching verifier', async () => {
    const d = deps();
    const { url, cookie } = startLogin(d, '/fund/abc');
    const u = new URL(url);
    expect(u.origin + u.pathname).toBe('https://github.com/login/oauth/authorize');
    expect(u.searchParams.get('client_id')).toBe('Iv1.test');
    expect(u.searchParams.get('redirect_uri')).toBe('http://localhost:3001/auth/callback');
    expect(u.searchParams.get('code_challenge_method')).toBe('S256');
    const r = await finishLogin(d, { code: 'c0de', state: u.searchParams.get('state')! }, cookie);
    // the verifier we sent hashes to the challenge GitHub was given
    expect(createHash('sha256').update(exchanged[0]!.code_verifier!).digest('base64url')).toBe(u.searchParams.get('code_challenge'));
    expect(exchanged[0]).toMatchObject({ client_secret: 'shh', code: 'c0de', redirect_uri: 'http://localhost:3001/auth/callback' });
    expect(r.redirect).toBe('http://localhost:3000/fund/abc');
    const user = await db.user.findUniqueOrThrow({ where: { githubId: 55n } });
    expect(user.githubLogin).toBe('maint');
    expect(user.isAdmin).toBe(false);
    expect(await db.session.count({ where: { userId: user.id } })).toBe(1);
  });
  it('the admin flag follows config on every login', async () => {
    const d = deps({ adminGithubIds: [55n] });
    let { url, cookie } = startLogin(d, '/admin');
    await finishLogin(d, { code: 'c', state: new URL(url).searchParams.get('state')! }, cookie);
    expect((await db.user.findUniqueOrThrow({ where: { githubId: 55n } })).isAdmin).toBe(true);
    const d2 = deps({ adminGithubIds: [] });
    ({ url, cookie } = startLogin(d2, '/admin'));
    await finishLogin(d2, { code: 'c', state: new URL(url).searchParams.get('state')! }, cookie);
    expect((await db.user.findUniqueOrThrow({ where: { githubId: 55n } })).isAdmin).toBe(false);
  });
  it('rejects a missing cookie, a wrong state, a forged cookie and an expired one', async () => {
    const d = deps();
    const { url, cookie } = startLogin(d, '/me');
    const state = new URL(url).searchParams.get('state')!;
    expect(await code(finishLogin(d, { code: 'c', state }, undefined))).toBe('bad_request');
    expect(await code(finishLogin(d, { code: 'c', state: 'other' }, cookie))).toBe('bad_state');
    expect(await code(finishLogin(d, { code: 'c', state }, cookie.slice(0, -2) + 'xx'))).toBe('bad_state');
    expect(await code(finishLogin(deps({ secret: 'z'.repeat(32) }), { code: 'c', state }, cookie))).toBe('bad_state'); // signed with another key
    clock += 11 * 60_000;
    expect(await code(finishLogin(d, { code: 'c', state }, cookie))).toBe('state_expired');
    expect(exchanged).toHaveLength(0); // never reached GitHub
    expect(await db.session.count()).toBe(0);
  });
  it('creates no session when GitHub rejects the code or the profile is unavailable', async () => {
    const d = deps();
    const { url, cookie } = startLogin(d, '/me'); const state = new URL(url).searchParams.get('state')!;
    tokenReply = { error: 'bad_verification_code' };
    expect(await code(finishLogin(d, { code: 'c', state }, cookie))).toBe('github_rejected');
    tokenReply = { access_token: 'x' }; ghUser = null;
    expect(await code(finishLogin(d, { code: 'c', state }, cookie))).toBe('github_unavailable');
    expect(await db.session.count()).toBe(0);
  });
  it('only ever redirects to a relative path on our own site', () => {
    expect(safeNext('/fund/abc?x=1')).toBe('/fund/abc?x=1');
    for (const bad of ['https://evil.test', '//evil.test', '/\\evil.test', 'javascript:alert(1)', '', undefined, 42, '/' + 'a'.repeat(300), '/a b']) expect(safeNext(bad)).toBe('/me');
  });
});

describe('over HTTP', () => {
  const cfg = loadConfig({ DATABASE_URL: 'x', BOUNTY_CONTRACT_ID: 'CCEKBPYCWKNTXIQLXJHVAF7RYGUZZ6MITA7VVJBOK5BHXUO44ELZ4F4V', SESSION_SECRET: 's'.repeat(32), CSRF_SECRET: 'c'.repeat(32), GITHUB_WEBHOOK_SECRET: 'hooksecret', LOG_LEVEL: 'silent' });
  const app = () => buildServer(cfg, { insert: async () => true, enqueue: async () => {} }, { db, csrfSecret: cfg.CSRF_SECRET, login: deps({ now: undefined as never }) });

  it('/auth/login redirects to GitHub and sets a state cookie; the callback sets the session cookie and redirects home', async () => {
    const a = await app();
    const l = await a.inject('/auth/login?next=/fund/abc');
    expect(l.statusCode).toBe(302);
    expect(l.headers.location).toContain('https://github.com/login/oauth/authorize');
    const stateCookie = String(l.headers['set-cookie']);
    expect(stateCookie).toContain('HttpOnly');
    const state = new URL(String(l.headers.location)).searchParams.get('state')!;
    const cb = await a.inject({ url: `/auth/callback?code=c0de&state=${state}`, headers: { cookie: stateCookie.split(';')[0]! } });
    expect(cb.statusCode).toBe(302);
    expect(cb.headers.location).toBe('http://localhost:3000/fund/abc');
    const cookies = ([] as string[]).concat(cb.headers['set-cookie'] as string[]);
    const session = cookies.find((c) => c.startsWith('se_session='))!;
    expect(session).toMatch(/HttpOnly; SameSite=Lax/);
    const me = await a.inject({ url: '/auth/me', headers: { cookie: session.split(';')[0]! } });
    expect(me.json().login).toBe('maint');
  });
  it('a callback without our cookie is refused (login CSRF)', async () => {
    const a = await app();
    const r = await a.inject('/auth/callback?code=c&state=anything');
    expect(r.statusCode).toBe(400);
    expect(r.json().error.code).toBe('bad_request');
  });
});
