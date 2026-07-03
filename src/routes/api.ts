import type { FastifyInstance, FastifyReply, FastifyRequest } from 'fastify';
import { z } from 'zod';
import { createChallenge, unlinkWallet, verifyChallenge, WalletError } from '../auth/wallet.js';
import { COOKIE, csrfToken, csrfValid, destroySession, getSessionUser } from '../auth/session.js';
import { clearStateCookie, finishLogin, LoginError, OAUTH_COOKIE, sessionCookie, startLogin, stateCookie, type LoginDeps } from '../auth/github-login.js';
import type { Db } from '../db.js';
import { mutationLimit as limit } from './limits.js';
import { registerAdmin, type AdminChain } from './admin.js';
import { registerFunding, type FundingChain } from './funding.js';

export interface ApiDeps {
  db: Db;
  csrfSecret: string;
  /** called after a wallet is linked (continues pending payouts) */
  onWalletLinked?: (githubId: bigint) => Promise<unknown>;
  /** enables the funding, release and refund routes */
  chain?: FundingChain & AdminChain;
  /** enables GitHub sign-in (/auth/login, /auth/callback) */
  login?: LoginDeps;
}

const err = (r: FastifyReply, status: number, code: string, message: string) => r.code(status).send({ error: { code, message } });
const ser = (v: unknown): unknown => JSON.parse(JSON.stringify(v, (_, x) => (typeof x === 'bigint' ? x.toString() : x)));

type Authed = FastifyRequest & { sessionId?: string; user?: NonNullable<Awaited<ReturnType<typeof getSessionUser>>> };

export function registerApi(app: FastifyInstance, d: ApiDeps) {
  const cookies = (req: FastifyRequest) =>
    Object.fromEntries((req.headers.cookie ?? '').split(';').map((c) => c.trim().split('=') as [string, string]).filter(([k]) => k));

  async function requireUser(req: Authed, reply: FastifyReply, mutating: boolean) {
    const sid = cookies(req)[COOKIE];
    const user = await getSessionUser(d.db, sid);
    if (!user || !sid) return err(reply, 401, 'unauthenticated', 'sign in first');
    if (mutating && !csrfValid(d.csrfSecret, sid, req.headers['x-csrf-token'])) return err(reply, 403, 'csrf', 'missing or invalid CSRF token');
    req.user = user;
    req.sessionId = sid;
    return undefined;
  }
  const authed = (mutating: boolean) => ({
    preHandler: async (req: FastifyRequest, reply: FastifyReply) => requireUser(req as Authed, reply, mutating),
  });
  const mutationLimit = limit(20);

  if (d.login) {
    const login = d.login;
    const secure = login.callbackUrl.startsWith('https://');
    app.get('/auth/login', async (req, reply) => {
      const { url, cookie } = startLogin(login, (req.query as { next?: string }).next);
      return reply.header('set-cookie', stateCookie(cookie, secure)).redirect(url);
    });
    app.get('/auth/callback', { config: { rateLimit: { max: 30, timeWindow: '1 minute' } } }, async (req, reply) => {
      try {
        const r = await finishLogin(login, req.query as { code?: string; state?: string }, cookies(req)[OAUTH_COOKIE]);
        return reply.header('set-cookie', [sessionCookie(r.sessionId, r.maxAgeSeconds, secure), clearStateCookie]).redirect(r.redirect);
      } catch (e) {
        if (e instanceof LoginError) return reply.header('set-cookie', clearStateCookie).code(e.status).send({ error: { code: e.code, message: e.message } });
        throw e;
      }
    });
  }

  app.get('/auth/me', authed(false), async (req) => {
    const u = (req as Authed).user!;
    return { id: u.id, login: u.githubLogin, avatarUrl: u.avatarUrl, wallet: u.wallet, isAdmin: u.isAdmin, csrfToken: csrfToken(d.csrfSecret, (req as Authed).sessionId!) };
  });

  app.post('/auth/logout', authed(true), async (req, reply) => {
    await destroySession(d.db, (req as Authed).sessionId!);
    return reply.header('set-cookie', `${COOKIE}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax`).send({ ok: true });
  });

  const walletBody = z.object({ address: z.string() });
  app.post('/wallet/challenge', { ...authed(true), ...mutationLimit }, async (req, reply) => {
    const body = walletBody.safeParse(req.body);
    if (!body.success) return err(reply, 400, 'bad_request', 'address required');
    try {
      return await createChallenge({ db: d.db }, (req as Authed).user!.id, body.data.address);
    } catch (e) {
      if (e instanceof WalletError) return err(reply, e.status, e.code, e.message);
      throw e;
    }
  });
  const verifyBody = z.object({ challengeId: z.string(), signature: z.string() });
  app.post('/wallet/verify', { ...authed(true), ...mutationLimit }, async (req, reply) => {
    const body = verifyBody.safeParse(req.body);
    if (!body.success) return err(reply, 400, 'bad_request', 'challengeId and signature required');
    const u = (req as Authed).user!;
    try {
      const r = await verifyChallenge({ db: d.db }, u.id, body.data.challengeId, body.data.signature);
      await d.onWalletLinked?.(u.githubId);
      return r;
    } catch (e) {
      if (e instanceof WalletError) return err(reply, e.status, e.code, e.message);
      throw e;
    }
  });
  app.delete('/wallet', authed(true), async (req) => {
    await unlinkWallet({ db: d.db }, (req as Authed).user!.id);
    return { ok: true };
  });

  if (d.chain) {
    registerFunding(app, { db: d.db, chain: d.chain }, authed);
    registerAdmin(app, { db: d.db, chain: d.chain }, authed);
  }

  // ---- public reads ----
  const listQuery = z.object({
    status: z.enum(['OPEN', 'RELEASED', 'REFUNDED', 'DISPUTED', 'RESOLVED']).optional(),
    token: z.string().optional(),
    minAmount: z.string().regex(/^\d+$/).optional(),
    q: z.string().max(100).optional(),
    sort: z.enum(['newest', 'highest', 'ending']).default('newest'),
    cursor: z.string().regex(/^\d{1,18}$/).optional(),
    limit: z.coerce.number().int().min(1).max(50).default(20),
  });
  app.get('/bounties', async (req, reply) => {
    const p = listQuery.safeParse(req.query);
    if (!p.success) return err(reply, 400, 'bad_request', 'invalid query');
    const f = p.data;
    const rows = await d.db.bounty.findMany({
      where: {
        status: f.status ?? 'OPEN',
        ...(f.token ? { token: f.token } : {}),
        ...(f.minAmount ? { amount: { gte: f.minAmount } } : {}),
        ...(f.q ? { OR: [{ issueTitle: { contains: f.q, mode: 'insensitive' as const } }, { repo: { fullName: { contains: f.q, mode: 'insensitive' as const } } }] } : {}),
      },
      orderBy: f.sort === 'highest' ? [{ amount: 'desc' }, { id: 'desc' }] : f.sort === 'ending' ? [{ deadline: 'asc' }, { id: 'desc' }] : [{ id: 'desc' }],
      include: { repo: { select: { fullName: true } }, _count: { select: { prs: true } } },
      take: f.limit + 1,
      ...(f.cursor ? { skip: 1, cursor: { id: BigInt(f.cursor) } } : {}),
    });
    const items = rows.slice(0, f.limit);
    return ser({ items, nextCursor: rows.length > f.limit ? items.at(-1)?.id : null });
  });

  app.get('/bounties/:id', async (req, reply) => {
    const id = (req.params as { id: string }).id;
    if (!/^\d{1,18}$/.test(id)) return err(reply, 404, 'not_found', 'bounty not found');
    const b = await d.db.bounty.findUnique({
      where: { id: BigInt(id) },
      include: { repo: { select: { fullName: true } }, prs: { include: { pr: { include: { reviews: { orderBy: { createdAt: 'desc' }, take: 1 } } } } }, payouts: true },
    });
    if (!b) return err(reply, 404, 'not_found', 'bounty not found');
    return ser({ ...b, prs: b.prs.map((l) => l.pr) }); // the API keeps returning a flat PR list
  });

  app.get('/me/earnings', authed(false), async (req) => {
    const u = (req as Authed).user!;
    return ser(await d.db.payout.findMany({
      where: { winnerGithubId: u.githubId },
      include: { bounty: { select: { token: true, amount: true, feeBps: true, issueTitle: true } } },
      orderBy: { createdAt: 'desc' },
    }));
  });
  app.get('/me/funded', authed(false), async (req) => {
    const u = (req as Authed).user!;
    if (!u.wallet) return [];
    return ser(await d.db.bounty.findMany({ where: { maintainer: u.wallet }, orderBy: { id: 'desc' } }));
  });

  app.get('/users/:login', async (req, reply) => {
    const u = await d.db.user.findFirst({ where: { githubLogin: { equals: (req.params as { login: string }).login, mode: 'insensitive' } } });
    if (!u) return err(reply, 404, 'not_found', 'user not found');
    const paid = await d.db.payout.findMany({
      where: { winnerGithubId: u.githubId, status: 'CONFIRMED' },
      include: { bounty: { select: { id: true, token: true, amount: true, feeBps: true, issueTitle: true, issueUrl: true } } },
    });
    return ser({ login: u.githubLogin, avatarUrl: u.avatarUrl, paid: paid.map((p) => ({ bounty: p.bounty, txHash: p.txHash })) });
  });
}
