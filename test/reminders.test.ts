import { afterAll, beforeEach, describe, expect, it } from 'vitest';
import { createDb } from '../src/db.js';
import { sendRefundReminders } from '../src/ops/reminders.js';

const db = createDb(process.env.TEST_DATABASE_URL ?? 'postgresql://skillescrow:skillescrow@localhost:5432/skillescrow_test');
afterAll(() => db.$disconnect());
const H = 3600e3, GRACE = 72 * H, T0 = new Date('2026-11-02T00:00:00Z').getTime();
let posted: { id: bigint; body: string }[]; let fail = false;
const deps = (at: number) => ({ db, graceMs: GRACE, webUrl: 'https://app.test', now: () => new Date(at), commenter: { comment: async (id: bigint, body: string) => { if (fail) throw new Error('github down'); posted.push({ id, body }); } } });

beforeEach(async () => {
  posted = []; fail = false;
  await db.$executeRawUnsafe('TRUNCATE "AuditLog","Bounty","Repo","Installation" CASCADE');
  await db.installation.create({ data: { id: 1n, accountLogin: 'o', accountType: 'User' } });
  await db.repo.create({ data: { id: 100n, installationId: 1n, fullName: 'o/r', private: false } });
  await db.bounty.create({ data: { id: 1n, repoId: 100n, issueNumber: 1, issueTitle: '', issueUrl: '', criteria: [], maintainer: 'GM', token: 'C', amount: '1', feeBps: 300, deadline: new Date(T0), relayerAllowed: true, status: 'OPEN', fundTx: 't', createdAt: new Date(T0) } });
});

describe('sendRefundReminders', () => {
  it('stays quiet while the window is far away', async () => {
    expect(await sendRefundReminders(deps(T0))).toEqual([]); // window opens at T0+72h; reminders start at T0+24h
    expect(await sendRefundReminders(deps(T0 + 24 * H - 1))).toEqual([]);
    expect(await sendRefundReminders(deps(T0 + 24 * H))).toEqual(['soon:1']); // boundary is inclusive
  });
  it('sends "soon" 48 h before the window opens and "open" when it opens, once each', async () => {
    expect(await sendRefundReminders(deps(T0 + 23 * H))).toEqual([]);
    expect(await sendRefundReminders(deps(T0 + 30 * H))).toEqual(['soon:1']);
    expect(await sendRefundReminders(deps(T0 + 31 * H))).toEqual([]); // not repeated
    expect(await sendRefundReminders(deps(T0 + 73 * H))).toEqual(['open:1']);
    expect(await sendRefundReminders(deps(T0 + 80 * H))).toEqual([]);
    expect(posted.map((p) => p.body.slice(0, 22))).toEqual(['The refund window for ', 'The refund window for ']);
    expect(posted[1]!.body).toContain('now open');
  });
  it('a failed comment is retried on the next tick (not marked as sent)', async () => {
    fail = true;
    await expect(sendRefundReminders(deps(T0 + 30 * H))).rejects.toThrow('github down');
    fail = false;
    expect(await sendRefundReminders(deps(T0 + 31 * H))).toEqual(['soon:1']);
  });
  it('ignores bounties that are no longer open', async () => {
    await db.bounty.update({ where: { id: 1n }, data: { status: 'RELEASED' } });
    expect(await sendRefundReminders(deps(T0 + 80 * H))).toEqual([]);
  });
});
