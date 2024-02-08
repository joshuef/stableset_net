// Copyright 2024 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use super::files::{download_file, upload_files, ChunkManager, UploadedFile, UPLOADED_FILES};

use sn_client::{
    Client, FilesApi, FolderEntry, FoldersApi, WalletClient, BATCH_SIZE, MAX_UPLOAD_RETRIES,
};

use sn_protocol::storage::{Chunk, ChunkAddress, RegisterAddress};
use sn_transfers::HotWallet;

use clap::Parser;
use color_eyre::{
    eyre::{bail, eyre},
    Result,
};
use std::{
    collections::BTreeMap,
    fs::create_dir_all,
    path::{Path, PathBuf},
};
use tokio::task::JoinSet;
use walkdir::WalkDir;

#[derive(Parser, Debug)]
pub enum FoldersCmds {
    Upload {
        /// The location of the file(s) to upload for creating the folder on the network.
        ///
        /// Can be a file or a directory.
        #[clap(name = "path", value_name = "PATH")]
        path: PathBuf,
        /// The batch_size to split chunks into parallel handling batches
        /// during payment and upload processing.
        #[clap(long, default_value_t = BATCH_SIZE, short='b')]
        batch_size: usize,
        /// Should the file be made accessible to all. (This is irreversible)
        #[clap(long, name = "make_public", default_value = "false", short = 'p')]
        make_public: bool,
        /// The retry_count for retrying failed chunks
        /// during payment and upload processing.
        #[clap(long, default_value_t = MAX_UPLOAD_RETRIES, short = 'r')]
        max_retries: usize,
    },
    Download {
        /// The hex address of a folder.
        #[clap(name = "address")]
        folder_addr: String,
        /// The name to apply to the downloaded folder.
        #[clap(name = "target name")]
        folder_name: Option<String>,
        /// The batch_size for parallel downloading
        #[clap(long, default_value_t = BATCH_SIZE , short='b')]
        batch_size: usize,
    },
}

pub(crate) async fn folders_cmds(
    cmds: FoldersCmds,
    client: &Client,
    root_dir: &Path,
    verify_store: bool,
) -> Result<()> {
    match cmds {
        FoldersCmds::Upload {
            path,
            batch_size,
            max_retries,
            make_public,
        } => {
            upload_files(
                path.clone(),
                make_public,
                client,
                root_dir.to_path_buf(),
                verify_store,
                batch_size,
                max_retries,
            )
            .await?;

            let mut chunk_manager = ChunkManager::new(root_dir);
            chunk_manager.chunk_path(&path, true, make_public)?;

            let mut dirs_paths = BTreeMap::<PathBuf, FoldersApi>::new();
            for (dir_path, parent, dir_name) in WalkDir::new(&path)
                .into_iter()
                .filter_entry(|e| e.file_type().is_dir())
                .flatten()
                .filter(|e| e.depth() > 0)
                .filter_map(|e| {
                    e.path()
                        .parent()
                        .zip(e.file_name().to_str())
                        .map(|(p, n)| (e.path().to_path_buf(), p.to_owned(), n.to_owned()))
                })
            {
                let curr_folder_addr = *dirs_paths
                    .entry(dir_path)
                    .or_insert(FoldersApi::new(client.clone(), root_dir))
                    .address();

                let parent_folder = dirs_paths
                    .entry(parent)
                    .or_insert(FoldersApi::new(client.clone(), root_dir));
                parent_folder.add_folder(dir_name, curr_folder_addr)?;
            }

            // add chunked files to the corresponding Folders
            for chunked_file in chunk_manager.iter_chunked_files() {
                if let (Some(file_name), Some(parent)) = (
                    chunked_file.file_name.to_str(),
                    chunked_file.file_path.parent(),
                ) {
                    if let Some(folder) = dirs_paths.get_mut(parent) {
                        folder.add_file(file_name.to_string(), chunked_file.head_chunk_address)?;
                    }
                }
            }

            println!("Paying for folders hierarchy and uploading...");
            let root_dir_address = dirs_paths
                .get(&path)
                .map(|folder| *folder.address())
                .ok_or(eyre!("Failed to obtain main Folder network address"))?;
            pay_and_upload_folders(dirs_paths, verify_store, client, root_dir).await?;

            println!(
                "\nFolder hierarchy from {path:?} uploaded susccessfully at {}",
                root_dir_address.to_hex()
            );
        }
        FoldersCmds::Download {
            folder_addr,
            folder_name,
            batch_size,
        } => {
            let address =
                RegisterAddress::from_hex(&folder_addr).expect("Failed to parse Folder address");

            let download_dir = dirs_next::download_dir().unwrap_or(root_dir.to_path_buf());
            let download_folder_path = if let Some(name) = folder_name {
                download_dir.join(name)
            } else {
                download_dir
            };
            println!(
                "Downloading onto {download_folder_path:?} from {} with batch-size {batch_size}",
                address.to_hex()
            );
            debug!(
                "Downloading onto {download_folder_path:?} from {}",
                address.to_hex()
            );

            let mut files_to_download = vec![];
            let mut folders_to_download =
                vec![("".to_string(), address, download_folder_path.clone())];

            while let Some((name, folder_addr, target_path)) = folders_to_download.pop() {
                if !name.is_empty() {
                    println!(
                        "Downloading Folder '{name}' from {}",
                        hex::encode(folder_addr.xorname())
                    );
                }
                download_folder(
                    root_dir,
                    client,
                    &target_path,
                    folder_addr,
                    &mut files_to_download,
                    &mut folders_to_download,
                )
                .await?;
            }

            let files_api: FilesApi = FilesApi::new(client.clone(), download_folder_path);
            let uploaded_files_path = root_dir.join(UPLOADED_FILES);
            for (file_name, addr, path) in files_to_download {
                // try to read the data_map if it exists locally.
                let expected_data_map_location = uploaded_files_path.join(addr.to_hex());
                let local_data_map = UploadedFile::read(&expected_data_map_location)
                    .map(|uploaded_file_metadata| {
                        uploaded_file_metadata.data_map.map(|bytes| Chunk {
                            address: ChunkAddress::new(*addr.xorname()),
                            value: bytes,
                        })
                    })
                    .unwrap_or(None);

                download_file(
                    files_api.clone(),
                    *addr.xorname(),
                    (file_name.into(), local_data_map),
                    &path,
                    false,
                    batch_size,
                )
                .await;
            }
        }
    };
    Ok(())
}

// Make a single payment for all Folders (Registers) and upload them to the network
async fn pay_and_upload_folders(
    dirs_paths: BTreeMap<PathBuf, FoldersApi>,
    verify_store: bool,
    client: &Client,
    root_dir: &Path,
) -> Result<()> {
    // Let's make the storage payment
    let mut wallet_client = WalletClient::new(client.clone(), HotWallet::load_from(root_dir)?);
    let net_addresses = dirs_paths.values().map(|folder| folder.as_net_addr());
    let payment_result = wallet_client.pay_for_storage(net_addresses).await?;
    let balance = wallet_client.balance();
    match payment_result
        .storage_cost
        .checked_add(payment_result.royalty_fees)
    {
        Some(cost) => println!(
            "Made payment of {cost} for {} Folders. New balance: {balance}",
            dirs_paths.len()
        ),
        None => bail!("Failed to calculate total payment cost"),
    }

    // sync Folders concurrently
    let mut tasks = JoinSet::new();
    for (path, mut folder) in dirs_paths {
        let net_addr = folder.as_net_addr();
        let payment = wallet_client.get_payment_for_addr(&net_addr)?;
        let payment_info = payment_result
            .payee_map
            .get(&net_addr)
            .map(|payee| (payment, *payee));

        tasks.spawn(async move {
            match folder.sync(verify_store, payment_info).await {
                Ok(addr) => println!(
                    "Folder (for {}) synced with the network at: {}",
                    path.display(),
                    addr.to_hex()
                ),
                Err(err) => println!(
                    "Failed to sync Folder (for {}) with the network: {err}",
                    path.display(),
                ),
            }
        });
    }

    while let Some(res) = tasks.join_next().await {
        if let Err(err) = res {
            println!("Failed to sync a Folder with the network: {err:?}");
        }
    }

    Ok(())
}

// Download a Folder from the network and keep track of its subfolders and files
async fn download_folder(
    root_dir: &Path,
    client: &Client,
    target_path: &Path,
    folder_addr: RegisterAddress,
    files_to_download: &mut Vec<(String, ChunkAddress, PathBuf)>,
    folders_to_download: &mut Vec<(String, RegisterAddress, PathBuf)>,
) -> Result<()> {
    create_dir_all(target_path)?;
    let folders_api = FoldersApi::retrieve(client.clone(), root_dir, folder_addr).await?;

    for (file_name, folder_entry) in folders_api.entries()?.into_iter() {
        match folder_entry {
            FolderEntry::File(file_addr) => {
                files_to_download.push((file_name, file_addr, target_path.to_path_buf()))
            }
            FolderEntry::Folder(subfolder_addr) => {
                folders_to_download.push((
                    file_name.clone(),
                    subfolder_addr,
                    target_path.join(file_name),
                ));
            }
        }
    }

    Ok(())
}