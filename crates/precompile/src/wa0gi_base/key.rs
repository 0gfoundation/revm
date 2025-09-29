use primitives::{keccak256, Address, B256};

const SUPPLY_KEY: &[u8] = &[0x00];

pub(super) fn supply_key(account: Address) -> B256 {
    keccak256([SUPPLY_KEY, account.as_slice()].concat())
}

#[cfg(test)]
mod tests {
    use primitives::{address, hex::FromHex};

    use super::*;

    #[test]
    fn test_supply_key() {
        let addr1 = address!("0x1111111111111111111111111111111111111123");
        let addr2 = address!("0x2222222222222222222222222222222222222345");

        let h1 = supply_key(addr1);
        let h2 = supply_key(addr2);

        assert_eq!(
            h1,
            B256::from_hex("8b12f4266b423abc63ec1f1ab0f9ebcfc072fd829488c34106cd8a978d99436f")
                .unwrap()
        );
        assert_eq!(
            h2,
            B256::from_hex("28b180f4466013f6c23b06c61411d3056fd6b21b2f74b4b11a042593e031eb53")
                .unwrap()
        );
    }
}
