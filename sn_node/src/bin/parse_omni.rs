// Copyright 2023 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::str::FromStr;
use zip::read::ZipArchive;

#[derive(Deserialize, Debug)]
struct Transaction {
    sendingaddress: Option<String>,
    referenceaddress: Option<String>,
    amount: Option<String>,
    purchasedtokens: Option<String>,
    #[serde(rename = "type")]
    tx_type: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Open the ZIP file
    let zip_file = File::open("./resources/all_maidsafecoin_txs_omnicore_2022-09-06.json.zip")?;
    let mut archive = ZipArchive::new(zip_file)?;

    // Assume that the ZIP file contains exactly one file and open it
    let mut file = archive.by_index(0)?;

    // Read the JSON data from the file
    let mut json_data = String::new();
    file.read_to_string(&mut json_data)?;

    // Parse the JSON data into a Vector of Transactions
    let transactions: Vec<Transaction> = serde_json::from_str(&json_data)?;

    // Initialize a HashMap to store the balances
    let mut balances: HashMap<String, f64> = HashMap::new();

    // First pass: handle "Crowdsale Purchase" transactions
    for tx in &transactions {
        if tx.tx_type == "Crowdsale Purchase" {
            if let Some(recipient) = &tx.referenceaddress {
                let purchased_tokens = f64::from_str(&tx.purchasedtokens.as_ref().unwrap_or(&"0".to_string()))?;
                *balances.entry(recipient.clone()).or_insert(0.0) += purchased_tokens;
            }
        }
    }

    // Second pass: handle "Simple Send" transactions
    for tx in &transactions {
        if tx.tx_type == "Simple Send" {
            if let Some(sender) = &tx.sendingaddress {
                let amount = f64::from_str(&tx.amount.as_ref().unwrap_or(&"0".to_string()))?;
                *balances.entry(sender.clone()).or_insert(0.0) -= amount;
            }
            if let Some(recipient) = &tx.referenceaddress {
                let amount = f64::from_str(&tx.amount.as_ref().unwrap_or(&"0".to_string()))?;
                *balances.entry(recipient.clone()).or_insert(0.0) += amount;
            }
        }
    }

    // Filter out addresses with zero or negative balance and print them
    for (address, balance) in balances {
        if balance > 0.0 {
            println!("Address: {}, Balance: {}", address, balance);
        }
    }

    Ok(())
}
