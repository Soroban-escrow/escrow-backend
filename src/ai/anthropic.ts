import Anthropic from '@anthropic-ai/sdk';
import { z } from 'zod';
import type { ModelPort } from './review.js';
import { reviewSchema } from './schema.js';

export const DEFAULT_MODEL = 'claude-sonnet-5-5';

/** Structured output via forced tool use. Not exercised against the real API yet (needs ANTHROPIC_API_KEY). */
export function anthropicModel(apiKey: string, model = DEFAULT_MODEL): ModelPort {
  const client = new Anthropic({ apiKey });
  const input_schema = z.toJSONSchema(reviewSchema) as Anthropic.Tool.InputSchema;
  return {
    async complete(system, user) {
      const res = await client.messages.create({
        model, max_tokens: 2048, system,
        tools: [{ name: 'submit_review', description: 'Submit the structured advisory review.', input_schema }],
        tool_choice: { type: 'tool', name: 'submit_review' },
        messages: [{ role: 'user', content: user }],
      });
      const block = res.content.find((b) => b.type === 'tool_use');
      return { output: block && block.type === 'tool_use' ? block.input : undefined, inputTokens: res.usage.input_tokens, outputTokens: res.usage.output_tokens };
    },
  };
}
