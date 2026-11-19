import { buildUserPrompt } from '../src/ai/input.js';
import { anthropicModel, DEFAULT_MODEL } from '../src/ai/anthropic.js';
import { reviewPr } from '../src/ai/review.js';
import { CASES } from './cases.js';

// Real model, real cost. `ANTHROPIC_API_KEY=... pnpm eval:ai`
const key = process.env.ANTHROPIC_API_KEY;
if (!key) { console.error('ANTHROPIC_API_KEY is required'); process.exit(1); }
const model = anthropicModel(key, process.env.AI_MODEL ?? DEFAULT_MODEL);
let failed = 0;
for (const c of CASES) {
  const { review, inputTokens } = await reviewPr(model, c.input);
  const okVerdict = c.expect.includes(review.verdict);
  const okFlags = (c.mustFlag ?? []).every((f) => (review.flags as string[]).includes(f));
  const okNot = !(c.mustNotVerdict ?? []).includes(review.verdict);
  const ok = okVerdict && okFlags && okNot;
  if (!ok) failed++;
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${c.name.padEnd(40)} ${review.verdict.padEnd(16)} score ${String(review.score).padStart(3)}  flags [${review.flags.join(',')}]  tokens ${inputTokens}  truncated=${buildUserPrompt(c.input).truncated}`);
}
console.log(`\n${CASES.length - failed}/${CASES.length} as expected`);
process.exit(failed ? 1 : 0);
