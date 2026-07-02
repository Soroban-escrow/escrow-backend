import { StrKey } from '@stellar/stellar-sdk';
import type { FastifyInstance, FastifyReply, FastifyRequest } from 'fastify';
import { z } from 'zod';
import { contractErrorName } from '../chain/errors.js';
import type { Db } from '../db.js';
import { mutationLimit } from './limits.js';

export interface AdminChain {
  buildOpenDisputeTx(arbiter: string, bountyId: bigint): Promise<string>;
  buildResolveTx(arbiter: string, bountyId: bigint, winner: string | undefined, winnerBps: number): Promise<string>;
}
export interface AdminDeps { db: Db; chain: AdminChain; now?: () => Date }

type Authed = FastifyRequest & { user?: { id: string; githubId: bigint; isAdmin: boolean } };
const err = (r: FastifyReply, status: number, code: string, message: string) => r.code(status).send({ error: { code, message } });
const ser = (v: unknown): unknown => JSON.parse(JSON.stringify(v, (_, x) => (typeof x === 'bigint' ? x.toString() : x)));
const HOURS = 36;

export function registerAdmin(app: FastifyInstance, d: AdminDeps, authed: (mutating: boolean) => object) {
  const now = () => (d.now ?? (() => new Date()))();
  /** Admin rights are checked server-side on every route; the flag follows config, not the client. */
  const adminOnly = (mutating: boolean) => (mutating ? { ...authed(mutating), ...mutationLimit(30) } : authed(mutating));
  const guard = async (req: FastifyRequest, reply: FastifyReply) => ((req as Authed).user?.isAdmin ? true : (void err(reply, 403, 'forbidden', 'admins only'), false));
  const audit = (req: FastifyRequest, action: string, target: string, data: object) =>
    d.db.auditLog.create({ data: { actor: `admin:${(req as Authed).user!.githubId}`, action, target, data } });

  app.get('/admin/disputes', adminOnly(false), async (req, reply) => {
    if (!(await guard(req, reply))) return reply;
    const rows = await d.db.dispute.findMany({
      where: { status: { in: ['requested', 'opened_onchain'] } }, orderBy: { createdAt: 'asc' },
      include: { bounty: { include: { prs: { include: { reviews: { take: 1, orderBy: { createdAt: 'desc' } } } }, payouts: true, repo: { select: { fullName: true } } } } },
    });
    return ser(rows.map((r) => ({ ...r, overdue: now().getTime() - r.createdAt.getTime() > HOURS * 3600e3 })));
  });

  const idOk = (id: string) => /^[a-z0-9]{10,40}$/.test(id);
  async function load(req: FastifyRequest, reply: FastifyReply) {
    const id = (req.params as { id: string }).id;
    if (!idOk(id)) return void err(reply, 404, 'not_found', 'dispute not found');
    const x = await d.db.dispute.findUnique({ where: { id } });
    return x ?? void err(reply, 404, 'not_found', 'dispute not found');
  }
  const mapErr = (reply: FastifyReply, e: unknown) => {
    const n = contractErrorName(e instanceof Error ? e.message : '');
    return err(reply, 422, n ? `contract_${n}` : 'chain_error', n ? `The contract rejected this: ${n}` : 'The network rejected this request.');
  };

  const addr = z.object({ arbiterAddress: z.string().refine((a) => StrKey.isValidEd25519PublicKey(a)) });
  app.post('/admin/disputes/:id/open-onchain', adminOnly(true), async (req, reply) => {
    if (!(await guard(req, reply))) return reply;
    const x = await load(req, reply); if (!x) return reply;
    const b = addr.safeParse(req.body);
    if (!b.success) return err(reply, 400, 'bad_request', 'a valid arbiterAddress is required');
    if (x.status !== 'requested') return err(reply, 409, 'wrong_state', `dispute is ${x.status}`);
    try {
      const xdr = await d.chain.buildOpenDisputeTx(b.data.arbiterAddress, x.bountyId);
      await audit(req, 'dispute.open_onchain.build', `dispute:${x.id}`, { bounty: String(x.bountyId) });
      return { xdr };
    } catch (e) { return mapErr(reply, e); }
  });

  const resolve = z.object({
    arbiterAddress: z.string().refine((a) => StrKey.isValidEd25519PublicKey(a)),
    winner: z.string().refine((a) => StrKey.isValidEd25519PublicKey(a)).optional(),
    winnerBps: z.number().int().min(0).max(10_000),
    note: z.string().max(1000).default(''),
  });
  app.post('/admin/disputes/:id/resolve/build', adminOnly(true), async (req, reply) => {
    if (!(await guard(req, reply))) return reply;
    const x = await load(req, reply); if (!x) return reply;
    const b = resolve.safeParse(req.body);
    if (!b.success) return err(reply, 400, 'bad_request', 'arbiterAddress, winnerBps (0-10000) and, when bps > 0, a winner are required');
    if (b.data.winnerBps > 0 && !b.data.winner) return err(reply, 400, 'winner_required', 'a winner address is required when winnerBps > 0');
    if (x.status !== 'opened_onchain') return err(reply, 409, 'wrong_state', `dispute is ${x.status}; open it on chain first`);
    try {
      const xdr = await d.chain.buildResolveTx(b.data.arbiterAddress, x.bountyId, b.data.winnerBps > 0 ? b.data.winner : undefined, b.data.winnerBps);
      await d.db.dispute.update({ where: { id: x.id }, data: { resolutionNote: b.data.note } });
      await audit(req, 'dispute.resolve.build', `dispute:${x.id}`, { winner: b.data.winner ?? null, bps: b.data.winnerBps });
      return { xdr };
    } catch (e) { return mapErr(reply, e); }
  });

  app.post('/admin/disputes/:id/reject', adminOnly(true), async (req, reply) => {
    if (!(await guard(req, reply))) return reply;
    const x = await load(req, reply); if (!x) return reply;
    const note = z.object({ note: z.string().min(1).max(1000) }).safeParse(req.body);
    if (!note.success) return err(reply, 400, 'bad_request', 'a note is required');
    if (x.status !== 'requested') return err(reply, 409, 'wrong_state', `dispute is ${x.status}`);
    await d.db.dispute.update({ where: { id: x.id }, data: { status: 'rejected', resolutionNote: note.data.note } });
    await audit(req, 'dispute.rejected', `dispute:${x.id}`, { note: note.data.note });
    return { ok: true };
  });

  app.get('/admin/payouts', adminOnly(false), async (req, reply) => {
    if (!(await guard(req, reply))) return reply;
    const status = z.enum(['FAILED', 'PENDING_WALLET', 'AWAITING_MAINTAINER', 'QUEUED', 'SUBMITTED', 'CONFIRMED']).default('FAILED').parse((req.query as { status?: string }).status ?? 'FAILED');
    return ser(await d.db.payout.findMany({ where: { status }, orderBy: { updatedAt: 'desc' }, take: 100 }));
  });

  /** Only payouts with no submitted transaction can be re-queued; one with a hash must be checked on chain first. */
  app.post('/admin/payouts/:id/retry', adminOnly(true), async (req, reply) => {
    if (!(await guard(req, reply))) return reply;
    const id = (req.params as { id: string }).id;
    const p = idOk(id) ? await d.db.payout.findUnique({ where: { id } }) : null;
    if (!p) return err(reply, 404, 'not_found', 'payout not found');
    if (p.status !== 'FAILED') return err(reply, 409, 'wrong_state', `payout is ${p.status}`);
    if (p.txHash) return err(reply, 409, 'has_tx_hash', 'a transaction was already submitted; verify it on chain instead of retrying');
    await d.db.payout.update({ where: { id }, data: { status: 'QUEUED', attempts: 0, lastError: null } });
    await audit(req, 'payout.retry', `payout:${id}`, {});
    return { ok: true };
  });

  app.get('/admin/status', adminOnly(false), async (req, reply) => {
    if (!(await guard(req, reply))) return reply;
    const [cursor, failed, pending, disputes] = await Promise.all([
      d.db.indexerCursor.findUnique({ where: { id: 'bounty_escrow' } }),
      d.db.payout.count({ where: { status: 'FAILED' } }),
      d.db.payout.count({ where: { status: { in: ['PENDING_WALLET', 'AWAITING_MAINTAINER'] } } }),
      d.db.dispute.count({ where: { status: { in: ['requested', 'opened_onchain'] } } }),
    ]);
    return { indexerLedger: cursor?.ledger ?? null, indexerUpdatedAt: cursor?.updatedAt ?? null, failedPayouts: failed, waitingPayouts: pending, openDisputes: disputes };
  });
}
