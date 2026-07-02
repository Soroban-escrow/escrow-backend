-- CreateEnum
CREATE TYPE "BountyStatus" AS ENUM ('OPEN', 'RELEASED', 'REFUNDED', 'DISPUTED', 'RESOLVED');

-- CreateEnum
CREATE TYPE "PayoutStatus" AS ENUM ('PENDING_WALLET', 'QUEUED', 'SUBMITTED', 'CONFIRMED', 'FAILED', 'AWAITING_MAINTAINER');

-- CreateTable
CREATE TABLE "User" (
    "id" TEXT NOT NULL,
    "githubId" BIGINT NOT NULL,
    "githubLogin" TEXT NOT NULL,
    "avatarUrl" TEXT,
    "wallet" TEXT,
    "walletLinkedAt" TIMESTAMP(3),
    "isAdmin" BOOLEAN NOT NULL DEFAULT false,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "User_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "Session" (
    "id" TEXT NOT NULL,
    "userId" TEXT NOT NULL,
    "expiresAt" TIMESTAMP(3) NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "Session_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "WalletChallenge" (
    "id" TEXT NOT NULL,
    "userId" TEXT NOT NULL,
    "address" TEXT NOT NULL,
    "message" TEXT NOT NULL,
    "nonce" TEXT NOT NULL,
    "expiresAt" TIMESTAMP(3) NOT NULL,
    "usedAt" TIMESTAMP(3),

    CONSTRAINT "WalletChallenge_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "Installation" (
    "id" BIGINT NOT NULL,
    "accountLogin" TEXT NOT NULL,
    "accountType" TEXT NOT NULL,
    "suspendedAt" TIMESTAMP(3),
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "Installation_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "Repo" (
    "id" BIGINT NOT NULL,
    "installationId" BIGINT NOT NULL,
    "fullName" TEXT NOT NULL,
    "private" BOOLEAN NOT NULL,

    CONSTRAINT "Repo_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "FundingIntent" (
    "id" TEXT NOT NULL,
    "repoId" BIGINT NOT NULL,
    "issueNumber" INTEGER NOT NULL,
    "requestedBy" BIGINT NOT NULL,
    "token" TEXT NOT NULL,
    "amount" DECIMAL(39,0) NOT NULL,
    "durationDays" INTEGER NOT NULL,
    "commentId" BIGINT NOT NULL,
    "expiresAt" TIMESTAMP(3) NOT NULL,
    "usedAt" TIMESTAMP(3),
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "FundingIntent_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "Bounty" (
    "id" BIGINT NOT NULL,
    "repoId" BIGINT NOT NULL,
    "issueNumber" INTEGER NOT NULL,
    "issueTitle" TEXT NOT NULL,
    "issueUrl" TEXT NOT NULL,
    "criteria" TEXT[],
    "maintainer" TEXT NOT NULL,
    "maintainerGithubId" BIGINT,
    "token" TEXT NOT NULL,
    "amount" DECIMAL(39,0) NOT NULL,
    "feeBps" INTEGER NOT NULL,
    "deadline" TIMESTAMP(3) NOT NULL,
    "relayerAllowed" BOOLEAN NOT NULL,
    "status" "BountyStatus" NOT NULL,
    "winnerAddress" TEXT,
    "winnerGithubId" BIGINT,
    "fundTx" TEXT NOT NULL,
    "closeTx" TEXT,
    "cardCommentId" BIGINT,
    "lastEventId" TEXT,
    "createdAt" TIMESTAMP(3) NOT NULL,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "Bounty_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "PullRequest" (
    "id" BIGINT NOT NULL,
    "bountyId" BIGINT NOT NULL,
    "number" INTEGER NOT NULL,
    "authorGithubId" BIGINT NOT NULL,
    "authorLogin" TEXT NOT NULL,
    "headSha" TEXT NOT NULL,
    "state" TEXT NOT NULL,
    "mergedAt" TIMESTAMP(3),
    "diffHash" TEXT,

    CONSTRAINT "PullRequest_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "AiReview" (
    "id" TEXT NOT NULL,
    "prId" BIGINT NOT NULL,
    "headSha" TEXT NOT NULL,
    "model" TEXT NOT NULL,
    "verdict" TEXT NOT NULL,
    "score" INTEGER NOT NULL,
    "result" JSONB NOT NULL,
    "flags" TEXT[],
    "inputTokens" INTEGER NOT NULL,
    "outputTokens" INTEGER NOT NULL,
    "commentId" BIGINT,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "AiReview_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "Payout" (
    "id" TEXT NOT NULL,
    "bountyId" BIGINT NOT NULL,
    "prNumber" INTEGER NOT NULL,
    "winnerGithubId" BIGINT NOT NULL,
    "winnerAddress" TEXT,
    "status" "PayoutStatus" NOT NULL,
    "idempotencyKey" TEXT NOT NULL,
    "txHash" TEXT,
    "attempts" INTEGER NOT NULL DEFAULT 0,
    "lastError" TEXT,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "Payout_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "Dispute" (
    "id" TEXT NOT NULL,
    "bountyId" BIGINT NOT NULL,
    "openedByGithubId" BIGINT NOT NULL,
    "reason" TEXT NOT NULL,
    "evidence" JSONB NOT NULL,
    "status" TEXT NOT NULL,
    "resolutionNote" TEXT,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "Dispute_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "WebhookDelivery" (
    "id" TEXT NOT NULL,
    "event" TEXT NOT NULL,
    "receivedAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "processedAt" TIMESTAMP(3),
    "error" TEXT,

    CONSTRAINT "WebhookDelivery_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "IndexerCursor" (
    "id" TEXT NOT NULL,
    "cursor" TEXT,
    "ledger" INTEGER NOT NULL,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "IndexerCursor_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "AuditLog" (
    "id" TEXT NOT NULL,
    "actor" TEXT NOT NULL,
    "action" TEXT NOT NULL,
    "target" TEXT NOT NULL,
    "data" JSONB NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "AuditLog_pkey" PRIMARY KEY ("id")
);

-- CreateIndex
CREATE UNIQUE INDEX "User_githubId_key" ON "User"("githubId");

-- CreateIndex
CREATE UNIQUE INDEX "User_wallet_key" ON "User"("wallet");

-- CreateIndex
CREATE INDEX "Session_userId_idx" ON "Session"("userId");

-- CreateIndex
CREATE UNIQUE INDEX "WalletChallenge_nonce_key" ON "WalletChallenge"("nonce");

-- CreateIndex
CREATE INDEX "WalletChallenge_userId_idx" ON "WalletChallenge"("userId");

-- CreateIndex
CREATE INDEX "Repo_installationId_idx" ON "Repo"("installationId");

-- CreateIndex
CREATE INDEX "FundingIntent_repoId_issueNumber_idx" ON "FundingIntent"("repoId", "issueNumber");

-- CreateIndex
CREATE INDEX "Bounty_repoId_issueNumber_idx" ON "Bounty"("repoId", "issueNumber");

-- CreateIndex
CREATE INDEX "Bounty_status_idx" ON "Bounty"("status");

-- CreateIndex
CREATE UNIQUE INDEX "PullRequest_bountyId_number_key" ON "PullRequest"("bountyId", "number");

-- CreateIndex
CREATE UNIQUE INDEX "AiReview_prId_headSha_key" ON "AiReview"("prId", "headSha");

-- CreateIndex
CREATE UNIQUE INDEX "Payout_idempotencyKey_key" ON "Payout"("idempotencyKey");

-- AddForeignKey
ALTER TABLE "Session" ADD CONSTRAINT "Session_userId_fkey" FOREIGN KEY ("userId") REFERENCES "User"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "WalletChallenge" ADD CONSTRAINT "WalletChallenge_userId_fkey" FOREIGN KEY ("userId") REFERENCES "User"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "Repo" ADD CONSTRAINT "Repo_installationId_fkey" FOREIGN KEY ("installationId") REFERENCES "Installation"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "Bounty" ADD CONSTRAINT "Bounty_repoId_fkey" FOREIGN KEY ("repoId") REFERENCES "Repo"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "PullRequest" ADD CONSTRAINT "PullRequest_bountyId_fkey" FOREIGN KEY ("bountyId") REFERENCES "Bounty"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "AiReview" ADD CONSTRAINT "AiReview_prId_fkey" FOREIGN KEY ("prId") REFERENCES "PullRequest"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "Payout" ADD CONSTRAINT "Payout_bountyId_fkey" FOREIGN KEY ("bountyId") REFERENCES "Bounty"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "Dispute" ADD CONSTRAINT "Dispute_bountyId_fkey" FOREIGN KEY ("bountyId") REFERENCES "Bounty"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
