import { beforeAll, describe, expect, it } from 'vitest';
import { loadConfig, loadDotEnv } from '../src/config.js';
import { createGithubClient } from '../src/github/client.js';

// Real GitHub: needs apps/api/.env, the App installed on the repo, and an open issue.
// LIVE_GITHUB=1 LIVE_REPO=owner/name LIVE_ISSUE=1 LIVE_INSTALLATION=123 LIVE_OWNER=login
const { LIVE_GITHUB, LIVE_REPO, LIVE_ISSUE, LIVE_INSTALLATION, LIVE_OWNER } = process.env;
describe.runIf(LIVE_GITHUB && LIVE_REPO && LIVE_ISSUE && LIVE_INSTALLATION && LIVE_OWNER)('GitHub client (real GitHub)', () => {
  // set up lazily: vitest still evaluates the body of a skipped suite, and CI has no .env
  let gh: ReturnType<typeof createGithubClient>;
  const inst = BigInt(LIVE_INSTALLATION ?? 0), repo = LIVE_REPO ?? '', issue = Number(LIVE_ISSUE);
  beforeAll(() => {
    loadDotEnv();
    const cfg = loadConfig();
    gh = createGithubClient({ appId: cfg.GITHUB_APP_ID!, privateKey: cfg.GITHUB_APP_PRIVATE_KEY! });
  });

  it('reads the issue and the owner permission', async () => {
    const i = await gh.getIssue(inst, repo, issue);
    expect(i.state).toBe('open');
    expect(await gh.getPermission(inst, repo, LIVE_OWNER!)).toBe('admin');
    expect(['none', 'read']).toContain(await gh.getPermission(inst, repo, 'octocat'));
  });
  it('posts, edits a comment and sets the bounty label (replacing an older one)', async () => {
    const id = await gh.postComment(inst, repo, issue, 'SkillEscrow client live test: created');
    expect(id).toBeGreaterThan(0n);
    await gh.updateComment(inst, repo, id, 'SkillEscrow client live test: edited');
    await gh.setBountyLabel(inst, repo, issue, 'bounty: 5 XLM');
    await gh.setBountyLabel(inst, repo, issue, 'bounty: 7 XLM');
    const after = await fetch(`https://api.github.com/repos/${repo}/issues/${issue}`).then((r) => r.json() as Promise<{ labels: { name: string }[] }>);
    expect(after.labels.map((l) => l.name).filter((n) => n.startsWith('bounty:'))).toEqual(['bounty: 7 XLM']);
  });
});
