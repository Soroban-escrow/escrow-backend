import { StrKey } from '@stellar/stellar-sdk';
import type { FastifyInstance, FastifyReply, FastifyRequest } from 'fastify';
import { z } from 'zod';
import { contractErrorName } from '../chain/errors.js';
import type { Db } from '../db.js';
import { mutationLimit } from './limits.js';

/** What the funding routes need from the chain; the real one is createChain(). */
export interface FundingChain {
  buildFundTx(p: { maintainer: string; repoId: bigint; issue: bigint; token: string; amount: bigint; deadline: bigint; relayerAllowed: boolean }): Promise<string>;
  buildReleaseTx(maintainer: string, bountyId: bigint, winner: string): Promise<string>;
  buildRefundTx(maintainer: string, bountyId: bigint): Promise<string>;
  submitSigned(xdr: string, expectedSource?: string): Promise<string>;
  getBounty(id: bigint): Promise<{ status: string }>;
}

export interface FundingDeps {
  db: Db;
  chain: FundingChain;
  now?: () => Date;
}

type Authed = FastifyRequest & { user?: { id: string; githubId: bigint; wallet: string | null } };
const err = (r: FastifyReply, status: number, code: string, message: string) => r.code(status).send({ error: { code, message } });

/** Turns a simulation failure into a stable, user-facing error (contract error name when we can find one). */
function chainError(r: FastifyReply, e: unknown) {
  const msg = e instanceof Error ? e.message : String(e);
  const name = contractErrorName(msg);
  return err(r, 422, name ? `contract_${name}` : 'chain_error', name ? `The contract rejected this: ${name}` : 'The network rejected this request.');
}

export function registerFunding(app: FastifyInstance, d: FundingDeps, authed: (mutating: boolean) => object) {
  const now = () => (d.now ?? (() => new Date()))();
  const limit = mutationLimit(20);

  const state = (i: { expiresAt: Date; usedAt: Date | null }) => (i.usedAt ? 'used' : i.expiresAt <= now() ? 'expired' : 'open');

  app.get('/intents/:id', async (req, reply) => {
    const id = (req.params as { id: string }).id;
    if (!/^[\w-]{10,64}$/.test(id)) return err(reply, 404, 'not_found', 'funding link not found');
    const i = await d.db.fundingIntent.findUnique({ where: { id } });
    if (!i) return err(reply, 404, 'not_found', 'funding link not found');
    const repo = await d.db.repo.findUnique({ where: { id: i.repoId } });
    const funded = i.usedAt ? await d.db.bounty.findFirst({ where: { repoId: i.repoId, issueNumber: i.issueNumber }, orderBy: { id: 'desc' } }) : null;
    return {
      state: state(i), repo: repo?.fullName ?? null, issueNumber: i.issueNumber, token: i.token, amount: i.amount.toString(),
      durationDays: i.durationDays, expiresAt: i.expiresAt, bountyId: funded ? funded.id.toString() : null,
    };
  });

  const build = z.object({ address: z.string(), relayerAllowed: z.boolean() });
  app.post('/intents/:id/build', { ...authed(true), ...limit }, async (req, reply) => {
    const u = (req as Authed).user!;
    const body = build.safeParse(req.body);
    if (!body.success || !StrKey.isValidEd25519PublicKey(body.data.address)) return err(reply, 400, 'bad_request', 'a valid address and relayerAllowed are required');
    const i = await d.db.fundingIntent.findUnique({ where: { id: (req.params as { id: string }).id } });
    if (!i) return err(reply, 404, 'not_found', 'funding link not found');
    if (state(i) !== 'open') return err(reply, 410, `intent_${state(i)}`, `this funding link is ${state(i)}`);
    // only the GitHub user who typed /bounty may fund through this link
    if (i.requestedBy !== u.githubId) return err(reply, 403, 'not_requester', 'this link belongs to another GitHub user');
    try {
      const deadline = BigInt(Math.floor(now().getTime() / 1000) + i.durationDays * 86400 + 60);
      const xdr = await d.chain.buildFundTx({
        maintainer: body.data.address, repoId: i.repoId, issue: BigInt(i.issueNumber), token: i.token,
        amount: BigInt(i.amount.toString()), deadline, relayerAllowed: body.data.relayerAllowed,
      });
      return { xdr };
    } catch (e) { return chainError(reply, e); }
  });

  /** Relays a signed single-operation contract call. Never trusts client claims about the result. */
  const submit = z.object({ xdr: z.string().min(10).max(20_000), address: z.string().optional() });
  app.post('/tx/submit', { ...authed(true), ...limit }, async (req, reply) => {
    const body = submit.safeParse(req.body);
    if (!body.success) return err(reply, 400, 'bad_request', 'xdr required');
    try { return { hash: await d.chain.submitSigned(body.data.xdr, body.data.address) }; }
    catch (e) { return err(reply, 422, 'submit_rejected', e instanceof Error ? e.message.slice(0, 200) : 'rejected'); }
  });

  // ---- maintainer self-release and refund ----
  async function maintainerBounty(req: FastifyRequest, reply: FastifyReply) {
    const id = (req.params as { id: string }).id;
    if (!/^\d{1,18}$/.test(id)) return void err(reply, 404, 'not_found', 'bounty not found');
    const b = await d.db.bounty.findUnique({ where: { id: BigInt(id) } });
    if (!b) return void err(reply, 404, 'not_found', 'bounty not found');
    const u = (req as Authed).user!;
    if (!u.wallet || u.wallet !== b.maintainer) return void err(reply, 403, 'not_maintainer', 'only the maintainer wallet can do this');
    return b;
  }

  app.post('/bounties/:id/release/build', { ...authed(true), ...limit }, async (req, reply) => {
    const b = await maintainerBounty(req, reply);
    if (!b) return reply;
    const payout = await d.db.payout.findUnique({ where: { idempotencyKey: `release:${b.id}` } });
    if (!payout || payout.status !== 'AWAITING_MAINTAINER' || !payout.winnerAddress) return err(reply, 409, 'nothing_to_release', 'no payout is waiting for your signature');
    try {
      const chain = await d.chain.getBounty(b.id); // never trust the DB for money decisions
      if (chain.status !== 'Open') return err(reply, 409, 'not_open', `bounty is ${chain.status}`);
      return { xdr: await d.chain.buildReleaseTx(b.maintainer, b.id, payout.winnerAddress), winner: payout.winnerAddress };
    } catch (e) { return chainError(reply, e); }
  });

  app.post('/bounties/:id/refund/build', { ...authed(true), ...limit }, async (req, reply) => {
    const b = await maintainerBounty(req, reply);
    if (!b) return reply;
    try {
      const chain = await d.chain.getBounty(b.id);
      if (chain.status !== 'Open') return err(reply, 409, 'not_open', `bounty is ${chain.status}`);
      return { xdr: await d.chain.buildRefundTx(b.maintainer, b.id) }; // the contract enforces deadline + grace
    } catch (e) { return chainError(reply, e); }
  });
}
