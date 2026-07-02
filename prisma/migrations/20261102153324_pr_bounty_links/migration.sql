-- A PR can close several bountied issues: move the single PullRequest.bountyId into a link table,
-- copying existing links first so no data is lost.

-- CreateTable
CREATE TABLE "PullRequestBounty" (
    "prId" BIGINT NOT NULL,
    "bountyId" BIGINT NOT NULL,

    CONSTRAINT "PullRequestBounty_pkey" PRIMARY KEY ("prId","bountyId")
);

-- Copy existing links
INSERT INTO "PullRequestBounty" ("prId", "bountyId") SELECT "id", "bountyId" FROM "PullRequest";

-- CreateIndex
CREATE INDEX "PullRequestBounty_bountyId_idx" ON "PullRequestBounty"("bountyId");

-- AddForeignKey
ALTER TABLE "PullRequestBounty" ADD CONSTRAINT "PullRequestBounty_prId_fkey" FOREIGN KEY ("prId") REFERENCES "PullRequest"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "PullRequestBounty" ADD CONSTRAINT "PullRequestBounty_bountyId_fkey" FOREIGN KEY ("bountyId") REFERENCES "Bounty"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- DropForeignKey
ALTER TABLE "PullRequest" DROP CONSTRAINT "PullRequest_bountyId_fkey";

-- DropIndex
DROP INDEX "PullRequest_bountyId_number_key";

-- AlterTable
ALTER TABLE "PullRequest" DROP COLUMN "bountyId";
