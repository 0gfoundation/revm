use ark_bn254::{Bn254, Fq, FqConfig, Fr, G1Affine, G2Affine};
use ark_ec::{pairing::Pairing, AffineRepr, CurveGroup};
use ark_ff::{BigInt, Field, MontConfig, One, PrimeField};
use ark_serialize::CanonicalSerialize;
use primitives::{keccak256, Address, U256};

use crate::contract_interface::{IDASigners::SignerDetail, BN254::G1Point};

const PUBKEY_REGISTRATION_DOMAIN: &[u8] = "0G_BN254_Pubkey_Registration".as_bytes();

pub(super) fn serialize_g1_point(point: G1Affine) -> G1Point {
    let mut value: Vec<u8> = Vec::new();
    point.x().unwrap().serialize_uncompressed(&mut value).unwrap();
    let x = U256::from_le_slice(&value);
    value = Vec::new();
    point.y().unwrap().serialize_uncompressed(&mut value).unwrap();
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
    let mut value: Vec<u8> = Vec::new();
    p.x().unwrap().serialize_uncompressed(&mut value).unwrap();
    buf[0..32].copy_from_slice(&value[0..32]);
    reverse_bytes(&mut buf[0..32]);

    value.clear();
    p.y().unwrap().serialize_uncompressed(&mut value).unwrap();
    buf[32..64].copy_from_slice(&value[0..32]);
    reverse_bytes(&mut buf[32..64]);

    buf
}

/// serialize G2Affine to fixed bytes in big endian
pub(super) fn serialize_g2(p: G2Affine) -> [u8; 128] {
    let mut buf = [0u8; 128];
    let mut value: Vec<u8> = Vec::new();
    p.x().unwrap().c0.serialize_uncompressed(&mut value).unwrap();
    buf[0..32].copy_from_slice(&value[0..32]);
    reverse_bytes(&mut buf[0..32]);
    value.clear();

    p.x().unwrap().c1.serialize_uncompressed(&mut value).unwrap();
    buf[32..64].copy_from_slice(&value[0..32]);
    reverse_bytes(&mut buf[32..64]);
    value.clear();

    p.y().unwrap().c0.serialize_uncompressed(&mut value).unwrap();
    buf[64..96].copy_from_slice(&value[0..32]);
    reverse_bytes(&mut buf[64..96]);
    value.clear();

    p.y().unwrap().c1.serialize_uncompressed(&mut value).unwrap();
    buf[96..128].copy_from_slice(&value[0..32]);
    reverse_bytes(&mut buf[96..128]);
    value.clear();

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

#[cfg(test)]
mod tests {
    use ark_ff::QuadExtField;
    use primitives::hex;

    use super::*;

    #[test]
    fn test_serialize_g1() {
        let p = G1Affine::new_unchecked(
            Fq::from_be_bytes_mod_order(
                &U256::from_str_radix(
                    "19300522510534054799330569506194579913800365625278702540049559191851317457335",
                    10,
                )
                .unwrap()
                .to_be_bytes_vec(),
            ),
            Fq::from_be_bytes_mod_order(
                &U256::from_str_radix(
                    "21506615804111993086024125047185347092253679892553376328557576951218017569466",
                    10,
                )
                .unwrap()
                .to_be_bytes_vec(),
            ),
        );
        assert_eq!(hex::encode(serialize_g1(p)), "2aabb56813568e22856b1e090f5ee32dc951423b65f2d5bb80418436fcb5f1b72f8c502c35f9499fc98bd619620210e0d50a34c4e191e57e0d79519e843e8aba");
    }

    #[test]
    fn test_serialize_g2() {
        let p = G2Affine::new_unchecked(
            QuadExtField::new(
                Fq::from_be_bytes_mod_order(&U256::from_str_radix("20330596197210395241356549584419927603351085555088806176574690490794984008944", 10).unwrap().to_be_bytes_vec()),
                Fq::from_be_bytes_mod_order(&U256::from_str_radix("15787159264193133731964071396477495274492189810403383639371877574524834519407", 10).unwrap().to_be_bytes_vec()),
            ),
            QuadExtField::new(
                Fq::from_be_bytes_mod_order(&U256::from_str_radix("11029159417960220792740346453748230672677331804832155710703958796437158259101", 10).unwrap().to_be_bytes_vec()),
                Fq::from_be_bytes_mod_order(&U256::from_str_radix("7266804587715537947859661776892396802219153443931392992660400968992197326887", 10).unwrap().to_be_bytes_vec()),
            ),
        );
        assert_eq!(hex::encode(serialize_g2(p)), "2cf2b5ac9e4c3fde611b48355bf24ea1a7f33de84a3f75894c6cdd4601eaecf022e7372a723ecd283fd33b3d2c34b08232c7f91e41689da4741b055956fd996f186248738006a72254c98fda715603f5d16735c1339949e3c650cf292f16699d1010dd9ab9d9dac4cfc26cc26784738e1000d430b86fe34030e8737bc3ae3827");
    }
}
