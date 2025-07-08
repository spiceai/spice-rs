# RetryableQueryStream

## Overview

`RetryableQueryStream` is a stream type returned by `Client::query()` that provides robust retry capabilities for handling network interruptions during data streaming. This feature ensures that your data streaming operations can recover gracefully from transient connection issues.

## Key Features

- **Automatic Query Reissue**: When a connection reset occurs during streaming, the query can be reissued if stream consumer keeps polling.
- **Resumable Data Streaming**: After a connection reset, the data streaming can restart from the beginning.
- **Error Handling Options**: Provides flexibility to either retry the stream or stop polling and handle the error.

## Usage

The `RetryableQueryStream` is returned from the `Client::query()` method:

```rust
use spiceai::ClientBuilder;

#[tokio::main]
async fn main() {
    let client = ClientBuilder::new().build().await.unwrap();

    // The query method returns a RetryableQueryStream
    let stream = client.query(
        "SELECT trip_distance, total_amount FROM taxi_trips ORDER BY trip_distance DESC LIMIT 10;"
    ).await.unwrap();

    // Now you can consume the stream
    // ...
}
```

## Retry Behavior

When consuming the `RetryableQueryStream`, you have two options for handling connection resets:

### Option 1: Automatic Retry (Continue Polling)

If you continue polling the stream after receiving a `ConnectionReset` error, the query will be automatically reissued and data streaming will restart from the beginning:

```rust
use futures::StreamExt;
use spiceai::error::SpiceClientError;

async fn process_stream(mut stream: RetryableQueryStream) -> Result<(), Box<dyn std::error::Error>> {
    let batches_collected = Vec::new();
    while let Some(batch_result) = stream.next().await {
        match batch_result {
            Ok(batch) => {
                // Process the batch
                println!("Received batch with {} rows", batch.num_rows());
            },
            Err(err) => {
                if matches!(err, SpiceClientError::ConnectionReset(_)) {
                    // The next poll will automatically retry the query
                    println!("Connection reset detected, will retry on next poll");
                    // Clear the collected record batches since the whole stream will retry
                    batches_collected.clear()
                    continue;
                } else {
                    // Handle other errors
                    return Err(err.into());
                }
            }
        }
    }

    Ok(())
}
```

**Example Flow:**

1. Poll 1: ✅ Received batch1
2. Poll 2: ✅ Received batch2
3. Poll 3: ❌ `SpiceClientError::ConnectionReset` error → Continue polling
4. Poll 4: ✅ Received batch1 (query restarted automatically)
5. Poll 5: ✅ Received batch2
6. Poll 6: ✅ Received batch3
7. ...

### Option 2: Stop Polling and Handle Error

Alternatively, you can choose to stop polling when a `ConnectionReset` error occurs and handle the error:

```rust
use futures::StreamExt;
use spiceai::error::SpiceClientError;

async fn process_stream(mut stream: RetryableQueryStream) -> Result<(), Box<dyn std::error::Error>> {
    while let Some(batch_result) = stream.next().await {
        match batch_result {
            Ok(batch) => {
                // Process the batch
                println!("Received batch with {} rows", batch.num_rows());
            },
            Err(err) => {
                if matches!(err, SpiceClientError::ConnectionReset(_)) {
                    // Stop polling and handle the error
                    println!("Connection reset detected, stopping stream processing");
                    return Err(err.into());
                } else {
                    // Handle other errors
                    return Err(err.into());
                }
            }
        }
    }

    Ok(())
}
```

**Example Flow:**

1. Poll 1: ✅ Received batch1
2. Poll 2: ✅ Received batch2
3. Poll 3: ❌ `SpiceClientError::ConnectionReset` error → Stop polling and handle error

## Considerations

- When a stream is retried, all previously received data must be processed again from the beginning.
- Consider implementing idempotent processing of record batches to handle duplicate data when retries occur.
- For long-running queries or large result sets, implement appropriate error handling to decide whether to retry or abort.

## Related APIs

- `Client::query()`: Returns a `RetryableQueryStream` for executing SQL queries.
- `SpiceClientError::ConnectionReset`: The error type that indicates a connection reset has occurred.
