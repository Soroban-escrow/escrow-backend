import { afterAll, beforeEach, describe, expect, it } from 'vitest';
import { syncInstallations } from '../src/github/backfill.js';
import { createDb } from '../src/db.js';

const db = createDb(process.env.TEST_DATABASE_URL ?? 'postgresql://skillescrow:skillescrow@localhost:5432/skillescrow_test');
afterAll(() => db.$disconnect());
beforeEach(() => db.$executeRawUnsafe('TRUNCATE "Payout","Bounty","Repo","Installation" CASCADE'));

const src = (susp: string | null = null) => ({
  listInstallations: async () => [{ id: 9, account: { login: 'o', type: 'User' }, suspended_at: susp }],
  listRepositories: async () => [{ id: 100, full_name: 'o/r', private: false }, { id: 101, full_name: 'o/r2', private: true }],
});

describe('syncInstallations', () => {
  it('upserts installations and repos, and is safe to repeat', async () => {
    expect(await syncInstallations(db, src())).toEqual({ installations: 1, repos: 2 });
    expect(await syncInstallations(db, src())).toEqual({ installations: 1, repos: 2 });
    expect(await db.repo.count()).toBe(2);
    expect((await db.repo.findUniqueOrThrow({ where: { id: 101n } })).private).toBe(true);
  });
  it('records a suspended installation but does not list its repos', async () => {
    expect(await syncInstallations(db, src('2026-11-01T00:00:00Z'))).toEqual({ installations: 1, repos: 0 });
    expect((await db.installation.findUniqueOrThrow({ where: { id: 9n } })).suspendedAt).not.toBeNull();
  });
});
