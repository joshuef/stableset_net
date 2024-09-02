use std::time::Duration;

use bytes::Bytes;
use eyre::{bail, Result};
use libautonomi::Client;
use tokio::time::sleep;

mod common;

#[tokio::test]
async fn file() -> Result<(), Box<dyn std::error::Error>> {
    common::enable_logging();

    let mut client = Client::connect(&[]).await?;
    let mut wallet = common::load_hot_wallet_from_faucet();

    // let data = common::gen_random_data(1024 * 1024 * 1000);
    // let user_key = common::gen_random_data(32);

    let (root, addr) = client
        .upload_from_dir("tests/file/test_dir".into(), &mut wallet)
        .await?;
    sleep(Duration::from_secs(2)).await;

    let root_fetched = client.fetch_root(addr).await?;

    assert_eq!(
        root.map, root_fetched.map,
        "root fetched should match root put"
    );

    Ok(())
}

#[tokio::test]
async fn file_into_accnt() -> Result<()> {
    common::enable_logging();

    let mut client = Client::connect(&[])
        .await?
        .with_account_entropy(Bytes::from("at least 32 bytes of entropy here"))?;
    let mut wallet = common::load_hot_wallet_from_faucet();

    let (root, addr) = client
        .upload_from_dir("tests/file/test_dir".into(), &mut wallet)
        .await?;
    sleep(Duration::from_secs(2)).await;

    let root_fetched = client.fetch_root(addr).await?;

    assert_eq!(
        root.map, root_fetched.map,
        "root fetched should match root put"
    );

    // now assert over the stored account packet
    let new_client = Client::connect(&[])
        .await?
        .with_account_entropy(Bytes::from("at least 32 bytes of entropy here"))?;

    if let Some(ap) = new_client.get_decrypt_account().await? {
        let ap_root_fetched = Client::deserialise_root(ap)?;

        assert_eq!(
            root.map, ap_root_fetched.map,
            "root fetched should match root put"
        );
    } else {
        bail!("No account packet found");
    }

    Ok(())
}
