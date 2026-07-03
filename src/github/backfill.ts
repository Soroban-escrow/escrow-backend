import type { Db } from '../db.js';

interface Source {
  listInstallations(): Promise<{ id: number; account: { login: string; type: string }; suspended_at: string | null }[]>;
  listRepositories(installation: bigint): Promise<{ id: number; full_name: string; private: boolean }[]>;
}

/** Brings installations and their repositories in line with GitHub (covers webhooks missed while we were down). */
export async function syncInstallations(db: Db, gh: Source): Promise<{ installations: number; repos: number }> {
  let repos = 0;
  const installations = await gh.listInstallations();
  for (const i of installations) {
    const id = BigInt(i.id);
    const suspendedAt = i.suspended_at ? new Date(i.suspended_at) : null;
    await db.installation.upsert({
      where: { id }, create: { id, accountLogin: i.account.login, accountType: i.account.type, suspendedAt },
      update: { accountLogin: i.account.login, accountType: i.account.type, suspendedAt },
    });
    if (suspendedAt) continue;
    for (const r of await gh.listRepositories(id)) {
      await db.repo.upsert({
        where: { id: BigInt(r.id) }, create: { id: BigInt(r.id), installationId: id, fullName: r.full_name, private: r.private },
        update: { installationId: id, fullName: r.full_name, private: r.private },
      });
      repos++;
    }
  }
  return { installations: installations.length, repos };
}
