import { describe, expect, it } from 'vitest';
import { contractErrorName } from '../src/chain/errors.js';

describe('contractErrorName', () => {
  it('maps contract error numbers', () => {
    expect(contractErrorName('HostError: Error(Contract, #11)')).toBe('InvalidStatus');
    expect(contractErrorName('Error(Contract, #17)')).toBe('RelayerNotAllowed');
    expect(contractErrorName('Error(Contract, #99)')).toBeUndefined();
    expect(contractErrorName('boom')).toBeUndefined();
  });
});
