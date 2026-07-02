import type { Db } from '../db.js';

export interface HealthDeps {
  db: Db;
  latestLedger: () => Promise<number>;
  redisPing?: () => Promise<unknown>;
  /** ledgers behind the tip that we still call healthy (60 is about 5 minutes) */
  maxLagLedgers?: number;
}

const check = async (fn: () => Promise<unknown>) => { try { await fn(); return true; } catch { return false; } };

/** Dependency checks for /health. 503 when the database or RPC is unreachable or the indexer is far behind. */
export async function healthReport(d: HealthDeps) {
  const [dbOk, rpcOk, redisOk] = await Promise.all([
    check(() => d.db.$queryRaw`SELECT 1`),
    check(d.latestLedger),
    d.redisPing ? check(d.redisPing) : Promise.resolve(true),
  ]);
  let indexerLag: number | null = null;
  if (dbOk && rpcOk) {
    const [cursor, tip] = await Promise.all([d.db.indexerCursor.findUnique({ where: { id: 'bounty_escrow' } }), d.latestLedger()]);
    indexerLag = cursor ? Math.max(0, tip - cursor.ledger) : null;
  }
  const lagOk = indexerLag === null || indexerLag <= (d.maxLagLedgers ?? 60);
  return { ok: dbOk && rpcOk && redisOk && lagOk, db: dbOk, rpc: rpcOk, redis: redisOk, indexerLagLedgers: indexerLag };
}
