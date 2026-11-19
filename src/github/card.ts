import { fromBaseUnits } from '@skillescrow/shared';

export interface CardInput {
  id: bigint; status: 'OPEN' | 'RELEASED' | 'REFUNDED' | 'DISPUTED' | 'RESOLVED';
  amount: bigint; symbol: string; deadline: Date; relayerAllowed: boolean;
  criteria: string[]; issueNumber: number; webUrl: string;
}

const BADGE = { OPEN: '🟢 Open', RELEASED: '✅ Paid', REFUNDED: '↩️ Refunded', DISPUTED: '⚠️ Disputed', RESOLVED: '⚖️ Resolved' } as const;

/** Escapes markdown/HTML-significant characters in untrusted text. */
export const esc = (s: string) => s.replace(/[\\`*_{}[\]<>()#+!|~@]/g, (c) => `\\${c}`).replace(/\r?\n/g, ' ');

export function renderCard(c: CardInput): string {
  const lines = [
    `### SkillEscrow bounty: ${fromBaseUnits(c.amount, 7, 2)} ${c.symbol} · ${BADGE[c.status]}`,
    `**Deadline:** ${c.deadline.toISOString().slice(0, 10)} · **Release on merge:** ${c.relayerAllowed ? 'automatic' : 'maintainer signs'}`,
  ];
  if (c.criteria.length) lines.push('', '**Acceptance criteria**', ...c.criteria.map((x) => `- ${esc(x)}`));
  if (c.status === 'OPEN') {
    lines.push('', `**How to claim:** open a PR that says \`Closes #${c.issueNumber}\`. Link your wallet at ${c.webUrl}/me before the merge to be paid instantly.`);
  }
  lines.push('', `[Bounty details](${c.webUrl}/b/${c.id})`);
  return lines.join('\n');
}

export const labelFor = (amount: bigint, symbol: string) => `bounty: ${fromBaseUnits(amount, 7, 0)} ${symbol}`;
