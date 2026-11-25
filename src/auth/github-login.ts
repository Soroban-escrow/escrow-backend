import { createHash, createHmac, randomBytes, timingSafeEqual } from 'node:crypto';
import type { Db } from '../db.js';
import { createSession, COOKIE } from './session.js';

export const OAUTH_COOKIE = 'se_oauth';
const TTL_MS = 10 * 60 * 1000;

export interface LoginDeps {
  db: Db;
  clientId: string;
  clientSecret: string;
  /** where GitHub sends the browser back: must equal the App's registered callback URL */
  callbackUrl: string;
  webUrl: string;
  /** signing key for the state cookie */
  secret: string;
  adminGithubIds: bigint[];
  fetchImpl?: typeof fetch;
  now?: () => number;
}

const b64u = (b: Buffer) => b.toString('base64url');
const sign = (secret: string, payload: string) => createHmac('sha256', secret).update(payload).digest('base64url');
const eq = (a: string, b: string) => { const x = Buffer.from(a), y = Buffer.from(b); return x.length === y.length && timingSafeEqual(x, y); };

/** Only same-site relative paths: never an absolute or protocol-relative URL (open redirect). */
export function safeNext(next: unknown): string {
  return typeof next === 'string' && /^\/(?![/\\])[\w\-./?=&%~]*$/.test(next) && next.length <= 200 ? next : '/me';
}

/** Step 1: the GitHub authorize URL plus the signed cookie that binds the flow to this browser. */
export function startLogin(d: LoginDeps, next: unknown) {
  const state = b64u(randomBytes(24));
  const verifier = b64u(randomBytes(48));
  const exp = (d.now ?? Date.now)() + TTL_MS;
  const payload = b64u(Buffer.from(JSON.stringify({ state, verifier, exp, next: safeNext(next) })));
  const url = new URL('https://github.com/login/oauth/authorize');
  url.search = new URLSearchParams({
    client_id: d.clientId, redirect_uri: d.callbackUrl, state,
    code_challenge: createHash('sha256').update(verifier).digest('base64url'), code_challenge_method: 'S256',
  }).toString();
  return { url: url.toString(), cookie: `${payload}.${sign(d.secret, payload)}` };
}

export class LoginError extends Error {
  constructor(public status: number, public code: string, message: string) { super(message); }
}

/** Step 2: verify the state cookie, exchange the code (with the PKCE verifier), upsert the user, open a session. */
export async function finishLogin(d: LoginDeps, q: { code?: string; state?: string }, cookie: string | undefined) {
  if (!cookie || !q.code || !q.state) throw new LoginError(400, 'bad_request', 'missing login state');
  const [payload, mac] = cookie.split('.');
  if (!payload || !mac || !eq(mac, sign(d.secret, payload))) throw new LoginError(400, 'bad_state', 'login state is invalid');
  let st: { state: string; verifier: string; exp: number; next: string };
  try { st = JSON.parse(Buffer.from(payload, 'base64url').toString('utf8')); } catch { throw new LoginError(400, 'bad_state', 'login state is invalid'); }
  if (st.exp <= (d.now ?? Date.now)()) throw new LoginError(400, 'state_expired', 'login took too long, please try again');
  if (!eq(st.state, q.state)) throw new LoginError(400, 'bad_state', 'login state does not match');

  const f = d.fetchImpl ?? fetch;
  const tr = await f('https://github.com/login/oauth/access_token', {
    method: 'POST', headers: { accept: 'application/json', 'content-type': 'application/json' },
    body: JSON.stringify({ client_id: d.clientId, client_secret: d.clientSecret, code: q.code, redirect_uri: d.callbackUrl, code_verifier: st.verifier }),
  });
  const tj = (await tr.json().catch(() => ({}))) as { access_token?: string; error?: string };
  if (!tr.ok || !tj.access_token) throw new LoginError(400, 'github_rejected', `GitHub rejected the login${tj.error ? ` (${tj.error})` : ''}`);

  const ur = await f('https://api.github.com/user', { headers: { authorization: `Bearer ${tj.access_token}`, accept: 'application/vnd.github+json', 'user-agent': 'skillescrow' } });
  if (!ur.ok) throw new LoginError(502, 'github_unavailable', 'could not read your GitHub profile');
  const u = (await ur.json()) as { id: number; login: string; avatar_url?: string };
  if (!Number.isInteger(u.id) || !u.login) throw new LoginError(502, 'github_unavailable', 'unexpected GitHub response');

  const githubId = BigInt(u.id);
  // the admin flag follows config on every login, not what the browser says
  const user = await d.db.user.upsert({
    where: { githubId },
    create: { githubId, githubLogin: u.login, avatarUrl: u.avatar_url ?? null, isAdmin: d.adminGithubIds.includes(githubId) },
    update: { githubLogin: u.login, avatarUrl: u.avatar_url ?? null, isAdmin: d.adminGithubIds.includes(githubId) },
  });
  const session = await createSession(d.db, user.id, new Date((d.now ?? Date.now)()));
  return { sessionId: session.id, maxAgeSeconds: session.maxAgeSeconds, redirect: `${d.webUrl}${safeNext(st.next)}` };
}

export const sessionCookie = (id: string, maxAge: number, secure: boolean) =>
  `${COOKIE}=${id}; Path=/; Max-Age=${maxAge}; HttpOnly; SameSite=Lax${secure ? '; Secure' : ''}`;
export const stateCookie = (value: string, secure: boolean) =>
  `${OAUTH_COOKIE}=${value}; Path=/; Max-Age=${TTL_MS / 1000}; HttpOnly; SameSite=Lax${secure ? '; Secure' : ''}`;
export const clearStateCookie = `${OAUTH_COOKIE}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax`;
