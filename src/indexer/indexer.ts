import type { rpc } from '@stellar/stellar-sdk';
import type { Db } from '../db.js';
import { applyEvent, type Outcome } from './apply.js';
import { decodeEvent } from './decode.js';

export const CURSOR_ID = 'bounty_escrow';

export interface IndexerDeps {
  db: Db;
  server: Pick<rpc.Server, 'getEvents' | 'getLatestLedger'>;
  contractId: string;
  /** called after each committed batch for events that changed state */
  onApplied?: (e: { bountyId: bigint; kind: string }) => void;
  log?: (msg: string, extra?: unknown) => void;
}

/** Plain-object RPC rejections ({code, message}) are not Errors. */
export function errorMessage(e: unknown): string {
  if (e instanceof Error) return e.message;
  if (e && typeof e === 'object' && 'message' in e) return String((e as { message: unknown }).message);
  return String(e);
}

/** The ledger sequence encoded in an RPC event cursor (TOID: ledger is the high 32 bits). */
export function cursorLedger(cursor: string): number {
  const toid = BigInt(cursor.split('-')[0] ?? '0');
  return Number(toid >> 32n);
}

const MAX_PAGES = 200;

/**
 * Catches up to the chain tip. The RPC scans a bounded ledger range per request and may return
 * an empty page with an advanced cursor, so we page until the cursor reaches the latest ledger.
 * Each page and its cursor are committed in one transaction. Returns events seen.
 */
export async function pollOnce(d: IndexerDeps, startLedgerFallback?: number): Promise<number> {
  let seen = 0;
  for (let page = 0; page < MAX_PAGES; page++) {
    const saved = await d.db.indexerCursor.findUnique({ where: { id: CURSOR_ID } });
    const filters = [{ type: 'contract' as const, contractIds: [d.contractId] }];
    let res: rpc.Api.GetEventsResponse;
    try {
      res = saved?.cursor
        ? await d.server.getEvents({ filters, cursor: saved.cursor, limit: 200 })
        : await d.server.getEvents({ filters, startLedger: startLedgerFallback ?? (await d.server.getLatestLedger()).sequence - 1000, limit: 200 });
    } catch (e) {
      const msg = errorMessage(e);
      // cursor older than RPC retention: drop it so the next poll restarts from the fallback, loudly
      if (/ledger range|retention|startLedger|cursor/i.test(msg)) {
        d.log?.('cursor outside retained range; restarting from fallback', msg);
        await d.db.indexerCursor.deleteMany({ where: { id: CURSOR_ID } });
        return seen;
      }
      throw e;
    }
    const applied: { bountyId: bigint; kind: string }[] = [];
    await d.db.$transaction(async (tx) => {
      let ledger = saved?.ledger ?? 0;
      for (const raw of res.events) {
        if (raw.inSuccessfulContractCall === false) continue;
        const ev = decodeEvent(raw);
        ledger = Math.max(ledger, raw.ledger);
        if (!ev) continue;
        const out: Outcome = await applyEvent(tx, ev, { eventId: raw.id, txHash: raw.txHash, closedAt: new Date(raw.ledgerClosedAt) });
        if (out === 'anomaly') d.log?.('anomalous event ignored', { id: raw.id, kind: ev.kind });
        if (out === 'applied') applied.push({ bountyId: ev.id, kind: ev.kind });
      }
      await tx.indexerCursor.upsert({
        where: { id: CURSOR_ID },
        create: { id: CURSOR_ID, cursor: res.cursor, ledger },
        update: { cursor: res.cursor, ledger },
      });
    });
    for (const a of applied) d.onApplied?.(a);
    seen += res.events.length;
    const caughtUp = cursorLedger(res.cursor) >= res.latestLedger - 1 || res.cursor === saved?.cursor;
    if (res.events.length < 200 && caughtUp) break;
  }
  return seen;
}

export async function run(d: IndexerDeps, signal: AbortSignal, intervalMs = 5000): Promise<void> {
  while (!signal.aborted) {
    try {
      await pollOnce(d);
    } catch (e) {
      d.log?.('poll failed', errorMessage(e));
    }
    await new Promise((r) => setTimeout(r, intervalMs));
  }
}
