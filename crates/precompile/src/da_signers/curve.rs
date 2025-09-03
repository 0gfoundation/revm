use ark_bn254::{Bn254, Fq, FqConfig, Fr, G1Affine, G2Affine};
use ark_ec::{pairing::Pairing, AffineRepr, CurveGroup};
use ark_ff::{BigInt, Field, MontConfig, One, PrimeField};
use ark_serialize::CanonicalSerialize;
use primitives::{keccak256, Address, U256};

use crate::contract_interface::{IDASigners::SignerDetail, BN254::G1Point};

const PUBKEY_REGISTRATION_DOMAIN: &[u8] = "0G_BN254_Pubkey_Registration".as_bytes();

pub(super) fn serialize_g1_point(point: G1Affine) -> G1Point {
    let mut value: Vec<u8> = Vec::new();
    point
        .x()
        .unwrap()
        .serialize_uncompressed(&mut value)
        .unwrap();
    let x = U256::from_le_slice(&value);
    value = Vec::new();
    point
        .y()
        .unwrap()
        .serialize_uncompressed(&mut value)
        .unwrap();
    let y = U256::from_le_slice(&value);
    G1Point { X: x, Y: y }
}

pub(super) fn signer_registration_hash(signer_address: Address, chain_id: u64) -> G1Affine {
    let mut message = vec![];
    message.append(&mut signer_address.as_slice().to_vec());
    message.append(&mut left_pad_zeros(chain_id, 32));
    message.append(&mut PUBKEY_REGISTRATION_DOMAIN.to_vec());
    map_to_g1(keccak256(message).to_vec())
}

pub(super) fn epoch_registration_hash(
    signer_address: Address,
    epoch: u64,
    chain_id: u64,
) -> G1Affine {
    let mut message = vec![];
    message.append(&mut signer_address.as_slice().to_vec());
    message.append(&mut left_pad_zeros(epoch, 8));
    message.append(&mut left_pad_zeros(chain_id, 32));
    map_to_g1(keccak256(message).to_vec())
}

pub(super) fn left_pad_zeros(x: u64, l: usize) -> Vec<u8> {
    let mut res = vec![0; l - 8];
    res.append(&mut x.to_be_bytes().to_vec());
    res
}

pub(super) fn left_pad_to_fixed_size(data: Vec<u8>, len: usize) -> Vec<u8> {
    if data.len() >= len {
        data
    } else {
        let mut padded = vec![0u8; len - data.len()];
        padded.extend_from_slice(&data);
        padded
    }
}

pub(super) fn map_to_g1(digest: Vec<u8>) -> G1Affine {
    let mut x: Fq = Fq::from_be_bytes_mod_order(&digest);

    loop {
        match find_y_from_x(x) {
            Some(y) => {
                return G1Affine::new(x, y);
            }
            None => x += Fq::ONE,
        }
    }
}

#[inline]
fn find_y_from_x(x: Fq) -> Option<Fq> {
    const SQRT_POW: BigInt<4> = match <FqConfig as MontConfig<4>>::MODULUS_PLUS_ONE_DIV_FOUR {
        Some(x) => x,
        None => panic!("Unsupport type"),
    };

    let beta = x * x * x + Fq::from(3);
    // y = sqrt(beta) = beta^((p+1) / 4)
    let y = beta.pow(SQRT_POW.as_ref());
    (y * y == beta).then_some(y)
}

/// validate signature by pairing
pub(super) fn validate_signature(
    signer: &SignerDetail,
    hash: G1Affine,
    signature: G1Affine,
) -> bool {
    let pubkey_g1 = signer.pkG1.clone().into();
    let pubkey_g2 = signer.pkG2.clone().into();
    let gamma = gamma(hash, signature, pubkey_g1, pubkey_g2);

    // pairing
    let p = [
        (signature + pubkey_g1 * gamma).into_affine(),
        (hash + G1Affine::generator() * gamma).into_affine(),
    ];
    let q = [-G2Affine::generator(), pubkey_g2];
    Bn254::multi_pairing(p, q).0.is_one()
}

fn reverse_bytes(bytes: &mut [u8]) {
    bytes.reverse();
}

/// serialize G1Affine to fixed bytes in big endian
pub(super) fn serialize_g1(p: G1Affine) -> [u8; 64] {
    let mut buf = [0u8; 64];
    let mut temp = Vec::with_capacity(64);
    p.serialize_uncompressed(&mut temp).unwrap();

    buf[0..32].copy_from_slice(&temp[0..32]);
    reverse_bytes(&mut buf[0..32]);
    buf[32..64].copy_from_slice(&temp[32..64]);
    reverse_bytes(&mut buf[32..64]);

    buf
}

/// serialize G2Affine to fixed bytes in big endian
pub(super) fn serialize_g2(p: G2Affine) -> [u8; 128] {
    let mut buf = [0u8; 128];
    let mut temp = Vec::with_capacity(128);
    p.serialize_uncompressed(&mut temp).unwrap();

    for i in 0..4 {
        let start = i * 32;
        buf[start..start + 32].copy_from_slice(&temp[start..start + 32]);
        reverse_bytes(&mut buf[start..start + 32]);
    }

    buf
}

/// calculate gamma for pairing
pub(super) fn gamma(hash: G1Affine, signature: G1Affine, pk_g1: G1Affine, pk_g2: G2Affine) -> Fr {
    let mut to_hash = Vec::with_capacity(64 * 3 + 128);
    to_hash.extend_from_slice(&serialize_g1(hash));
    to_hash.extend_from_slice(&serialize_g1(signature));
    to_hash.extend_from_slice(&serialize_g1(pk_g1));
    to_hash.extend_from_slice(&serialize_g2(pk_g2));

    let msg_hash = keccak256(&to_hash);

    Fr::from_be_bytes_mod_order(msg_hash.as_slice())
}
