use serde::{Deserialize, Serialize};

use crate::{as_bin::AsBinStr, contract_interface::WrappedA0GIBase::Supply};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MinterSupply {
    #[serde(rename = "Cap")]
    pub cap: AsBinStr,
    #[serde(rename = "InitialSupply")]
    pub initial_supply: AsBinStr,
    #[serde(rename = "Supply")]
    pub supply: AsBinStr,
}

impl From<Supply> for MinterSupply {
    fn from(value: Supply) -> Self {
        Self {
            cap: value.cap.into(),
            initial_supply: value.initialSupply.into(),
            supply: value.supply.into(),
        }
    }
}

impl From<MinterSupply> for Supply {
    fn from(value: MinterSupply) -> Self {
        Self {
            cap: value.cap.into(),
            initialSupply: value.initial_supply.into(),
            supply: value.supply.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use primitives::{address, hex, Address, U256};
    use rmp_serde::{Deserializer as RMPDeserializer, Serializer as RMPSerializer};

    #[test]
    fn test_supply() {
        let cases = [(MinterSupply {
            cap: U256::from(1_000_000_000_000_000_000u128).into(),
            initial_supply: U256::from(500_000_000_000_000_000u128).into(),
            supply: U256::from(500_000_000_000_000_000u128).into(),
        }, hex::decode("83a3436170c41331303030303030303030303030303030303030ad496e697469616c537570706c79c412353030303030303030303030303030303030a6537570706c79c412353030303030303030303030303030303030").unwrap()),
        (MinterSupply {
            cap: U256::from(0).into(),
            initial_supply: U256::from(0).into(),
            supply: U256::from(0).into(),
        }, hex::decode("83a3436170c40130ad496e697469616c537570706c79c40130a6537570706c79c40130").unwrap())];

        // println!("{}", cases[0].0);

        for case in cases.iter() {
            // conversion
            let t = Supply::from(case.0.clone());
            assert_eq!(MinterSupply::from(t), case.0);

            // serialize
            let mut buf = Vec::new();
            case.0
                .serialize(&mut RMPSerializer::new(&mut buf).with_struct_map())
                .unwrap();

            // println!("Msgpack hex: {}, length: {}", hex::encode(&buf), buf.len());
            assert_eq!(case.1, buf);

            // deserialize back
            let mut de = RMPDeserializer::new(&buf[..]);
            let decoded: MinterSupply = Deserialize::deserialize(&mut de).unwrap();

            assert_eq!(decoded, case.0);
        }
    }
}
