use ahash::{HashSet, HashSetExt};
use alloy_primitives::Address;
use serde::{Deserialize, Deserializer};
use std::convert::TryFrom;
use std::fs::read_to_string;
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;

#[allow(clippy::len_without_is_empty)]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BlockList {
    list: HashSet<Address>,
}

impl BlockList {
    fn new() -> Self {
        Self {
            list: HashSet::new(),
        }
    }

    fn from_file(path: PathBuf) -> eyre::Result<Self> {
        let blocklist_file = read_to_string(path)?;
        let blocklist: Vec<Address> = serde_json::from_str(&blocklist_file)?;

        Ok(Self {
            list: blocklist.into_iter().collect(),
        })
    }
}

impl TryFrom<PathBuf> for BlockList {
    type Error = eyre::Report; // Using eyre::Report since from_file returns eyre::Result

    fn try_from(path: PathBuf) -> Result<Self, Self::Error> {
        Self::from_file(path)
    }
}

impl From<Vec<Address>> for BlockList {
    fn from(addresses: Vec<Address>) -> Self {
        Self {
            list: addresses.into_iter().collect(),
        }
    }
}

impl Deref for BlockList {
    type Target = HashSet<Address>;

    fn deref(&self) -> &Self::Target {
        &self.list
    }
}

impl DerefMut for BlockList {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.list
    }
}

impl<'de> Deserialize<'de> for BlockList {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let path: Option<PathBuf> = Option::deserialize(deserializer)?;

        match path {
            Some(path) => BlockList::from_file(path).map_err(serde::de::Error::custom),
            None => Ok(BlockList::new()),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use alloy_primitives::{address, Address};
    use serde::Deserialize;

    #[test]
    fn test_read_blocklist_from_file() {
        let block_list = BlockList::from_file(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/blocklist/testdata/blocklist.txt"),
        )
        .unwrap();

        let addr0 = address!("14dC79964da2C08b23698B3D3cc7Ca32193d9955");
        assert_eq!(block_list.contains(&addr0), true);

        let addr1 = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
        assert_eq!(block_list.contains(&addr1), true);

        let addr2 = address!("a0Ee7A142d267C1f36714E4a8F75612F20a79720");
        assert_eq!(block_list.contains(&addr2), false);
    }

    #[test]
    fn test_blocklist() {
        let mut blocklist = BlockList::new();
        let addr0 = Address::random();

        blocklist.insert(addr0);
        assert_eq!(blocklist.len(), 1);
        assert_eq!(blocklist.contains(&addr0), true);

        // you cannot add twice the same value
        blocklist.insert(addr0);
        assert_eq!(blocklist.len(), 1);

        let addr1 = Address::random();
        assert_eq!(blocklist.contains(&addr1), false);

        blocklist.insert(addr1);
        assert_eq!(blocklist.len(), 2);
        assert_eq!(blocklist.contains(&addr1), true);
    }

    #[derive(Deserialize)]
    struct Config {
        block_list: BlockList,
    }

    #[test]
    fn test_deserialize_config() {
        let config_str = r#"
            block_list = "src/blocklist/testdata/blocklist.txt"
        "#;
        let config: Config = toml::from_str(config_str).unwrap();
        assert_eq!(config.block_list.len(), 3);

        let addr1 = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
        assert_eq!(config.block_list.contains(&addr1), true);

        let empty_config_str = r#""#;
        let config: Config = toml::from_str(empty_config_str).unwrap();
        assert_eq!(config.block_list.len(), 0);
    }

    #[test]
    fn test_from_vec() {
        let addr0 = address!("14dC79964da2C08b23698B3D3cc7Ca32193d9955");
        let addr1 = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

        let addresses = vec![addr0, addr1];
        let blocklist = BlockList::from(addresses);

        assert_eq!(blocklist.len(), 2);
        assert_eq!(blocklist.contains(&addr0), true);
        assert_eq!(blocklist.contains(&addr1), true);
    }
}
