use crate::v1::sparse_mpt::{DeletionError as DeletionErrorV1, DiffTrie};
use crate::v2::trie::{DeletionError as DeletionErrorV2, Trie};

use alloy_primitives::{hex, keccak256, Bytes, FixedBytes};
use quickcheck::{quickcheck, Arbitrary, Gen};
use std::collections::HashMap;

#[derive(Clone, Debug)]
enum Op {
    Insert(FixedKey, Vec<u8>),
    Delete(FixedKey),
}

// helper trait to extend `choose` with exception handling
trait ChooseNonempty {
    fn one_of<'a, T>(&'a mut self, entries: &'a [T]) -> &'a T;
}

impl ChooseNonempty for Gen {
    fn one_of<'a, T>(&'a mut self, entries: &'a [T]) -> &'a T {
        self.choose(entries).expect("empty list in choose nonempty")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct FixedKey(FixedBytes<32>);

impl FixedKey {
    fn as_slice(&self) -> &[u8; 32] {
        &self.0
    }

    fn from_string(s: &str) -> Self {
        Self(keccak256("a"))
    }

    fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(FixedBytes::new(bytes))
    }

    fn into_bytes(self) -> Bytes {
        Bytes::from(self.0)
    }
}

impl From<Bytes> for FixedKey {
    fn from(bytes: Bytes) -> FixedKey {
        let fbytes = FixedBytes::from_slice(bytes.as_ref());
        FixedKey(fbytes)
    }
}

// We chose a small number of keys, to make sure our error cases handle key collisions,
// as well as shared prefixes, properties that would be very unlikely for random keys
impl Arbitrary for FixedKey {
    fn arbitrary(g: &mut Gen) -> Self {
        let keys = [
            FixedKey::from_bytes(hex!(
                "0000000000000000000000000000000000000000000000000000000000000000"
            )),
            FixedKey::from_bytes(hex!(
                "0000000000000000000000000000000000000000000000000000000000000001"
            )),
            FixedKey::from_bytes(hex!(
                "0000000000000000000000000000001000000000000000000000000000000001"
            )),
            FixedKey::from_string("0"),
            FixedKey::from_string("1"),
            FixedKey::from_string("2"),
            FixedKey::from_string("3"),
            FixedKey::from_string("4"),
            FixedKey::from_string("5"),
            FixedKey::from_string("6"),
            FixedKey::from_string("7"),
        ];
        *g.one_of(&keys)
    }
}

impl Arbitrary for Op {
    fn arbitrary(g: &mut Gen) -> Self {
        // pick a random key to perform an operation on
        let key = FixedKey::arbitrary(g);

        if *g.one_of(&[true, false]) {
            Op::Insert(key, "value".into())
        } else {
            Op::Delete(key)
        }
    }
}

/// This test fails, since the Trie is designed for fixed key sizes
#[ignore]
#[test]
fn crash_example_v2() {
    let mut trie = Trie::new_empty();
    trie.insert(b"00aeee", b"ok").unwrap();
    trie.insert(b"00ae", b"ok").unwrap();
}

quickcheck! {
    fn model_test_v1_map(ops: Vec<Op>) -> bool {
        let mut model = HashMap::new();
        let mut implementation = DiffTrie::new_empty();

        for op in ops {
            match op {
                Op::Insert(key, value) => {
                    implementation.insert(key.into_bytes(), Bytes::from(value.clone())).unwrap();
                    model.insert(key, value);
                }
                Op::Delete(key) => {
                    match (implementation.delete(key.into_bytes()), model.remove(&key)) {
                        (Err(DeletionErrorV1::KeyNotFound), None) => (),
                        (Err(err), _) => panic!("Implementation error {err:?}"),
                        (Ok(_), Some(_)) => (),
                        (Ok(returned), None) => panic!("Implementation returned {returned:?} on delete"),
                    }
                }
            }
        }
        true
    }
}

quickcheck! {
    fn model_test_v2_map(ops: Vec<Op>) -> bool {
        let mut model = HashMap::new();
        let mut implementation = Trie::new_empty();

        for op in ops {
            match op {
                Op::Insert(k, v) => {
                    implementation.insert(k.as_slice(), v.as_slice()).unwrap();
                    model.insert(k, v);
                }
                Op::Delete(k) => {
                    match (implementation.delete(k.as_slice()), model.remove(&k)) {
                        (Err(DeletionErrorV2::KeyNotFound), None) => (),
                        (Err(e), _) => panic!("Implementation error {e:?}"),
                        (Ok(_), Some(_)) => (),
                        (Ok(a), None) => panic!("Implementation returned {a:?} on delete"),
                    }
                }
            }
        }
        true
    }
}
