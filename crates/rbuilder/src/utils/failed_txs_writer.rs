use serde::{Deserialize, Serialize};
use serde_json::{self};
use sqlx::types::chrono::Local;
use std::fs::{self};
use std::io::{self};
use std::path::Path;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FailedTx {
    pub uuid: String,
    pub tx_hash: String,
    pub failed_reason: String,
}

pub fn append_json(data: &FailedTx) -> io::Result<()> {
    // Check if the file exists, create it if it doesn't
    // Get the filename based on the current date
    let filename = get_filename();
    let path = Path::new(&filename);
    if !path.exists() {
        // Create a new file with empty array
        fs::write(path, "[]")?;
    }

    // Read the current contents of the file
    let file_contents = fs::read_to_string(path)?;
    let mut json_array: Vec<FailedTx> = serde_json::from_str(&file_contents)?;

    // Append the new data
    json_array.push(data.clone());

    // Write the updated array back to the file
    let updated_contents = serde_json::to_string_pretty(&json_array)?;
    fs::write(path, updated_contents)?;

    Ok(())
}

fn get_filename() -> String {
    // Get the current date
    let now = Local::now().to_utc();
    let date = now.format("%Y-%m-%d").to_string();
    format!("./failedTxs/rbuilder_failed_txs_{}.json", date)
}