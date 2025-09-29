//! read/write journal

use context::{ContextTr, JournalTr};
use primitives::{keccak256, Address, B256, U256};

use crate::{stateful_precompiles::convert_db_err, PrecompileError};

/// stores an arbitrary-length []byte into journal using multiple storage slots.
pub fn store_bytes<CTX: ContextTr>(
    context: &mut CTX,
    addr: Address,
    key: B256,
    value: &[u8],
) -> Result<(), PrecompileError> {
    // Compute the storage root key (keccak256(key)) as the base
    let length_key = keccak256(key);
    context
        .journal_mut()
        .sstore(addr, length_key.into(), U256::from(value.len()))
        .map_err(convert_db_err::<CTX::Db>)?;

    // Split value into 32-byte chunks and store them
    for (i, chunk_bytes) in value.chunks(32).enumerate() {
        let mut chunk = [0u8; 32];
        chunk[..chunk_bytes.len()].copy_from_slice(chunk_bytes);

        let mut key_input = Vec::with_capacity(32 + 8);
        key_input.extend_from_slice(length_key.as_slice());
        key_input.extend_from_slice(&(i as u64).to_be_bytes());
        let storage_key = keccak256(key_input);

        context
            .journal_mut()
            .sstore(addr, storage_key.into(), U256::from_be_bytes(chunk))
            .map_err(convert_db_err::<CTX::Db>)?;
    }

    Ok(())
}

/// retrieves an arbitrary-length []byte from journal.
pub fn load_bytes<CTX: ContextTr>(
    context: &mut CTX,
    addr: Address,
    key: B256,
) -> Result<Vec<u8>, PrecompileError> {
    // Read length
    let length_key = keccak256(key);
    let length = context
        .journal_mut()
        .sload(addr, length_key.into())
        .map_err(convert_db_err::<CTX::Db>)?
        .data
        .to::<u64>();

    // Read stored chunks
    let mut buffer = Vec::with_capacity(length as usize);
    let mut i = 0u64;
    while i < length {
        let mut key_input = Vec::with_capacity(32 + 8);
        key_input.extend_from_slice(length_key.as_slice());
        key_input.extend_from_slice(&(i / 32u64).to_be_bytes());
        let storage_key = keccak256(key_input);

        let chunk: [u8; 32] = context
            .journal_mut()
            .sload(addr, storage_key.into())
            .map_err(convert_db_err::<CTX::Db>)?
            .data
            .to_be_bytes();

        buffer.extend_from_slice(&chunk);

        i += 32;
    }

    buffer.truncate(length as usize); // Trim padding

    Ok(buffer)
}
