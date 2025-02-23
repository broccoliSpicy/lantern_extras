use crate::logger::{LogLevel, Logger};
use crate::types::*;
use crate::utils::{append_params_to_uri, get_full_table_name, quote_ident};
use core::{get_available_runtimes, get_runtime};
use csv::Writer;
use rand::Rng;
use std::io::Write;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{mpsc, Arc, RwLock};
use std::thread::JoinHandle;
use std::time::Instant;

use postgres::{Client, NoTls, Row};

pub mod cli;
pub mod core;
pub mod measure_speed;

type EmbeddingRecord = (String, Vec<f32>);

static CONNECTION_PARAMS: &'static str = "connect_timeout=10";

// Helper function to calculate progress using total and processed row count
fn calculate_progress(total: i64, processed: usize) -> u8 {
    if total <= 0 {
        return 0;
    }

    return ((processed as f64 / total as f64) * 100.0) as u8;
}
// This function will do the following
// 1. Get approximate number of rows from pg_class (this is just for info logging)
// 2. Create transaction portal which will poll data from database of batch size provided via args
// 3. Send the rows over the channel
fn producer_worker(
    args: Arc<cli::EmbeddingArgs>,
    batch_size: usize,
    tx: Sender<Vec<Row>>,
    estimate_count: bool,
    logger: Arc<Logger>,
) -> Result<(JoinHandle<AnyhowVoidResult>, i64), anyhow::Error> {
    let mut item_count = 0;
    let (count_tx, count_rx): (Sender<i64>, Receiver<i64>) = mpsc::channel();

    let handle = std::thread::spawn(move || {
        let column = &args.column;
        let schema = &args.schema;
        let table = &args.table;
        let full_table_name = get_full_table_name(schema, table);

        let filter_sql = if args.filter.is_some() {
            format!("WHERE {}", args.filter.as_ref().unwrap())
        } else {
            format!("WHERE {column} IS NOT NULL", column = quote_ident(column))
        };

        let limit_sql = if args.limit.is_some() {
            format!("LIMIT {}", args.limit.as_ref().unwrap())
        } else {
            "".to_owned()
        };

        let uri = append_params_to_uri(&args.uri, CONNECTION_PARAMS);
        let client = Client::connect(&uri, NoTls);

        // we are excplicity checking for error here
        // because the item_count atomic should be update
        // as we have a while loop waiting for it
        // if before updating the variable there will be error the while
        // loop will never exit

        if let Err(e) = client {
            count_tx.send(0)?;
            anyhow::bail!("{e}");
        }
        let mut client = client.unwrap();

        let mut transaction = client.transaction()?;

        if estimate_count {
            let rows = transaction.query(
                &format!("SELECT COUNT(*) FROM {full_table_name} {filter_sql} {limit_sql};"),
                &[],
            );

            if let Err(e) = rows {
                count_tx.send(0)?;
                anyhow::bail!("{e}");
            }

            let rows = rows.unwrap();

            let count: i64 = rows[0].get(0);
            count_tx.send(count)?;
            if count > 0 {
                logger.info(&format!(
                    "Found approximately {} items in table \"{}\"",
                    count, table,
                ));
            }
        } else {
            count_tx.send(0)?;
        }

        // With portal we can execute a query and poll values from it in chunks
        let portal = transaction.bind(
            &format!(
                "SELECT ctid::text, {column}::text FROM {full_table_name} {filter_sql} {limit_sql};",
                column = quote_ident(column),
            ),
            &[],
        )?;

        loop {
            // poll batch_size rows from portal and send it to embedding thread via channel
            let rows = transaction.query_portal(&portal, batch_size as i32)?;

            if rows.len() == 0 {
                break;
            }

            if tx.send(rows).is_err() {
                break;
            }
        }
        drop(tx);
        Ok(())
    });

    // If we have filter or limit we are not going to fetch
    // the item count and progress anyway, so we won't lock the process waiting
    // for thread
    // Wait for the other thread to release the lock
    while let Ok(count) = count_rx.recv() {
        item_count = count;
        break;
    }

    return Ok((handle, item_count));
}

// Embedding worker will listen to the producer channel
// and execute embeddings_core's corresponding function to generate embeddings
// we will here map each vector to it's row ctid before sending the results over channel
// So we will get Vec<Row<String, String> and output Vec<(String, Vec<f32>)> the output will
// contain generated embeddings for the text. If text will be null we will skip that row
fn embedding_worker(
    args: Arc<cli::EmbeddingArgs>,
    rx: Receiver<Vec<Row>>,
    tx: Sender<Vec<EmbeddingRecord>>,
    is_canceled: Option<Arc<RwLock<bool>>>,
    logger: Arc<Logger>,
) -> Result<JoinHandle<AnyhowUsizeResult>, anyhow::Error> {
    let handle = std::thread::spawn(move || {
        let mut count: usize = 0;
        let mut processed_tokens: usize = 0;
        let model = &args.model;
        let mut start = Instant::now();
        let runtime = get_runtime(&args.runtime, None, &args.runtime_params)?;

        while let Ok(rows) = rx.recv() {
            if is_canceled.is_some() && *is_canceled.as_ref().unwrap().read().unwrap() {
                // This variable will be changed from outside to gracefully
                // exit job on next chunk
                anyhow::bail!(JOB_CANCELLED_MESSAGE);
            }

            if count == 0 {
                // mark exact start time
                start = Instant::now();
            }

            let mut input_vectors: Vec<&str> = Vec::with_capacity(rows.len());
            let mut input_ids: Vec<String> = Vec::with_capacity(rows.len());

            for row in &rows {
                if let Ok(Some(src_data)) = row.try_get::<usize, Option<&str>>(1) {
                    if src_data.trim() != "" {
                        input_vectors.push(src_data);
                        input_ids.push(row.get::<usize, String>(0));
                    }
                }
            }

            if input_vectors.len() == 0 {
                continue;
            }

            let embedding_response = runtime.process(&model, &input_vectors);

            if let Err(e) = embedding_response {
                anyhow::bail!("{}", e);
            }

            let embedding_response = embedding_response.unwrap();

            processed_tokens += embedding_response.processed_tokens;
            let mut embeddings = embedding_response.embeddings;

            count += embeddings.len();

            let duration = start.elapsed().as_secs();
            // avoid division by zero error
            let duration = if duration > 0 { duration } else { 1 };
            logger.debug(&format!(
                "Generated {} embeddings - speed {} emb/s",
                count,
                count / duration as usize
            ));

            let mut response_data = Vec::with_capacity(rows.len());

            for _ in 0..embeddings.len() {
                response_data.push((input_ids.pop().unwrap(), embeddings.pop().unwrap()));
            }

            if tx.send(response_data).is_err() {
                // Error occured in exporter worker and channel has been closed
                break;
            }
        }

        if count > 0 {
            logger.info("Embedding generation finished, waiting to export results...");
        } else {
            logger.warn("No data to generate embeddings");
        }
        drop(tx);
        Ok(processed_tokens)
    });

    return Ok(handle);
}

// DB exporter worker will create temp table with name _lantern_tmp_${rand(0,1000)}
// Then it will create writer stream which will COPY bytes from stdin to that table
// After that it will receiver the output embeddings mapped with row ids over the channel
// And write them using writer instance
// At the end we will flush the writer commit the transaction and UPDATE destination table
// Using our TEMP table data
fn db_exporter_worker(
    args: Arc<cli::EmbeddingArgs>,
    rx: Receiver<Vec<EmbeddingRecord>>,
    item_count: i64,
    progress_cb: Option<ProgressCbFn>,
    logger: Arc<Logger>,
) -> Result<JoinHandle<AnyhowUsizeResult>, anyhow::Error> {
    let handle = std::thread::spawn(move || {
        let uri = args.out_uri.as_ref().unwrap_or(&args.uri);
        let column = &args.out_column;
        let table = args.out_table.as_ref().unwrap_or(&args.table);
        let schema = &args.schema;
        let full_table_name = get_full_table_name(schema, table);

        let uri = append_params_to_uri(uri, CONNECTION_PARAMS);

        let mut client = Client::connect(&uri, NoTls)?;
        let mut transaction = client.transaction()?;
        let mut rng = rand::thread_rng();
        let temp_table_name = format!("_lantern_tmp_{}", rng.gen_range(0..1000));

        if args.create_column {
            transaction.execute(
                &format!(
                    "ALTER TABLE {full_table_name} ADD COLUMN IF NOT EXISTS {column} REAL[]",
                    column = quote_ident(column)
                ),
                &[],
            )?;
        }

        // Try to check if user has write permissions to table
        let res = transaction.query("SELECT 1 FROM information_schema.column_privileges WHERE table_schema=$1 AND table_name=$2 AND column_name=$3 AND privilege_type='UPDATE' AND grantee=current_user", &[schema, table, column])?;

        if res.get(0).is_none() {
            anyhow::bail!("User does not have write permissions to target table");
        }

        transaction
            .execute(
                &format!(
                    "CREATE TEMPORARY TABLE {temp_table_name} AS SELECT ctid::TEXT as id, '{{}}'::REAL[] AS {column} FROM {full_table_name} LIMIT 0",
                    column=quote_ident(column)
                ),
                &[],
            )?;
        transaction.commit()?;

        let mut transaction = client.transaction()?;
        let mut writer = transaction.copy_in(&format!(
            "COPY {temp_table_name} FROM stdin WITH NULL AS 'NULL'"
        ))?;
        let update_sql = &format!("UPDATE {full_table_name} dest SET {column} = src.{column} FROM {temp_table_name} src WHERE src.id::tid = dest.ctid", column=quote_ident(column), temp_table_name=quote_ident(&temp_table_name));

        let flush_interval = 10;
        let min_flush_rows = 50;
        let max_flush_rows = 1000;
        let mut start = Instant::now();
        let mut collected_row_cnt = 0;
        let mut processed_row_cnt = 0;
        let mut old_progress = 0;

        while let Ok(rows) = rx.recv() {
            for row in &rows {
                writer.write(row.0.as_bytes())?;
                writer.write("\t".as_bytes())?;
                if row.1.len() > 0 {
                    writer.write("{".as_bytes())?;
                    let row_str: String = row.1.iter().map(|&x| x.to_string() + ",").collect();
                    writer.write(row_str[0..row_str.len() - 1].as_bytes())?;
                    drop(row_str);
                    writer.write("}".as_bytes())?;
                } else {
                    writer.write("NULL".as_bytes())?;
                }
                writer.write("\n".as_bytes())?;
                collected_row_cnt += 1;
            }

            processed_row_cnt += rows.len();
            let progress = calculate_progress(item_count, processed_row_cnt);

            if progress > old_progress {
                old_progress = progress;
                logger.debug(&format!("Progress {progress}%",));
                if progress_cb.is_some() {
                    let cb = progress_cb.as_ref().unwrap();
                    cb(progress);
                }
            }

            drop(rows);

            if !args.stream {
                continue;
            }

            if collected_row_cnt >= max_flush_rows
                || (flush_interval <= start.elapsed().as_secs()
                    && collected_row_cnt >= min_flush_rows)
            {
                // if job is run in streaming mode
                // it will write results to target table each 10 seconds (if collected rows are
                // more than 50) or if collected row count is more than 1000 rows
                writer.flush()?;
                writer.finish()?;
                transaction.batch_execute(&format!(
                    "
                    {update_sql};
                    TRUNCATE TABLE {temp_table_name};
                "
                ))?;
                transaction.commit()?;
                transaction = client.transaction()?;
                writer = transaction.copy_in(&format!("COPY {temp_table_name} FROM stdin"))?;
                collected_row_cnt = 0;
                start = Instant::now();
            }
        }

        // There might be a case when filter is provided manually
        // And `{column} IS NOT NULL` will be missing from the table
        // So we will check if the column is null in rust code before generating embedding
        // Thus the processed rows may be less than the actual estimated row count
        // And progress will not be 100
        if old_progress != 100 {
            logger.debug("Progress 100%");
            if progress_cb.is_some() {
                let cb = progress_cb.as_ref().unwrap();
                cb(100);
            }
        }

        if processed_row_cnt == 0 {
            return Ok(processed_row_cnt);
        }

        writer.flush()?;
        writer.finish()?;
        transaction.execute(update_sql, &[])?;
        transaction.commit()?;
        logger.info(&format!(
            "Embeddings exported to table {} under column {}",
            &table, &column
        ));
        Ok(processed_row_cnt)
    });

    return Ok(handle);
}

fn csv_exporter_worker(
    args: Arc<cli::EmbeddingArgs>,
    rx: Receiver<Vec<EmbeddingRecord>>,
    logger: Arc<Logger>,
) -> Result<JoinHandle<AnyhowUsizeResult>, anyhow::Error> {
    let handle = std::thread::spawn(move || {
        let csv_path = args.out_csv.as_ref().unwrap();
        let mut wtr = Writer::from_path(&csv_path).unwrap();
        let mut processed_row_cnt = 0;
        while let Ok(rows) = rx.recv() {
            for row in &rows {
                let vector_string = &format!(
                    "{{{}}}",
                    row.1
                        .iter()
                        .map(|f| f.to_string())
                        .collect::<Vec<String>>()
                        .join(",")
                );
                wtr.write_record(&[&row.0.to_string(), vector_string])
                    .unwrap();
                processed_row_cnt += rows.len();
            }
        }
        wtr.flush().unwrap();
        logger.info(&format!("Embeddings exported to {}", &csv_path));
        Ok(processed_row_cnt)
    });

    return Ok(handle);
}

pub fn get_default_batch_size(model: &str) -> usize {
    match model {
        "clip/ViT-B-32-textual" => 2000,
        "clip/ViT-B-32-visual" => 50,
        "BAAI/bge-small-en" => 300,
        "BAAI/bge-base-en" => 100,
        "BAAI/bge-large-en" => 60,
        "jinaai/jina-embeddings-v2-small-en" => 500,
        "jinaai/jina-embeddings-v2-base-en" => 80,
        "intfloat/e5-base-v2" => 300,
        "intfloat/e5-large-v2" => 100,
        "llmrails/ember-v1" => 100,
        "thenlper/gte-base" => 1000,
        "thenlper/gte-large" => 800,
        "microsoft/all-MiniLM-L12-v2" => 1000,
        "microsoft/all-mpnet-base-v2" => 400,
        "transformers/multi-qa-mpnet-base-dot-v1" => 300,
        "openai/text-embedding-ada-002" => 500,
        "openai/text-embedding-3-small" => 500,
        "openai/text-embedding-3-large" => 500,
        "cohere/embed-english-v3.0"
        | "cohere/embed-multilingual-v3.0"
        | "cohere/embed-english-light-v3.0"
        | "cohere/embed-multilingual-light-v3.0"
        | "cohere/embed-english-v2.0"
        | "cohere/embed-english-light-v2.0"
        | "cohere/embed-multilingual-v2.0" => 5000,
        _ => 100,
    }
}

pub fn create_embeddings_from_db(
    args: cli::EmbeddingArgs,
    track_progress: bool,
    progress_cb: Option<ProgressCbFn>,
    is_canceled: Option<Arc<RwLock<bool>>>,
    logger: Option<Logger>,
) -> Result<(usize, usize), anyhow::Error> {
    let logger = Arc::new(logger.unwrap_or(Logger::new("Lantern Embeddings", LogLevel::Debug)));
    logger.info("Lantern CLI - Create Embeddings");
    let args = Arc::new(args);
    let batch_size = args
        .batch_size
        .unwrap_or(get_default_batch_size(&args.model));

    logger.debug(&format!(
        "Model - {}, Visual - {}, Batch Size - {}",
        args.model, args.visual, batch_size
    ));

    // Create channel that will send the database rows to embedding worker
    let (producer_tx, producer_rx): (Sender<Vec<Row>>, Receiver<Vec<Row>>) = mpsc::channel();
    let (embedding_tx, embedding_rx): (
        Sender<Vec<EmbeddingRecord>>,
        Receiver<Vec<EmbeddingRecord>>,
    ) = mpsc::channel();

    let (producer_handle, item_cnt) = producer_worker(
        args.clone(),
        batch_size,
        producer_tx,
        track_progress,
        logger.clone(),
    )?;

    // Create exporter based on provided args
    // For now we only have csv and db exporters
    let exporter_handle = if args.out_csv.is_some() {
        csv_exporter_worker(args.clone(), embedding_rx, logger.clone())?
    } else {
        db_exporter_worker(
            args.clone(),
            embedding_rx,
            item_cnt,
            progress_cb,
            logger.clone(),
        )?
    };

    let embedding_handle = embedding_worker(
        args.clone(),
        producer_rx,
        embedding_tx,
        is_canceled,
        logger.clone(),
    )?;
    // Collect the thread handles in a vector to wait them
    let handles = vec![producer_handle];

    for handle in handles {
        match handle.join() {
            Err(e) => {
                logger.error(&format!("{:?}", e));
                anyhow::bail!("{:?}", e);
            }
            Ok(res) => {
                if let Err(e) = res {
                    logger.error(&format!("{:?}", e));
                    anyhow::bail!("{:?}", e);
                }
            }
        }
    }

    let processed_tokens = match embedding_handle.join() {
        Err(e) => {
            logger.error(&format!("{:?}", e));
            anyhow::bail!("{:?}", e);
        }
        Ok(res) => res?,
    };

    let processed_rows = match exporter_handle.join() {
        Err(e) => {
            logger.error(&format!("{:?}", e));
            anyhow::bail!("{:?}", e);
        }
        Ok(res) => res?,
    };

    Ok((processed_rows, processed_tokens))
}

pub fn show_available_models(
    args: &cli::ShowModelsArgs,
    logger: Option<Logger>,
) -> AnyhowVoidResult {
    let logger = logger.unwrap_or(Logger::new("Lantern Embeddings", LogLevel::Info));
    logger.info("Available Models\n");
    let runtime = get_runtime(&args.runtime, None, &args.runtime_params)?;
    logger.print_raw(&runtime.get_available_models().0);
    Ok(())
}

pub fn show_available_runtimes(logger: Option<Logger>) -> AnyhowVoidResult {
    let logger = logger.unwrap_or(Logger::new("Lantern Embeddings", LogLevel::Info));
    let mut runtimes_str = get_available_runtimes().join("\n");
    runtimes_str.push_str("\n");
    logger.info("Available Runtimes\n");
    logger.print_raw(&runtimes_str);
    Ok(())
}
