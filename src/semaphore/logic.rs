extern crate redis;

use std::num::NonZeroUsize;
use std::sync::mpsc::channel;

use log::debug;
use pyo3::PyErr;
use redis::{AsyncCommands, Client, LposOptions};
use tokio::task::JoinHandle;

use crate::semaphore::errors::SemaphoreError;
use crate::semaphore::utils::{estimate_appropriate_sleep_duration, SemResult};
use crate::semaphore::ThreadState;
use crate::utils::open_client_connection;

/// Enter queue and return when the Semaphore has capacity.
pub(crate) async fn wait_for_slot(ts: ThreadState) -> Result<(), PyErr> {
    // Connect to redis
    let mut connection = open_client_connection::<Client, SemaphoreError>(&ts.client).await?;

    // Enter queue and get the current position
    let mut position = connection
        .rpush(&ts.queue_key, &ts.id)
        .await
        .map_err(|e| PyErr::from(SemaphoreError::from(e)))?;
    debug!("Entered queue in position {}", position);

    loop {
        // If our position is within the Semaphore's capacity, return
        if position < ts.capacity {
            debug!("Position is less than capacity. Returning.");
            break;
        }

        // If the position exceeds the maximum tolerated position, throw an error
        if ts.max_position > 0 && position > ts.max_position {
            debug!("Position is greater than max position. Returning.");
            return Err(PyErr::from(SemaphoreError::MaxPositionExceeded(format!(
                "Position {} exceeds the max position ({}).",
                position, ts.max_position
            ))));
        }

        // Otherwise, sleep for a bit and check again
        let sleep_duration =
            estimate_appropriate_sleep_duration(&position, &ts.capacity, &ts.sleep_duration);
        debug!(
            "Position {} is greater than capacity ({}). Sleeping",
            position, ts.capacity
        );
        tokio::time::sleep(sleep_duration).await;

        // Retrieve position again
        position = connection
            .lpos::<&String, &String, Option<u32>>(&ts.queue_key, &ts.id, LposOptions::default())
            .await
            .map_err(|e| PyErr::from(SemaphoreError::from(e)))?
            .unwrap_or(1);
        debug!("Position is now {}", position);
    }
    Ok(())
}

/// Pop from the queue, to add capacity back to the
/// semaphore, and refresh expiry for the queue.
pub(crate) async fn clean_up(ts: ThreadState) -> SemResult<()> {
    struct S {
        client: Client,
        queue_key: String,
    }
    let (s1, r1) = channel();
    let (s2, r2) = channel();

    s1.send(S {
        client: ts.client.to_owned(),
        queue_key: ts.queue_key.to_owned(),
    })?;
    s2.send(S {
        client: ts.client,
        queue_key: ts.queue_key,
    })?;

    let task1: JoinHandle<SemResult<()>> = tokio::task::spawn(async move {
        let data = r1.recv()?;
        let mut con = open_client_connection::<Client, SemaphoreError>(&data.client).await?;
        con.expire(&data.queue_key, 30_usize).await?;
        Ok(())
    });
    let task2: JoinHandle<SemResult<()>> = tokio::task::spawn(async move {
        let data = r2.recv()?;
        let mut con = open_client_connection::<Client, SemaphoreError>(&data.client).await?;
        con.lpop(&data.queue_key, NonZeroUsize::new(1_usize))
            .await?;
        Ok(())
    });

    task1.await??;
    task2.await??;
    Ok(())
}
