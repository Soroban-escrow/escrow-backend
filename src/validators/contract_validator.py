"""
Soroban Escrow Backend - Contract ID Format Validator
Fix for Issue #198: Validate TOKEN_USDC_CONTRACT_ID and TOKEN_XLM_CONTRACT_ID format
Accepts valid Soroban contract addresses (C... 56 chars) and Stellar public keys (G... 56 chars).
"""
import re
from typing import Dict, Any

CONTRACT_ID_REGEX = re.compile(r'^[CG][A-Z0-9]{55}$')

def validate_contract_id(contract_id: str, token_name: str = 'TOKEN') -> Dict[str, Any]:
    if not contract_id or not isinstance(contract_id, str):
        return {
            'valid': False,
            'error': f'{token_name} contract ID cannot be empty'
        }
    contract_id = contract_id.strip()
    if len(contract_id) != 56:
        return {
            'valid': False,
            'error': f'{token_name} contract ID must be exactly 56 characters (got {len(contract_id)})'
        }
    if not CONTRACT_ID_REGEX.match(contract_id):
        return {
            'valid': False,
            'error': f'{token_name} contract ID must start with C or G and contain uppercase alphanumeric characters'
        }
    return {'valid': True, 'contract_id': contract_id}

if __name__ == '__main__':
    # Test valid USDC Soroban contract id
    valid_usdc = 'CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZU4CTSTDFJ4Q46Y2F6N'
    r1 = validate_contract_id(valid_usdc, 'TOKEN_USDC_CONTRACT_ID')
    assert r1['valid'] is True, f'Expected valid, got {r1}'
    
    # Test invalid short
    r2 = validate_contract_id('C123', 'TOKEN_XLM_CONTRACT_ID')
    assert r2['valid'] is False
    
    # Test invalid characters
    r3 = validate_contract_id('CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZU4CTSTDFJ4Q46Y2F6!', 'TOKEN_USDC_CONTRACT_ID')
    assert r3['valid'] is False
    
    print('Soroban #198 Unit Test: PASS (Exit 0)')
