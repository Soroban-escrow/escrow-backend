import { Queue, Worker, type Job } from 'bullmq';
import { Redis } from 'ioredis';

export const WEBHOOKS = 'webhooks';
export interface WebhookJob { id: string; event: string; payload: unknown }

export function connection(url: string) {
  // BullMQ requires maxRetriesPerRequest: null for workers
  return new Redis(url, { maxRetriesPerRequest: null });
}

/** Producer side (API). The delivery id is the job id, so a redelivery can never queue twice. */
export function webhookProducer(redisUrl: string) {
  const conn = connection(redisUrl);
  const queue = new Queue<WebhookJob>(WEBHOOKS, { connection: conn });
  return {
    enqueue: async (id: string, event: string, payload: unknown) => {
      await queue.add(event, { id, event, payload }, { jobId: id, attempts: 5, backoff: { type: 'exponential', delay: 2000 }, removeOnComplete: 1000, removeOnFail: 5000 });
    },
    close: async () => { await queue.close(); conn.disconnect(); },
  };
}

/** Consumer side (worker process). */
export function webhookWorker(redisUrl: string, handle: (job: WebhookJob) => Promise<unknown>, concurrency = 4) {
  const conn = connection(redisUrl);
  const w = new Worker<WebhookJob>(WEBHOOKS, (job: Job<WebhookJob>) => handle(job.data), { connection: conn, concurrency });
  return { worker: w, close: async () => { await w.close(); conn.disconnect(); } };
}
