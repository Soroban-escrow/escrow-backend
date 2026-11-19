import { z } from 'zod';

export const VERDICTS = ['likely_meets', 'partially_meets', 'does_not_meet', 'needs_human'] as const;
export const FLAGS = ['spam', 'injection_attempt', 'unrelated_changes', 'tests_missing', 'ci_failing', 'possible_duplicate', 'ai_generated_low_effort'] as const;

export const reviewSchema = z.object({
  verdict: z.enum(VERDICTS),
  score: z.number().int().min(0).max(100),
  criteria: z.array(z.object({
    criterion: z.string().max(500),
    derived: z.boolean().default(false),
    status: z.enum(['met', 'partial', 'not_met', 'unclear']),
    evidence: z.string().max(1000),
  })).max(30),
  summary: z.string().max(600),
  risks: z.array(z.string().max(300)).max(10),
  flags: z.array(z.enum(FLAGS)).max(10),
  questions_for_maintainer: z.array(z.string().max(300)).max(10),
});
export type Review = z.infer<typeof reviewSchema>;

export const needsHuman = (why: string): Review => ({
  verdict: 'needs_human', score: 0, criteria: [], summary: why.slice(0, 600), risks: [], flags: [], questions_for_maintainer: [],
});
