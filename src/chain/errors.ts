// Mirrors contracts/bounty_escrow/src/errors.rs. Never renumber.
export const CONTRACT_ERRORS: Readonly<Record<number, string>> = {
  1: 'AlreadyInitialized', 2: 'NotInitialized', 3: 'Unauthorized', 4: 'Paused',
  5: 'TokenNotAllowed', 6: 'AmountTooSmall', 7: 'AmountTooLarge', 8: 'InvalidDeadline',
  9: 'IssueAlreadyFunded', 10: 'BountyNotFound', 11: 'InvalidStatus', 12: 'DeadlineNotReached',
  13: 'SelfPayout', 14: 'InvalidSplit', 15: 'InvalidFee', 16: 'Overflow', 17: 'RelayerNotAllowed',
};

/** Extracts "Error(Contract, #N)" from a simulation/RPC message. */
export function contractErrorName(message: string): string | undefined {
  const m = /Error\(Contract, #(\d+)\)/.exec(message);
  return m ? CONTRACT_ERRORS[Number(m[1])] : undefined;
}
