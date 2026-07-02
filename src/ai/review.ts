import { createHash } from 'node:crypto';
import { buildUserPrompt, SYSTEM_PROMPT, type ReviewInput } from './input.js';
import { needsHuman, reviewSchema, type Review } from './schema.js';

/** The model call. Returns the tool-call arguments as parsed JSON, plus token counts. */
export interface ModelPort {
  complete(system: string, user: string): Promise<{ output: unknown; inputTokens: number; outputTokens: number }>;
}

export async function reviewPr(m: ModelPort, input: ReviewInput) {
  const { text } = buildUserPrompt(input);
  let inputTokens = 0, outputTokens = 0;
  for (let attempt = 0; attempt < 2; attempt++) {
    const r = await m.complete(SYSTEM_PROMPT, text);
    inputTokens += r.inputTokens; outputTokens += r.outputTokens;
    const parsed = reviewSchema.safeParse(r.output);
    if (parsed.success) return { review: parsed.data, inputTokens, outputTokens };
  }
  return { review: needsHuman('The model returned invalid output twice; a human should review this PR.'), inputTokens, outputTokens };
}

/** Same changed content (ignoring whitespace) hashes equal: duplicate detection across PRs on one bounty. */
export function diffHash(files: ReviewInput['files']): string {
  const h = createHash('sha256');
  for (const f of [...files].sort((a, b) => a.path.localeCompare(b.path))) h.update(f.path + '\0' + (f.patch ?? '').replace(/\s+/g, '') + '\0');
  return h.digest('hex');
}

const ORDER = { likely_meets: 0, partially_meets: 1, does_not_meet: 2, needs_human: 3 } as const;
export interface Rankable { id: string; verdict: Review['verdict']; score: number; ci: 'success' | 'failure' | 'pending' | 'none'; submittedAt: number }

/** verdict, then score, then CI, then earliest submission. Returns ids best first. */
export function rankPrs(prs: Rankable[]): string[] {
  const ci = { success: 0, pending: 1, none: 1, failure: 2 } as const;
  return [...prs].sort((a, b) =>
    ORDER[a.verdict] - ORDER[b.verdict] || b.score - a.score || ci[a.ci] - ci[b.ci] || a.submittedAt - b.submittedAt).map((p) => p.id);
}

const ICON = { met: '✅', partial: '⚠️', not_met: '❌', unclear: '❔' } as const;
const WORDS = { likely_meets: 'Likely meets', partially_meets: 'Partially meets', does_not_meet: 'Does not meet', needs_human: 'Needs a human' } as const;
const cell = (s: string) => s.replace(/[\\`*_{}[\]<>()#+!|~@]/g, (c) => `\\${c}`).replace(/\r?\n/g, ' ');

export function renderReview(r: Review, o: { rank?: number; of?: number; sha: string; model: string }): string {
  return [
    '🤖 **SkillEscrow review** (advisory only, the maintainer decides)',
    `**Verdict:** ${WORDS[r.verdict]} (score ${r.score})${o.rank && o.of ? ` · Rank ${o.rank} of ${o.of} on this bounty` : ''}`,
    cell(r.summary),
    ...r.criteria.map((c) => `${ICON[c.status]} ${cell(c.criterion)}${c.derived ? ' _(derived)_' : ''}: ${cell(c.evidence)}`),
    r.risks.length ? `**Risks:** ${r.risks.map(cell).join('; ')}` : '',
    r.flags.length ? `**Flags:** ${r.flags.join(', ')}` : '',
    `_Reviewed commit ${o.sha.slice(0, 7)} · model ${o.model}_`,
  ].filter(Boolean).join('\n\n');
}
