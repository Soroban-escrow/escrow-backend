import type { Db } from '../db.js';

/** Posts a comment on the bounty's issue. The real one uses the GitHub App; tests use a fake. */
export interface IssueCommenter { comment(bountyId: bigint, body: string): Promise<void> }

const HOUR = 3600e3;

/**
 * Refund reminders (BUILD_SPEC 11.7): one 48 h before the refund window opens (deadline + grace),
 * one when it opens. Each is sent at most once, recorded in the audit log.
 */
export async function sendRefundReminders(
  d: { db: Db; commenter: IssueCommenter; graceMs: number; webUrl: string; now?: () => Date },
): Promise<string[]> {
  const now = (d.now ?? (() => new Date()))().getTime();
  const open = await d.db.bounty.findMany({ where: { status: 'OPEN' } });
  const sent: string[] = [];
  for (const b of open) {
    const opensAt = b.deadline.getTime() + d.graceMs;
    const kind = now >= opensAt ? 'open' : now >= opensAt - 48 * HOUR ? 'soon' : null;
    if (!kind) continue;
    const action = `reminder.refund_${kind}`;
    const target = `bounty:${b.id}`;
    if (await d.db.auditLog.findFirst({ where: { action, target } })) continue;
    const body = kind === 'soon'
      ? `The refund window for this bounty opens in under 48 hours. If a PR was merged but not yet paid, resolve it at ${d.webUrl}/b/${b.id}; otherwise the maintainer may reclaim the funds.`
      : `The refund window for this bounty is now open. The maintainer can reclaim the funds at ${d.webUrl}/me. A merged PR can still be released until then.`;
    // record first: a failed comment is retried next tick only if we did not mark it, so mark after success
    await d.commenter.comment(b.id, body);
    await d.db.auditLog.create({ data: { actor: 'system', action, target, data: {} } });
    sent.push(`${kind}:${b.id}`);
  }
  return sent;
}
