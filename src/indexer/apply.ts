import type { Prisma } from '@prisma/client';
import type { ChainEvent } from './decode.js';

export interface EventMeta {
  eventId: string;
  txHash: string;
  closedAt: Date;
}

export type Outcome = 'applied' | 'stale' | 'untracked' | 'anomaly';

/**
 * Applies one event inside the caller's transaction. Idempotent: every bounty records the last
 * event applied, so replaying an old event is recognised as stale instead of re-applied.
 * Returns what happened so the caller can enqueue follow-up jobs only for 'applied'.
 */
export async function applyEvent(tx: Prisma.TransactionClient, ev: ChainEvent, meta: EventMeta): Promise<Outcome> {
  if (ev.kind === 'funded') {
    const repo = await tx.repo.findUnique({ where: { id: ev.repoId } });
    if (!repo) return 'untracked'; // anyone can fund any repo on-chain; we only track installed ones
    const existing = await tx.bounty.findUnique({ where: { id: ev.id } });
    if (existing) return existing.lastEventId && existing.lastEventId >= meta.eventId ? 'stale' : 'anomaly';
    const issueNumber = Number(ev.issue);
    const intent = await tx.fundingIntent.findFirst({
      where: { repoId: ev.repoId, issueNumber, usedAt: null, amount: ev.amount.toString(), token: ev.token },
      orderBy: { createdAt: 'desc' },
    });
    if (intent) await tx.fundingIntent.update({ where: { id: intent.id }, data: { usedAt: meta.closedAt } });
    await tx.bounty.create({
      data: {
        id: ev.id, repoId: ev.repoId, issueNumber,
        issueTitle: '', issueUrl: `https://github.com/${repo.fullName}/issues/${issueNumber}`,
        criteria: [], maintainer: ev.maintainer, maintainerGithubId: intent?.requestedBy ?? null,
        token: ev.token, amount: ev.amount.toString(), feeBps: ev.feeBps,
        deadline: new Date(Number(ev.deadline) * 1000), relayerAllowed: ev.relayerAllowed,
        status: 'OPEN', fundTx: meta.txHash, createdAt: meta.closedAt, lastEventId: meta.eventId,
      },
    });
    return 'applied';
  }

  const b = await tx.bounty.findUnique({ where: { id: ev.id } });
  if (!b) return 'untracked';
  if (b.lastEventId && b.lastEventId >= meta.eventId) return 'stale';

  const done = { lastEventId: meta.eventId, closeTx: meta.txHash };
  switch (ev.kind) {
    case 'released': {
      if (b.status !== 'OPEN') return 'anomaly';
      await tx.bounty.update({ where: { id: b.id }, data: { ...done, status: 'RELEASED', winnerAddress: ev.winner } });
      await tx.payout.updateMany({
        where: { bountyId: b.id, status: { in: ['QUEUED', 'SUBMITTED', 'AWAITING_MAINTAINER'] } },
        data: { status: 'CONFIRMED', txHash: meta.txHash, winnerAddress: ev.winner },
      });
      return 'applied';
    }
    case 'refunded':
      if (b.status !== 'OPEN') return 'anomaly';
      await tx.bounty.update({ where: { id: b.id }, data: { ...done, status: 'REFUNDED' } });
      return 'applied';
    case 'disputed':
      if (b.status !== 'OPEN') return 'anomaly';
      await tx.bounty.update({ where: { id: b.id }, data: { lastEventId: meta.eventId, status: 'DISPUTED' } });
      return 'applied';
    case 'resolved':
      if (b.status !== 'DISPUTED') return 'anomaly';
      await tx.bounty.update({ where: { id: b.id }, data: { ...done, status: 'RESOLVED', winnerAddress: ev.winner } });
      return 'applied';
  }
}
