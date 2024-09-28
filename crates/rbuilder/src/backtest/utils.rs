macro_rules! timeit {
    ($block:expr) => {{
        let t0 = std::time::Instant::now();
        let res = $block;
        let elapsed = t0.elapsed();
        (res, elapsed)
    }};
}

pub(crate) use timeit;