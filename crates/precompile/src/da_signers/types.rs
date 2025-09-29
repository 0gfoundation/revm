use ark_bn254::{Fq, Fq2, G1Affine, G2Affine};
use ark_ff::PrimeField;
use serde::{Deserialize, Serialize};

use crate::{
    as_bin::AsBinStr,
    contract_interface::{
        IDASigners::SignerDetail,
        BN254::{G1Point, G2Point},
    },
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BN254G1Point {
    #[serde(rename = "X")]
    pub x: AsBinStr,
    #[serde(rename = "Y")]
    pub y: AsBinStr,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BN254G2Point {
    #[serde(rename = "X")]
    pub x: [AsBinStr; 2],
    #[serde(rename = "Y")]
    pub y: [AsBinStr; 2],
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IDASignersSignerDetail {
    #[serde(rename = "Signer")]
    pub signer: AsBinStr,
    #[serde(rename = "Socket")]
    pub socket: String,
    #[serde(rename = "PkG1")]
    pub pk_g1: BN254G1Point,
    #[serde(rename = "PkG2")]
    pub pk_g2: BN254G2Point,
}

impl From<G1Point> for BN254G1Point {
    fn from(value: G1Point) -> Self {
        Self { x: value.X.into(), y: value.Y.into() }
    }
}

impl From<BN254G1Point> for G1Point {
    fn from(value: BN254G1Point) -> Self {
        Self { X: value.x.into(), Y: value.y.into() }
    }
}

impl From<G2Point> for BN254G2Point {
    fn from(value: G2Point) -> Self {
        Self { x: value.X.map(|v| v.into()), y: value.Y.map(|v| v.into()) }
    }
}

impl From<BN254G2Point> for G2Point {
    fn from(value: BN254G2Point) -> Self {
        Self { X: value.x.map(|v| v.into()), Y: value.y.map(|v| v.into()) }
    }
}

impl From<SignerDetail> for IDASignersSignerDetail {
    fn from(value: SignerDetail) -> Self {
        Self {
            //signer: value.signer.to_string().to_lowercase().into(),
            signer: value.signer.into(),
            socket: value.socket,
            pk_g1: value.pkG1.into(),
            pk_g2: value.pkG2.into(),
        }
    }
}

impl From<IDASignersSignerDetail> for SignerDetail {
    fn from(value: IDASignersSignerDetail) -> Self {
        Self {
            signer: value.signer.into(),
            socket: value.socket,
            pkG1: value.pk_g1.into(),
            pkG2: value.pk_g2.into(),
        }
    }
}

impl From<G1Point> for G1Affine {
    fn from(value: G1Point) -> Self {
        G1Affine::new_unchecked(
            Fq::from_be_bytes_mod_order(&value.X.to_be_bytes_vec()),
            Fq::from_be_bytes_mod_order(&value.Y.to_be_bytes_vec()),
        )
    }
}

impl From<G2Point> for G2Affine {
    fn from(value: G2Point) -> Self {
        G2Affine::new_unchecked(
            Fq2::new(
                Fq::from_be_bytes_mod_order(&value.X[0].to_be_bytes_vec()),
                Fq::from_be_bytes_mod_order(&value.X[1].to_be_bytes_vec()),
            ),
            Fq2::new(
                Fq::from_be_bytes_mod_order(&value.Y[0].to_be_bytes_vec()),
                Fq::from_be_bytes_mod_order(&value.Y[1].to_be_bytes_vec()),
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use primitives::{address, hex, Address};
    use rmp_serde::{Deserializer as RMPDeserializer, Serializer as RMPSerializer};

    #[test]
    fn test_msgpack_da_signers() {
        let cases = [(IDASignersSignerDetail {
            signer: "0x0000000000000000000000000000000100000001".into(),
            socket: "0.0.0.0:1234".to_string(),
            pk_g1: BN254G1Point {
                x: "19033251874843656108471242320417533909414939332036131356573128480367742634479".into(),
                y: "20792135454608030201903199625673964159744755218442260092768620403349374102584".into(),
            },
            pk_g2: BN254G2Point {
                x: [
                    "8472151341754925747860535367990505955708751825377817860727104273184244800723".into(),
                    "15624790064206502667756020446826209080711344272800176518784649088946231692936".into(),
                ],
                y: [
                    "1196137947243150610106053819405501111182787323156221967342356892090037828244".into(),
                    "19488077321171448217727198730828487286865984357780136663388739985720647978898".into(),
                ],
            },
        }, hex::decode("84a65369676e6572c42a307830303030303030303030303030303030303030303030303030303030303030313030303030303031a6536f636b6574ac302e302e302e303a31323334a4506b473182a158c44d3139303333323531383734383433363536313038343731323432333230343137353333393039343134393339333332303336313331333536353733313238343830333637373432363334343739a159c44d3230373932313335343534363038303330323031393033313939363235363733393634313539373434373535323138343432323630303932373638363230343033333439333734313032353834a4506b473282a15892c44c38343732313531333431373534393235373437383630353335333637393930353035393535373038373531383235333737383137383630373237313034323733313834323434383030373233c44d3135363234373930303634323036353032363637373536303230343436383236323039303830373131333434323732383030313736353138373834363439303838393436323331363932393336a15992c44c31313936313337393437323433313530363130313036303533383139343035353031313131313832373837333233313536323231393637333432333536383932303930303337383238323434c44d3139343838303737333231313731343438323137373237313938373330383238343837323836383635393834333537373830313336363633333838373339393835373230363437393738383938").unwrap()),
        (IDASignersSignerDetail {
            signer: "0x9f3a7c2b8e1d4f6a0b7c9d2e3f5a1b0c4d6e7f8a".into(),
            socket: "10.123.45.67:65530/service/node/region-1/cluster-42/instance-9876543210/session-abcdef1234567890/extra/path/for/testing/very/long/socket/address/example".to_string(),
            pk_g1: BN254G1Point {
                x: "1".into(),
                y: "2".into(),
            },
            pk_g2: BN254G2Point {
                x: [
                    "10857046999023057135944570762232829481370756359578518086990519993285655852781".into(),
                    "11559732032986387107991004021392285783925812861821192530917403151452391805634".into(),
                ],
                y: [
                    "8495653923123431417604973247489272438418190587263600148770280649306958101930".into(),
                    "4082367875863433681332203403145435568316851327593401208105741076214120093531".into(),
                ],
            },
        }, hex::decode("84a65369676e6572c42a307839663361376332623865316434663661306237633964326533663561316230633464366537663861a6536f636b6574d99831302e3132332e34352e36373a36353533302f736572766963652f6e6f64652f726567696f6e2d312f636c75737465722d34322f696e7374616e63652d393837363534333231302f73657373696f6e2d616263646566313233343536373839302f65787472612f706174682f666f722f74657374696e672f766572792f6c6f6e672f736f636b65742f616464726573732f6578616d706c65a4506b473182a158c40131a159c40132a4506b473282a15892c44d3130383537303436393939303233303537313335393434353730373632323332383239343831333730373536333539353738353138303836393930353139393933323835363535383532373831c44d3131353539373332303332393836333837313037393931303034303231333932323835373833393235383132383631383231313932353330393137343033313531343532333931383035363334a15992c44c38343935363533393233313233343331343137363034393733323437343839323732343338343138313930353837323633363030313438373730323830363439333036393538313031393330c44c34303832333637383735383633343333363831333332323033343033313435343335353638333136383531333237353933343031323038313035373431303736323134313230303933353331").unwrap()),
        (IDASignersSignerDetail {
            signer: "0x9f3a7c2b8e1d4f6a0b7c9d2e3f5a1b0c4d6e7f8a".into(),
            socket: "".to_string(),
            pk_g1: BN254G1Point {
                x: "1".into(),
                y: "2".into(),
            },
            pk_g2: BN254G2Point {
                x: [
                    "10857046999023057135944570762232829481370756359578518086990519993285655852781".into(),
                    "11559732032986387107991004021392285783925812861821192530917403151452391805634".into(),
                ],
                y: [
                    "8495653923123431417604973247489272438418190587263600148770280649306958101930".into(),
                    "4082367875863433681332203403145435568316851327593401208105741076214120093531".into(),
                ],
            },
        }, hex::decode("84a65369676e6572c42a307839663361376332623865316434663661306237633964326533663561316230633464366537663861a6536f636b6574a0a4506b473182a158c40131a159c40132a4506b473282a15892c44d3130383537303436393939303233303537313335393434353730373632323332383239343831333730373536333539353738353138303836393930353139393933323835363535383532373831c44d3131353539373332303332393836333837313037393931303034303231333932323835373833393235383132383631383231313932353330393137343033313531343532333931383035363334a15992c44c38343935363533393233313233343331343137363034393733323437343839323732343338343138313930353837323633363030313438373730323830363439333036393538313031393330c44c34303832333637383735383633343333363831333332323033343033313435343335353638333136383531333237353933343031323038313035373431303736323134313230303933353331").unwrap())];

        // println!("{}", cases[0].0);

        for case in cases.iter() {
            // conversion
            let t = SignerDetail::from(case.0.clone());
            assert_eq!(IDASignersSignerDetail::from(t), case.0);

            // serialize
            let mut buf = Vec::new();
            case.0.serialize(&mut RMPSerializer::new(&mut buf).with_struct_map()).unwrap();

            // println!("Msgpack hex: {}, length: {}", hex::encode(&buf), buf.len());
            assert_eq!(case.1, buf);

            // deserialize back
            let mut de = RMPDeserializer::new(&buf[..]);
            let decoded: IDASignersSignerDetail = Deserialize::deserialize(&mut de).unwrap();

            assert_eq!(decoded, case.0);
        }
    }

    #[test]
    fn test_quorums() {
        let cases = [(vec![
            address!("0x9f3a7c2b8e1d4f6a0b7c9d2e3f5a1b0c4d6e7f8a"),
            address!("0x9f3a7c2b8e1d4f6a0b7c9d2e3f5a1b0c4d6e7f8b"),
            address!("0x9f3a7c2b8e1d4f6a0b7c9d2e3f5a1b0c4d6e7f8c"),
        ], hex::decode("93c42a307839663361376332623865316434663661306237633964326533663561316230633464366537663861c42a307839663361376332623865316434663661306237633964326533663561316230633464366537663862c42a307839663361376332623865316434663661306237633964326533663561316230633464366537663863").unwrap()),
        (vec![], hex::decode("90").unwrap())
        ];

        for case in cases.iter() {
            // conversion
            let t = case.0.clone().into_iter().map(AsBinStr::from).collect::<Vec<AsBinStr>>();
            assert_eq!(t.clone().into_iter().map(Address::from).collect::<Vec<Address>>(), case.0);

            // serialize
            let mut buf = Vec::new();
            t.serialize(&mut RMPSerializer::new(&mut buf).with_struct_map()).unwrap();

            // println!("Msgpack hex: {}, length: {}", hex::encode(&buf), buf.len());
            assert_eq!(case.1, buf);

            // deserialize back
            let mut de = RMPDeserializer::new(&buf[..]);
            let decoded: Vec<AsBinStr> = Deserialize::deserialize(&mut de).unwrap();

            assert_eq!(decoded.into_iter().map(Address::from).collect::<Vec<Address>>(), case.0);
        }
    }
}
