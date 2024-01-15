// Copyright 2023 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

#[macro_use]
extern crate tracing;

mod api;
mod audit;
mod chunks;
mod error;
mod event;
mod faucet;
mod files;
mod register;
mod wallet;

pub(crate) use error::Result;

pub use self::{
    error::Error,
    event::{ClientEvent, ClientEventsReceiver},
    faucet::{get_tokens_from_faucet, load_faucet_wallet_from_genesis_wallet},
    files::{
        download::{FilesDownload, FilesDownloadEvent},
        upload::{FileUploadEvent, FilesUpload},
        FilesApi, BATCH_SIZE, MAX_UPLOAD_RETRIES,
    },
    register::ClientRegister,
    wallet::{broadcast_signed_spends, send, WalletClient},
};

use self::event::ClientEventsChannel;
// use indicatif::ProgressBar;
use sn_networking::Network;

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;
#[cfg(target_arch = "wasm32")]
use web_sys::console;




// This is like the `main` function, except for JavaScript.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen(start)]
pub async fn main_js() -> std::result::Result<(), JsValue> {
    // This provides better error messages in debug mode.
    // It's disabled in release mode so it doesn't bloat up the file size.
    #[cfg(debug_assertions)]
    console_error_panic_hook::set_once();


    // Your code goes here!
    console::log_1(&JsValue::from_str("Hello safe world!"));
    
    if let Err(error) = Client::quick_start(None).await {
        console::log_1(&JsValue::from_str("quick_start err!.{error:?}"));

    }

    console::log_1(&JsValue::from_str("Supposedly a client started!"));

    Ok(())
}


/// A quick client that only takes some peers to connect to
#[wasm_bindgen]
#[cfg(target_arch = "wasm32")]
pub async fn greet(s: &str) -> std::result::Result<(), JsValue> {
    console::log_1(&JsValue::from_str(s));
    Ok(())
}

/// Client API implementation to store and get data.
#[derive(Clone)]
pub struct Client {
    network: Network,
    events_channel: ClientEventsChannel,
    signer: bls::SecretKey,
    peers_added: usize,
    // progress: Option<ProgressBar>,
}
