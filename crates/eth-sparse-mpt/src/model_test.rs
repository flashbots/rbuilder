use crate::v2::trie::Trie;
use quickcheck::{quickcheck, Arbitrary, Gen};
use std::collections::HashMap;

// The maximum key size. keeping it relatively
// small increases the chance of multiple
// operations being executed against the same
// key, which will tease out more bugs.
const KEY_SPACE: u8 = 16;

#[derive(Clone, Debug)]
enum Op {
    Insert(Vec<u8>, Vec<u8>),
    Get(Vec<u8>),
}

trait ChooseNonempty {
    fn one_of<'a, T>(&'a mut self, entries: &'a [T]) -> &'a T;
}

impl ChooseNonempty for Gen {
    fn one_of<'a, T>(&'a mut self, entries: &'a [T]) -> &'a T {
        self.choose(entries).expect("empty list in choose nonempty")
    }
}

// Arbitrary lets you create randomized instances
// of types that you're interested in testing
// properties with. QuickCheck will look for
// this trait for things that are the arguments
// to properties that it is testing.
impl Arbitrary for Op {
    fn arbitrary(g: &mut Gen) -> Self {
        // pick a random key to perform an operation on
        let key = g
            .one_of(&["key00", "key01", "odd", "key010"])
            .as_bytes()
            .to_owned();

        if *g.one_of(&[true, false]) {
            Op::Insert(key, "value".into())
        } else {
            Op::Get(key)
        }
    }
}

quickcheck! {
    fn model_test_v2(ops: Vec<Op>) -> bool {
        let mut model = HashMap::new();
        let mut implementation = Trie::new_empty();

        for op in ops {
            match op {
                Op::Insert(k, v) => {
                    implementation.insert(k.as_slice(), v.as_slice());
                    model.insert(k, v);
                }
                Op::Get(k) => {
                    // if implementation.get(&k) != model.get(&k).map(AsRef::as_ref) {
                    //     return false;
                    // }
                }
            }
        }

        true
    }
}
