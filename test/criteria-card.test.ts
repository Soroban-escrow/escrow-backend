import { describe, expect, it } from 'vitest';
import { renderCard, esc, labelFor } from '../src/github/card.js';
import { parseCriteria } from '../src/github/criteria.js';

describe('parseCriteria', () => {
  it('reads the Acceptance criteria section', () => {
    const body = '## Context\n- not this\n\n## Acceptance Criteria\n- [ ] first\n- [x] second\n* third\n1. fourth\n\n## Notes\n- no';
    expect(parseCriteria(body)).toEqual(['first', 'second', 'third', 'fourth']);
  });
  it('falls back to all checklist items, then empty', () => {
    expect(parseCriteria('- [ ] a\ntext\n- [x] b\n- plain')).toEqual(['a', 'b']);
    expect(parseCriteria('just prose')).toEqual([]);
    expect(parseCriteria(null)).toEqual([]);
    expect(parseCriteria('## Acceptance criteria\nnone listed\n- [ ] later')).toEqual(['later']);
  });
  it('handles CRLF', () => {
    expect(parseCriteria('## Acceptance criteria\r\n- [ ] a\r\n- [ ] b\r\n')).toEqual(['a', 'b']);
  });
});

describe('card', () => {
  const base = { id: 4n, status: 'OPEN' as const, amount: 500000000n, symbol: 'USDC', deadline: new Date('2026-12-01T00:00:00Z'), relayerAllowed: true, criteria: ['works'], issueNumber: 12, webUrl: 'https://app.test' };
  it('renders an open card', () => {
    const t = renderCard(base);
    expect(t).toContain('50.00 USDC');
    expect(t).toContain('Closes #12');
    expect(t).toContain('2026-12-01');
    expect(t).toContain('automatic');
    expect(t).toContain('https://app.test/b/4');
  });
  it('drops the claim text once paid', () => {
    const t = renderCard({ ...base, status: 'RELEASED' });
    expect(t).toContain('Paid');
    expect(t).not.toContain('How to claim');
  });
  it('escapes untrusted criteria', () => {
    const t = renderCard({ ...base, criteria: ['<img src=x onerror=1> @everyone [x](http://e)'] });
    expect(t).not.toMatch(/(^|[^\\])</);
    expect(t).not.toMatch(/(^|[^\\])@everyone/);
    expect(esc('a\nb')).toBe('a b');
  });
  it('label', () => expect(labelFor(500000000n, 'USDC')).toBe('bounty: 50 USDC'));
});
