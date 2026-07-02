import { randomBytes } from 'node:crypto';
import { fromBaseUnits, parseCommand } from '@skillescrow/shared';
import type { Db } from '../db.js';

export interface GithubPort {
  /** repo permission of a user: admin | maintain | write | triage | read | none */
  getPermission(installationId: bigint, repoFullName: string, login: string): Promise<string>;
  /** posts an issue comment, returns its id */
  postComment(installationId: bigint, repoFullName: string, issue: number, body: string): Promise<bigint>;
}

export interface CommandDeps {
  db: Db;
  gh: GithubPort;
  webUrl: string;
  tokens: Record<'USDC' | 'XLM', { address: string; min: bigint; max: bigint }>;
  minDays: number;
  maxDays: number;
  now?: () => Date;
}

export interface IssueCommentPayload {
  installation?: { id: number };
  repository: { id: number; full_name: string };
  issue: { number: number; state: string; pull_request?: unknown };
  comment: { id: number; body: string; user: { id: number; login: string; type: string } };
}

const WRITERS = new Set(['admin', 'maintain', 'write']);
const HELP = [
  '**SkillEscrow commands**',
  '- `/bounty <amount> [USDC|XLM] [<days>d]` fund this issue, e.g. `/bounty 50 USDC 14d` (needs write access)',
  '- `/bounty dispute <reason>` flag a problem on a bountied issue',
  '- `/bounty help`',
].join('\n');

/** Returns a short outcome string for logs/tests. Never throws on user error, only on infrastructure failure. */
export async function handleIssueComment(d: CommandDeps, p: IssueCommentPayload): Promise<string> {
  if (p.comment.user.type === 'Bot') return 'ignored:bot';
  const cmd = parseCommand(p.comment.body);
  if (!cmd) return 'ignored:not-a-command';
  const inst = p.installation ? BigInt(p.installation.id) : undefined;
  if (inst === undefined) return 'ignored:no-installation';
  const repo = p.repository.full_name;
  const reply = (body: string) => d.gh.postComment(inst, repo, p.issue.number, body);

  if (cmd.kind === 'help') return await reply(HELP), 'help';
  if (cmd.kind === 'error') return await reply(`${cmd.message}\n\n${HELP}`), 'error';
  if (cmd.kind === 'review') return 'ignored:review-not-on-issues';

  if (cmd.kind === 'dispute') return handleDispute(d, p, inst, reply, cmd.reason);

  // fund
  const perm = await d.gh.getPermission(inst, repo, p.comment.user.login);
  if (!WRITERS.has(perm)) return await reply(`@${p.comment.user.login} only collaborators with write access can fund a bounty.`), 'denied';
  if (p.issue.pull_request) return await reply('Bounties can only be placed on issues, not pull requests.'), 'rejected:pr';
  if (p.issue.state !== 'open') return await reply('This issue is closed.'), 'rejected:closed';
  const active = await d.db.bounty.findFirst({
    where: { repoId: BigInt(p.repository.id), issueNumber: p.issue.number, status: { in: ['OPEN', 'DISPUTED'] } },
  });
  if (active) return await reply('This issue already has an active bounty.'), 'rejected:active';
  const tok = d.tokens[cmd.token as "USDC" | "XLM"];
  if (cmd.amount < tok.min || cmd.amount > tok.max) {
    return await reply(`Amount must be between ${fromBaseUnits(tok.min)} and ${fromBaseUnits(tok.max)} ${cmd.token}.`), 'rejected:amount';
  }
  if (cmd.days < d.minDays || cmd.days > d.maxDays) {
    return await reply(`Duration must be between ${d.minDays} and ${d.maxDays} days.`), 'rejected:duration';
  }

  const now = (d.now ?? (() => new Date()))();
  const id = randomBytes(24).toString('base64url');
  const link = `${d.webUrl}/fund/${id}`;
  const commentId = await reply(
    `**Bounty requested: ${fromBaseUnits(cmd.amount, 7, 2)} ${cmd.token}**, open for ${cmd.days} days.\n\n` +
      `@${p.comment.user.login}, [fund this bounty](${link}) to activate it (link valid 24 h).`,
  );
  await d.db.fundingIntent.create({
    data: {
      id, repoId: BigInt(p.repository.id), issueNumber: p.issue.number, requestedBy: BigInt(p.comment.user.id),
      token: tok.address, amount: cmd.amount.toString(), durationDays: cmd.days, commentId,
      expiresAt: new Date(now.getTime() + 24 * 3600 * 1000),
    },
  });
  return 'intent-created';
}

async function handleDispute(
  d: CommandDeps, p: IssueCommentPayload, _inst: bigint, reply: (b: string) => Promise<bigint>, reason: string,
): Promise<string> {
  const bounty = await d.db.bounty.findFirst({
    where: { repoId: BigInt(p.repository.id), issueNumber: p.issue.number },
    orderBy: { createdAt: 'desc' },
    include: { prs: { include: { pr: true } } },
  });
  if (!bounty) return await reply('There is no bounty on this issue.'), 'rejected:no-bounty';
  const uid = BigInt(p.comment.user.id);
  const prAuthor = bounty.prs.some((l) => l.pr.authorGithubId === uid);
  if (bounty.maintainerGithubId !== uid && !prAuthor) {
    return await reply('Only the maintainer or an author of a linked PR can open a dispute.'), 'denied';
  }
  await d.db.dispute.create({
    data: { bountyId: bounty.id, openedByGithubId: uid, reason: reason.slice(0, 2000), evidence: {}, status: 'requested' },
  });
  await d.db.auditLog.create({ data: { actor: `github:${uid}`, action: 'dispute.requested', target: `bounty:${bounty.id}`, data: {} } });
  await reply('Dispute recorded. A human will review this within 48 hours.');
  return 'dispute-requested';
}
