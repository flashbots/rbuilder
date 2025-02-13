use std::io;
use rbuilder::utils::failed_txs_writer;


fn main() -> io::Result<()> {
    // Create some sample data
    let new_data = failed_txs_writer::FailedTx {
        uuid: "ssss".to_string(),
        tx_hash: "dddd".to_string(),
        failed_reason: "sss".to_string(),
    };
    // Append the JSON data to the file
    failed_txs_writer::append_json(&new_data)?;
    failed_txs_writer::append_json(&new_data)?;
    println!("failed transaction is appended.");
    Ok(())
}