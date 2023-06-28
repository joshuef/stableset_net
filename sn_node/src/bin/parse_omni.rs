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
use std::io::{BufReader, Read};
use std::str::FromStr;
use zip::read::ZipArchive;

#[derive(Deserialize, Debug)]
struct Transaction {
    sendingaddress: Option<String>,
    referenceaddress: Option<String>,
    amount: Option<String>,
    purchasedtokens: Option<String>,
}

fn main() {
    // Open the ZIP file
    let zip_file = File::open("./resources/all_maidsafecoin_txs_omnicore_2022-09-06.json.zip").unwrap();
    let mut archive = ZipArchive::new(zip_file).unwrap();

    // Assume that the ZIP file contains exactly one file and open it
    let mut file = archive.by_index(0).unwrap();

    // Read the JSON data from the file
    let mut json_data = String::new();
    file.read_to_string(&mut json_data).unwrap();

    // Parse the JSON data into a Vector of Transactions
    let transactions: Vec<Transaction> = serde_json::from_str(&json_data).unwrap();

    // Initialize a HashMap to store the balances
    let mut balances: HashMap<String, f64> = HashMap::new();

    // Iterate over each transaction
    for tx in transactions {
        if let Some(sender) = tx.sendingaddress {
            let amount = f64::from_str(&tx.amount.unwrap_or_else(|| "0".to_string())).unwrap();
            *balances.entry(sender).or_insert(0.0) -= amount;
        }
        
        if let Some(recipient) = tx.referenceaddress {
            let purchased_tokens = f64::from_str(&tx.purchasedtokens.unwrap_or_else(|| "0".to_string())).unwrap();
            *balances.entry(recipient).or_insert(0.0) += purchased_tokens;
        }
    }

    // Filter out addresses with zero balance and print them
    for (address, balance) in balances {
        if balance != 0.0 {
            println!("Address: {}, Balance: {}", address, balance);
        }
    }
}
