import type { Db } from '../db.js';
import { relayRelease, type PayoutDeps } from './payout.js';
import type { RelayerPort } from './ports.js';

/** Exponential backoff between attempts of the same payout (2 s, 4 s, 8 s, ...). */
export const backoffMs = (attempts: number) => Math.min(2 ** Math.max(attempts, 1) * 1000, 5 * 60_000);

/**
 * One pass: process queued and submitted payouts one at a time (single signing account, so
 * sequence numbers never race). Returns how many were attempted.
 */
export async function relayerPass(
  d: PayoutDeps & { db: Db }, r: RelayerPort, retryAt: Map<string, number>, now = () => Date.now(), log?: (m: string, x?: unknown) => void,
): Promise<number> {
  const due = await d.db.payout.findMany({ where: { status: { in: ['QUEUED', 'SUBMITTED'] } }, orderBy: { createdAt: 'asc' }, take: 20 });
  let n = 0;
  for (const p of due) {
    if ((retryAt.get(p.id) ?? 0) > now()) continue;
    n++;
    try {
      const res = await relayRelease(d, r, p.bountyId);
      retryAt.delete(p.id);
      log?.('relayer', { bounty: String(p.bountyId), result: res });
    } catch (e) {
      retryAt.set(p.id, now() + backoffMs(p.attempts + 1));
      log?.('relayer transient failure', { bounty: String(p.bountyId), error: e instanceof Error ? e.message : String(e) });
    }
  }
  return n;
}

export async function runRelayer(d: PayoutDeps & { db: Db }, r: RelayerPort, signal: AbortSignal, intervalMs = 5000, log?: (m: string, x?: unknown) => void) {
  const retryAt = new Map<string, number>();
  while (!signal.aborted) {
    try { await relayerPass(d, r, retryAt, Date.now, log); } catch (e) { log?.('relayer pass failed', e instanceof Error ? e.message : String(e)); }
    await new Promise((res) => setTimeout(res, intervalMs));
  }
}
