use eyre::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;
use tokio::fs as tokio_fs;

#[derive(Serialize, Deserialize)]
struct State {
    seen_books: HashSet<u32>,
}

impl State {
    fn new() -> Self {
        State {
            seen_books: HashSet::new(),
        }
    }

    fn load_from_file(path: &str) -> Result<Self> {
        if Path::new(path).exists() {
            let file = File::open(path)?;
            let state: State = serde_json::from_reader(file)?;
            Ok(state)
        } else {
            Ok(Self::new())
        }
    }

    fn save_to_file(&self, path: &str) -> Result<()> {
        let file = File::create(path)?;
        serde_json::to_writer(file, self)?;
        Ok(())
    }

    fn mark_seen(&mut self, book_id: u32) {
        self.seen_books.insert(book_id);
    }

    fn has_seen(&self, book_id: u32) -> bool {
        self.seen_books.contains(&book_id)
    }
}

async fn download_book(client: &Client, book_id: u32) -> Result<Vec<u8>> {
    let url = format!("http://www.gutenberg.org/ebooks/{}.txt.utf-8", book_id);
    let response = client.get(&url).send().await?.bytes().await?;
    Ok(response.to_vec())
}

async fn safe_upload(data: Vec<u8>) {
    // Replace this with your actual safe.upload logic
    println!("Uploading book with size: {}", data.len());
}

#[tokio::main]
async fn main() -> Result<()> {
    let client = Client::new();
    let state_file = "state.json";
    let mut state = State::load_from_file(state_file)?;

    let start_id = 1;
    let end_id = 10;

    for book_id in start_id..=end_id {
        if state.has_seen(book_id) {
            println!("Already seen book ID: {}", book_id);
            continue;
        }

        match download_book(&client, book_id).await {
            Ok(data) => {
                println!("Downloaded book ID: {}", book_id);
                // print the first 100 bytes of the book as text
                // println!("{}", String::from_utf8_lossy(&data[..100]));

                safe_upload(data).await;
                state.mark_seen(book_id);
                state.save_to_file(state_file)?;

                println!("Sleeping for 1 hour...");
                tokio::time::sleep(tokio::time::Duration::from_secs(60 * 60)).await;
            }
            Err(e) => {
                eprintln!("Failed to download book ID {}: {:?}", book_id, e);
            }
        }
    }

    Ok(())
}
