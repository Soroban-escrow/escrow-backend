import { createVerify, generateKeyPairSync } from 'node:crypto';
import { describe, expect, it } from 'vitest';
import { createGithubClient, GithubError } from '../src/github/client.js';

const { publicKey, privateKey } = generateKeyPairSync('rsa', { modulusLength: 2048 });
const PEM = privateKey.export({ type: 'pkcs1', format: 'pem' }).toString();
const pub = publicKey;

interface Call { method: string; path: string; auth: string; body?: unknown }
function fakeGithub(opts: { tokenTtlMs?: number; rejectFirstUse?: boolean } = {}) {
  const calls: Call[] = [];
  let issued = 0; let clock = 1_800_000_000_000; let rejected = false;
  const routes: Record<string, (c: Call) => { status: number; json?: unknown }> = {};
  const fetchImpl = (async (url: string, init: { method?: string; headers: Record<string, string>; body?: string }) => {
    const u = new URL(url); const method = init.method ?? 'GET'; const auth = init.headers.authorization ?? '';
    const call: Call = { method, path: u.pathname + u.search, auth, ...(init.body ? { body: JSON.parse(init.body) } : {}) };
    calls.push(call);
    let out: { status: number; json?: unknown };
    if (u.pathname === '/app/installations/9/access_tokens') {
      // verify the app JWT: RS256 signature, issuer, lifetime
      const [h, p, s] = auth.replace('Bearer ', '').split('.');
      const ok = createVerify('RSA-SHA256').update(`${h}.${p}`).verify(pub, s!, 'base64url');
      const claims = JSON.parse(Buffer.from(p!, 'base64url').toString());
      if (!ok || claims.iss !== '123' || claims.exp - claims.iat > 600) out = { status: 401 };
      else out = { status: 201, json: { token: `tok${++issued}`, expires_at: new Date(clock + (opts.tokenTtlMs ?? 3_600_000)).toISOString() } };
    } else if (opts.rejectFirstUse && !rejected && auth === 'Bearer tok1') { rejected = true; out = { status: 401 }; }
    else out = (routes[`${method} ${u.pathname}`] ?? (() => ({ status: 404 })))(call);
    return { ok: out.status < 300, status: out.status, json: async () => out.json } as unknown as Response;
  }) as unknown as typeof fetch;
  return { calls, routes, fetchImpl, advance: (ms: number) => { clock += ms; }, now: () => clock };
}
const make = (g: ReturnType<typeof fakeGithub>) => createGithubClient({ appId: '123', privateKey: PEM, fetchImpl: g.fetchImpl, now: g.now });

describe('installation tokens', () => {
  it('signs a valid app JWT, caches the token, and refreshes it near expiry', async () => {
    const g = fakeGithub({ tokenTtlMs: 600_000 });
    g.routes['GET /repos/o/r/collaborators/alice/permission'] = () => ({ status: 200, json: { permission: 'write' } });
    const c = make(g);
    expect(await c.getPermission(9n, 'o/r', 'alice')).toBe('write');
    await c.getPermission(9n, 'o/r', 'alice');
    expect(g.calls.filter((x) => x.path.includes('access_tokens'))).toHaveLength(1);
    g.advance(560_000); // inside the 60 s safety margin
    await c.getPermission(9n, 'o/r', 'alice');
    expect(g.calls.filter((x) => x.path.includes('access_tokens'))).toHaveLength(2);
    expect(g.calls.at(-1)!.auth).toBe('Bearer tok2');
  });
  it('retries once with a fresh token after a 401', async () => {
    const g = fakeGithub({ rejectFirstUse: true });
    g.routes['GET /repos/o/r/collaborators/alice/permission'] = () => ({ status: 200, json: { permission: 'admin' } });
    expect(await make(g).getPermission(9n, 'o/r', 'alice')).toBe('admin');
    expect(g.calls.filter((x) => x.path.includes('access_tokens'))).toHaveLength(2);
  });
  it('fails clearly when the installation token cannot be issued', async () => {
    const g = fakeGithub();
    const c = createGithubClient({ appId: '999', privateKey: PEM, fetchImpl: g.fetchImpl, now: g.now }); // wrong app id: fake rejects the JWT
    await expect(c.getPermission(9n, 'o/r', 'alice')).rejects.toBeInstanceOf(GithubError);
  });
  it('accepts a private key with literal \\n sequences (as stored in .env)', async () => {
    const g = fakeGithub();
    g.routes['GET /repos/o/r/collaborators/a/permission'] = () => ({ status: 200, json: { permission: 'read' } });
    const c = createGithubClient({ appId: '123', privateKey: PEM.trim().replace(/\n/g, '\\n'), fetchImpl: g.fetchImpl, now: g.now });
    expect(await c.getPermission(9n, 'o/r', 'a')).toBe('read');
  });
});

describe('calls', () => {
  it('permission 404 means none; other errors throw', async () => {
    const g = fakeGithub();
    g.routes['GET /repos/o/r/collaborators/stranger/permission'] = () => ({ status: 404 });
    g.routes['GET /repos/o/r/collaborators/boom/permission'] = () => ({ status: 500 });
    const c = make(g);
    expect(await c.getPermission(9n, 'o/r', 'stranger')).toBe('none');
    await expect(c.getPermission(9n, 'o/r', 'boom')).rejects.toThrow(/500/);
  });
  it('posts and updates comments', async () => {
    const g = fakeGithub();
    g.routes['POST /repos/o/r/issues/7/comments'] = (c) => ({ status: 201, json: { id: 4242, body: (c.body as { body: string }).body } });
    g.routes['PATCH /repos/o/r/issues/comments/4242'] = () => ({ status: 200, json: {} });
    const c = make(g);
    expect(await c.postComment(9n, 'o/r', 7, 'hello')).toBe(4242n);
    await c.updateComment(9n, 'o/r', 4242n, 'edited');
    expect(g.calls.at(-1)!.body).toEqual({ body: 'edited' });
  });
  it('sets the bounty label: removes older ones, tolerates an existing label, then applies', async () => {
    const g = fakeGithub();
    g.routes['GET /repos/o/r/issues/7/labels'] = () => ({ status: 200, json: [{ name: 'bounty: 10 USDC' }, { name: 'bug' }] });
    g.routes['DELETE /repos/o/r/issues/7/labels/bounty%3A%2010%20USDC'] = () => ({ status: 200, json: [] });
    g.routes['POST /repos/o/r/labels'] = () => ({ status: 422 });
    g.routes['POST /repos/o/r/issues/7/labels'] = () => ({ status: 200, json: [] });
    await make(g).setBountyLabel(9n, 'o/r', 7, 'bounty: 50 USDC');
    expect(g.calls.some((x) => x.method === 'DELETE' && x.path.endsWith('bounty%3A%2010%20USDC'))).toBe(true);
    expect(g.calls.at(-1)!.body).toEqual({ labels: ['bounty: 50 USDC'] });
  });
  it('reads PR files across pages and summarises CI', async () => {
    const g = fakeGithub();
    const page = (n: number) => Array.from({ length: n }, (_, i) => ({ filename: `f${i}`, additions: 1, deletions: 0, patch: '+x' }));
    g.routes['GET /repos/o/r/pulls/3/files'] = (c) => ({ status: 200, json: new URLSearchParams(c.path.split('?')[1]).get('page') === '1' ? page(100) : page(5) });
    expect(await make(g).getPullFiles(9n, 'o/r', 3)).toHaveLength(105);
    const ci = (runs: object[]) => { g.routes['GET /repos/o/r/commits/abc1234/check-runs'] = () => ({ status: 200, json: { check_runs: runs } }); return make(g).getCiConclusion(9n, 'o/r', 'abc1234'); };
    expect(await ci([])).toBe('none');
    expect(await ci([{ status: 'completed', conclusion: 'success' }, { status: 'completed', conclusion: 'skipped' }])).toBe('success');
    expect(await ci([{ status: 'in_progress', conclusion: null }])).toBe('pending');
    expect(await ci([{ status: 'completed', conclusion: 'success' }, { status: 'completed', conclusion: 'failure' }])).toBe('failure');
  });
});

describe('input validation (no request is made)', () => {
  it('refuses path-injection attempts', async () => {
    const g = fakeGithub(); const c = make(g);
    await expect(c.getPermission(9n, 'o/r/../../x', 'a')).rejects.toThrow(/repository/);
    await expect(c.getPermission(9n, 'o/r', '../admin')).rejects.toThrow(/login/);
    await expect(c.postComment(9n, 'o/r', -1, 'x')).rejects.toThrow(/issue/);
    await expect(c.postComment(9n, 'o/r?x=1', 1, 'x')).rejects.toThrow(/repository/);
    await expect(c.getCiConclusion(9n, 'o/r', 'abc/../x')).rejects.toThrow(/sha/);
    await expect(c.setBountyLabel(9n, 'o/r', 1, 'evil; label')).rejects.toThrow(/label/);
    expect(g.calls).toHaveLength(0);
  });
});
