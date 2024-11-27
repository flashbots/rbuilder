use alloy_primitives::{Address, B256, U256};
use std::{fs::File, io, io::Write, path::Path};

#[derive(Debug)]
pub struct CSVOutputRow {
    pub block_number: u64,
    pub block_hash: B256,
    pub address: Address,
    pub amount: U256,
}

#[derive(Debug)]
pub struct CSVResultWriter {
    file: File,
}

impl CSVResultWriter {
    pub fn new(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        let mut result = Self { file };
        result.write_header()?;
        Ok(result)
    }

    fn write_header(&mut self) -> io::Result<()> {
        writeln!(self.file, "block_number,block_hash,address,amount")?;
        self.file.flush()
    }

    pub fn write_data(&mut self, mut values: Vec<CSVOutputRow>) -> io::Result<()> {
        // first sort values by block from low to high and then by address
        values.sort_by(|a, b| {
            a.block_number
                .cmp(&b.block_number)
                .then_with(|| a.address.cmp(&b.address))
        });

        for value in values {
            writeln!(
                self.file,
                "{},{:?},{:?},{}",
                value.block_number, value.block_hash, value.address, value.amount
            )?;
        }
        self.file.flush()
    }
}
