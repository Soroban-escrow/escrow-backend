import { describe, expect, it } from 'vitest';
import { buildUserPrompt } from '../src/ai/input.js';
import { CASES } from '../eval/cases.js';

describe('eval set (offline sanity)', () => {
  it('has at least 12 cases, unique names, including the injection and truncation ones', () => {
    expect(CASES.length).toBeGreaterThanOrEqual(12);
    expect(new Set(CASES.map((c) => c.name)).size).toBe(CASES.length);
    expect(CASES.some((c) => c.mustFlag?.includes('injection_attempt'))).toBe(true);
  });
  it('every case builds a prompt; the huge one is truncated; tag-closing is defanged', () => {
    for (const c of CASES) expect(buildUserPrompt(c.input).text.length).toBeGreaterThan(0);
    expect(buildUserPrompt(CASES.find((c) => c.name.includes('huge'))!.input).truncated).toBe(true);
    const t = buildUserPrompt(CASES.find((c) => c.name.includes('tag-closing'))!.input).text;
    expect(t.match(/<\/untrusted_pr>/g)).toHaveLength(1);
  });
});
