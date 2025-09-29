use primitives::{keccak256, Address, B256};

const SIGNER_KEY: &[u8] = &[0x00];
const QUORUM_KEY: &[u8] = &[0x01];
const REGISTRATION_KEY: &[u8] = &[0x02];
const VOTES_KEY: &[u8] = &[0x03];
const QUORUM_COUNT_KEY: &[u8] = &[0x04];
const EPOCH_NUMBER_KEY: &[u8] = &[0x05];
const EPOCH_BLOCK_KEY: &[u8] = &[0x06];
const EPOCH_REGISTRATION_KEY: &[u8] = &[0x07];
const EPOCH_REGISTERED_SIGNER_KEY: &[u8] = &[0x08];

pub(super) fn signer_key(account: Address) -> B256 {
    keccak256([SIGNER_KEY, account.as_slice()].concat())
}

pub(super) fn quorum_key(epoch_number: u64, quorum_id: u64) -> B256 {
    keccak256([QUORUM_KEY, &epoch_number.to_be_bytes(), &quorum_id.to_be_bytes()].concat())
}

pub(super) fn registration_key(epoch_number: u64, account: Address) -> B256 {
    keccak256([REGISTRATION_KEY, &epoch_number.to_be_bytes(), account.as_slice()].concat())
}

pub(super) fn votes_key(epoch_number: u64, account: Address) -> B256 {
    keccak256([VOTES_KEY, &epoch_number.to_be_bytes(), account.as_slice()].concat())
}

pub(super) fn quorum_count_key(epoch_number: u64) -> B256 {
    keccak256([QUORUM_COUNT_KEY, &epoch_number.to_be_bytes()].concat())
}

pub(super) fn epoch_number_key() -> B256 {
    keccak256(EPOCH_NUMBER_KEY)
}

pub(super) fn epoch_block_key(epoch_number: u64) -> B256 {
    keccak256([EPOCH_BLOCK_KEY, &epoch_number.to_be_bytes()].concat())
}

pub(super) fn epoch_registration_key(epoch_number: u64) -> B256 {
    keccak256([EPOCH_REGISTRATION_KEY, &epoch_number.to_be_bytes()].concat())
}

pub(super) fn epoch_registered_signer_key(epoch_number: u64, index: u64) -> B256 {
    keccak256(
        [EPOCH_REGISTERED_SIGNER_KEY, &epoch_number.to_be_bytes(), &index.to_be_bytes()].concat(),
    )
}

#[cfg(test)]
mod tests {
    use primitives::{address, hex::FromHex};

    use super::*;

    #[test]
    fn test_signer_key() {
        let addr1 = address!("0x1111111111111111111111111111111111111111");
        let addr2 = address!("0x2222222222222222222222222222222222222222");

        let h1 = signer_key(addr1);
        let h2 = signer_key(addr2);

        assert_eq!(
            h1,
            B256::from_hex("7dc95ba3cb9e5e87824447501846b50797b2ea27f5fdee5fc351ef943975dd3f")
                .unwrap()
        );
        assert_eq!(
            h2,
            B256::from_hex("4039be391d5fdf4bc1db1e939e47d6905aa868fd6c8c8945ce003f8dd3dedd19")
                .unwrap()
        );
    }

    #[test]
    fn test_quorum_key() {
        let h1 = quorum_key(1, 42);
        let h2 = quorum_key(2, 42);

        assert_eq!(
            h1,
            B256::from_hex("e26d93f2acb8151395b7f036aa81f315fbffbf448442ebfe2090c00d453f81f3")
                .unwrap()
        );
        assert_eq!(
            h2,
            B256::from_hex("46dfc0b1112d1e286da6b17a279938f2c367897bd6c7863310dc26b210c7e0db")
                .unwrap()
        );
    }

    #[test]
    fn test_registration_key() {
        let addr = address!("0x3333333333333333333333333333333333333333");
        let h1 = registration_key(5, addr);
        let h2 = registration_key(6, addr);

        assert_eq!(
            h1,
            B256::from_hex("c34376cfa25ca1e94e5e07e41e7370aaf1f8a0ad9c16d264c02dc1e330a2c532")
                .unwrap()
        );
        assert_eq!(
            h2,
            B256::from_hex("3bfd46b3b65aecb24a68321783fdd08feb1c0defc6cee878111043896668d504")
                .unwrap()
        );
    }

    #[test]
    fn test_votes_key() {
        let addr = address!("0x4444444444444444444444455544444444444444");
        let h1 = votes_key(10, addr);
        let h2 = votes_key(11, addr);

        assert_eq!(
            h1,
            B256::from_hex("ab766c7a375a49dd920c60baf057a2221d70ff94072deddf3fdcc72e71106800")
                .unwrap()
        );
        assert_eq!(
            h2,
            B256::from_hex("e871e34217a4eaed1dc107fe606720e8fdab5bd0baf44c59c1397329ef0aed32")
                .unwrap()
        );
    }

    #[test]
    fn test_quorum_count_key() {
        let h1 = quorum_count_key(7);
        let h2 = quorum_count_key(8);

        assert_eq!(
            h1,
            B256::from_hex("8ebd2aa612c5b7e3592c81bf787942bf89d55337ebe586caa66cc845ef714d23")
                .unwrap()
        );
        assert_eq!(
            h2,
            B256::from_hex("f1904c2a5bc3c189153309e7e83eba563d38dd28204ed5672dd37fa133ad79c4")
                .unwrap()
        );
    }

    #[test]
    fn test_epoch_number_key() {
        let h1 = epoch_number_key();
        assert_eq!(
            h1,
            B256::from_hex("dbb8d0f4c497851a5043c6363657698cb1387682cac2f786c731f8936109d795")
                .unwrap()
        );
    }

    #[test]
    fn test_epoch_block_key() {
        let h1 = epoch_block_key(100);
        let h2 = epoch_block_key(200);
        assert_eq!(
            h1,
            B256::from_hex("62c78d1c7f4d86950e09972a20a8d807325864c45ecac6a55d1798949394a6d2")
                .unwrap()
        );
        assert_eq!(
            h2,
            B256::from_hex("d5afe942a5af543757536cc948c698f0bdd641a9159216597580b4ecaea137d4")
                .unwrap()
        );
    }

    #[test]
    fn test_epoch_registration_key() {
        let h1 = epoch_registration_key(50);
        let h2 = epoch_registration_key(60);
        assert_eq!(
            h1,
            B256::from_hex("f58867fcd8c4351a98917938bb247b81c52958c1d748166c6e1b2d73efe5d705")
                .unwrap()
        );
        assert_eq!(
            h2,
            B256::from_hex("1104866bdeb16911bb635e510e08f454b4589c408d1dae3bfe2dcc29a118501a")
                .unwrap()
        );
    }

    #[test]
    fn test_epoch_registered_signer_key() {
        let h1 = epoch_registered_signer_key(1, 0);
        let h2 = epoch_registered_signer_key(1, 1);
        assert_eq!(
            h1,
            B256::from_hex("457e72e24b3e3ad551d45b98daabe5431909d2a7668580aad12814549ead2d3e")
                .unwrap()
        );
        assert_eq!(
            h2,
            B256::from_hex("40f3f116519c41c2c92119692c2c1d6cc22bd3bca86080b3b389bf77c270788f")
                .unwrap()
        );
    }
}
