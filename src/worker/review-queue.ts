import { Queue, Worker } from 'bullmq';
import { connection } from './queue.js';

export const REVIEWS = 'ai-review';
export interface ReviewJob { prId: string; force: boolean }
export const DEBOUNCE_MS = 5 * 60_000;

/**
 * Producer: schedules a review after a quiet period. A newer push replaces a review that is still
 * waiting (debounce); one that is already running is left alone and the newer one queues behind it.
 */
export function reviewProducer(redisUrl: string) {
  const conn = connection(redisUrl);
  const queue = new Queue<ReviewJob>(REVIEWS, { connection: conn });
  return {
    async schedule(prId: bigint, delayMs = DEBOUNCE_MS, force = false) {
      const id = `review-${prId}`;
      const existing = await queue.getJob(id);
      if (existing) {
        const state = await existing.getState();
        if (state === 'delayed' || state === 'waiting') await existing.remove();
        else if (state === 'active') {
          // the running review used the earlier head; queue the new one right behind it
          await queue.add('review', { prId: String(prId), force }, { jobId: `${id}-next`, delay: delayMs, removeOnComplete: true, removeOnFail: 100 });
          return;
        }
      }
      await queue.add('review', { prId: String(prId), force }, { jobId: id, delay: delayMs, attempts: 3, backoff: { type: 'exponential', delay: 10_000 }, removeOnComplete: true, removeOnFail: 100 });
    },
    close: async () => { await queue.close(); conn.disconnect(); },
  };
}

export function reviewWorker(redisUrl: string, handle: (prId: bigint, force: boolean) => Promise<unknown>) {
  const conn = connection(redisUrl);
  // concurrency 1: reviews are paced by the model and the per-repo daily cap anyway
  const w = new Worker<ReviewJob>(REVIEWS, (job) => handle(BigInt(job.data.prId), job.data.force), { connection: conn, concurrency: 1 });
  return { worker: w, close: async () => { await w.close(); conn.disconnect(); } };
}
