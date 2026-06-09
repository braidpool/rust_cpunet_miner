use bitcoin::hashes::sha256;
use bitcoin::hashes::HashEngine;

pub const CPUNET_SUFFIX: &[u8] = b"cpunet\0";

pub fn midstate_from_prefix(prefix: &[u8; 64]) -> sha256::Midstate {
    let mut engine = sha256::HashEngine::default();
    engine.input(prefix);
    engine
        .midstate()
        .expect("header prefix is a full number of sha256 blocks")
}

pub fn hash_from_midstate(midstate: &sha256::Midstate, tail: &[u8], suffix: &[u8]) -> [u8; 32] {
    let mut engine = sha256::HashEngine::from_midstate(*midstate);
    engine.input(tail);
    engine.input(suffix);
    let first = sha256::Hash::from_engine(engine);
    let first_bytes = first.to_byte_array();
    let final_hash = sha256::Hash::hash(&first_bytes);
    final_hash.to_byte_array()
}
