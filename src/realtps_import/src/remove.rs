use crate::client::Client;
use crate::delay;
use crate::helpers::*;
use anyhow::{anyhow, Result};
use log::{debug, info, warn};
use realtps_common::{
    chain::Chain,
    db::{Block, Db},
};
use std::sync::Arc;

pub async fn remove_old_data_for_chain(chain: Chain, db: &Arc<dyn Db>) -> Result<()> {
    let highest_block_number = load_highest_know_block_number(chain, &db).await?;
    let highest_block_number = highest_block_number.ok_or_else(|| anyhow!("no data for chain {}", chain))?;

    let load_block = |number| load_block(chain, &db, number);

    let latest_timestamp = load_block(highest_block_number)
        .await?
        .expect("firt block")
        .timestamp;

    let seconds_per_week = 60 * 60 * 24 * 7;
    let min_timestamp = latest_timestamp
        .checked_sub(second_per_week)
        .expect("underflow");

    let mut current_block = load_block(highest_block_number)
        .await?
        .expect("firt_block");

    let mut to_remove_blocks = vec![];

    loop {
        let prev_block_number = current_block.prev_block_number;

        if prev_block_number.is_none() {
            break;
        }

        let prev_block_number = prev_block_number.unwrap();

        let prev_block = load_block(prev_block_number).await?;

        if prev_block.is_none() {
            break;
        }

        let prev_block = prev_block.unwrap();

        if prev_block.timestamp == 0 {
            break;
        }
        if prev_block.timestamp < min_timestamp {
            to_remove_blocks.push(prev_block_number);
        }

        current_block = prev_block;
        break;
    }


    // todo
    // remove data according to `to_remove_blocks`
    // from the earlist block
    
    
    Ok(())
}

