use bincode;
use bitcoin::blockdata::block::{Block, BlockHeader};
use bitcoin::blockdata::transaction::{Transaction, TxIn, TxOut};
use bitcoin::network::serialize::BitcoinHash;
use bitcoin::network::serialize::{deserialize, serialize};
use bitcoin::util::hash::Sha256dHash;
use crypto::digest::Digest;
use crypto::sha2::Sha256;
use std::collections::HashMap;
use std::iter::FromIterator;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use daemon::Daemon;
use signal::Waiter;
use store::{ReadStore, Row, WriteStore};
use util::{self, full_hash, hash_prefix, Bytes, FullHash, HashPrefix, HeaderEntry, HeaderList,
           HeaderMap, Timer, HASH_PREFIX_LEN};

use errors::*;

#[derive(Serialize, Deserialize)]
pub struct TxInKey {
    pub code: u8,
    pub prev_hash_prefix: HashPrefix,
    pub prev_index: u16,
}

#[derive(Serialize, Deserialize)]
pub struct TxInRow {
    key: TxInKey,
    pub txid_prefix: HashPrefix,
}

impl TxInRow {
    pub fn new(txid: &Sha256dHash, input: &TxIn) -> TxInRow {
        TxInRow {
            key: TxInKey {
                code: b'I',
                prev_hash_prefix: hash_prefix(&input.prev_hash[..]),
                prev_index: input.prev_index as u16,
            },
            txid_prefix: hash_prefix(&txid[..]),
        }
    }

    pub fn filter(txid: &Sha256dHash, output_index: usize) -> Bytes {
        bincode::serialize(&TxInKey {
            code: b'I',
            prev_hash_prefix: hash_prefix(&txid[..]),
            prev_index: output_index as u16,
        }).unwrap()
    }

    pub fn to_row(&self) -> Row {
        Row {
            key: bincode::serialize(&self).unwrap(),
            value: vec![],
        }
    }

    pub fn from_row(row: &Row) -> TxInRow {
        bincode::deserialize(&row.key).expect("failed to parse TxInRow")
    }
}

#[derive(Serialize, Deserialize)]
pub struct TxOutKey {
    code: u8,
    script_hash_prefix: HashPrefix,
}

#[derive(Serialize, Deserialize)]
pub struct TxOutRow {
    key: TxOutKey,
    pub txid_prefix: HashPrefix,
}

impl TxOutRow {
    pub fn new(txid: &Sha256dHash, output: &TxOut) -> TxOutRow {
        TxOutRow {
            key: TxOutKey {
                code: b'O',
                script_hash_prefix: hash_prefix(&compute_script_hash(&output.script_pubkey[..])),
            },
            txid_prefix: hash_prefix(&txid[..]),
        }
    }

    pub fn filter(script_hash: &[u8]) -> Bytes {
        bincode::serialize(&TxOutKey {
            code: b'O',
            script_hash_prefix: hash_prefix(&script_hash[..HASH_PREFIX_LEN]),
        }).unwrap()
    }

    pub fn to_row(&self) -> Row {
        Row {
            key: bincode::serialize(&self).unwrap(),
            value: vec![],
        }
    }

    pub fn from_row(row: &Row) -> TxOutRow {
        bincode::deserialize(&row.key).expect("failed to parse TxOutRow")
    }
}

#[derive(Serialize, Deserialize)]
pub struct TxKey {
    code: u8,
    pub txid: FullHash,
}

pub struct TxRow {
    pub key: TxKey,
    pub height: u32, // value
}

impl TxRow {
    pub fn new(txid: &Sha256dHash, height: u32) -> TxRow {
        TxRow {
            key: TxKey {
                code: b'T',
                txid: full_hash(&txid[..]),
            },
            height: height,
        }
    }

    pub fn filter_prefix(txid_prefix: &HashPrefix) -> Bytes {
        [b"T", &txid_prefix[..]].concat()
    }

    pub fn filter_full(txid: &Sha256dHash) -> Bytes {
        [b"T", &txid[..]].concat()
    }

    pub fn to_row(&self) -> Row {
        Row {
            key: bincode::serialize(&self.key).unwrap(),
            value: bincode::serialize(&self.height).unwrap(),
        }
    }

    pub fn from_row(row: &Row) -> TxRow {
        TxRow {
            key: bincode::deserialize(&row.key).expect("failed to parse TxKey"),
            height: bincode::deserialize(&row.value).expect("failed to parse height"),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct BlockKey {
    code: u8,
    hash: FullHash,
}

pub fn compute_script_hash(data: &[u8]) -> FullHash {
    let mut hash = FullHash::default();
    let mut sha2 = Sha256::new();
    sha2.input(data);
    sha2.result(&mut hash);
    hash
}

pub fn index_transaction(txn: &Transaction, height: usize, rows: &mut Vec<Row>) {
    let null_hash = Sha256dHash::default();
    let txid: Sha256dHash = txn.txid();
    for input in &txn.input {
        if input.prev_hash == null_hash {
            continue;
        }
        rows.push(TxInRow::new(&txid, &input).to_row());
    }
    for output in &txn.output {
        rows.push(TxOutRow::new(&txid, &output).to_row());
    }
    // Persist transaction ID and confirmed height
    rows.push(TxRow::new(&txid, height as u32).to_row());
}

fn index_block(block: &Block, height: usize) -> Vec<Row> {
    let mut rows = vec![];
    for txn in &block.txdata {
        index_transaction(&txn, height, &mut rows);
    }
    let blockhash = block.bitcoin_hash();
    // Persist block hash and header
    rows.push(Row {
        key: bincode::serialize(&BlockKey {
            code: b'B',
            hash: full_hash(&blockhash[..]),
        }).unwrap(),
        value: serialize(&block.header).unwrap(),
    });
    // Store last indexed block (i.e. all previous blocks were indexed)
    rows.push(Row {
        key: b"L".to_vec(),
        value: serialize(&blockhash).unwrap(),
    });
    rows
}

fn read_indexed_headers(store: &ReadStore) -> HeaderList {
    let mut timer = Timer::new();
    let latest_blockhash: Sha256dHash = match store.get(b"L") {
        // latest blockheader persisted in the DB.
        Some(row) => deserialize(&row).unwrap(),
        None => Sha256dHash::default(),
    };
    let mut map = HeaderMap::new();
    for row in store.scan(b"B") {
        let key: BlockKey = bincode::deserialize(&row.key).unwrap();
        let header: BlockHeader = deserialize(&row.value).unwrap();
        map.insert(deserialize(&key.hash).unwrap(), header);
    }
    timer.tick("load");
    let mut headers = vec![];
    let null_hash = Sha256dHash::default();
    let mut blockhash = latest_blockhash;
    while blockhash != null_hash {
        let header = map.remove(&blockhash)
            .expect(&format!("missing {} header in DB", blockhash));
        blockhash = header.prev_blockhash;
        headers.push(header);
    }
    headers.reverse();
    assert_eq!(
        headers
            .first()
            .map(|h| h.prev_blockhash)
            .unwrap_or(null_hash),
        null_hash
    );
    assert_eq!(
        headers
            .last()
            .map(|h| h.bitcoin_hash())
            .unwrap_or(null_hash),
        latest_blockhash
    );
    timer.tick("verify");
    let mut result = HeaderList::empty();
    let headers_len = headers.len();
    let entries = result.order(headers);
    result.apply(entries);
    timer.tick("apply");
    debug!("{} headers' verification {:?}", headers_len, timer);
    result
}

#[derive(Debug)]
struct Stats {
    blocks: usize,
    txns: usize,
    vsize: usize,
}

impl Stats {
    fn new() -> Stats {
        Stats {
            blocks: 0,
            txns: 0,
            vsize: 0,
        }
    }

    fn update(&mut self, block: &Block) {
        self.blocks += 1;
        self.txns += block.txdata.len();
        for tx in &block.txdata {
            self.vsize += tx.get_weight() as usize / 4;
        }
    }
}

pub struct Index {
    // TODO: store also latest snapshot.
    headers: RwLock<HeaderList>,
}

impl Index {
    pub fn load(store: &ReadStore) -> Index {
        Index {
            headers: RwLock::new(read_indexed_headers(store)),
        }
    }

    pub fn best_header(&self) -> Option<HeaderEntry> {
        let headers = self.headers.read().unwrap();
        headers.header_by_blockhash(headers.tip()).cloned()
    }

    pub fn get_header(&self, height: usize) -> Option<HeaderEntry> {
        self.headers
            .read()
            .unwrap()
            .header_by_height(height)
            .cloned()
    }

    pub fn update(
        &self,
        store: &WriteStore,
        daemon: &Daemon,
        waiter: &Waiter,
    ) -> Result<Sha256dHash> {
        let tip = daemon.getbestblockhash()?;
        let new_headers: Vec<HeaderEntry> = {
            let indexed_headers = self.headers.read().unwrap();
            indexed_headers.order(daemon.get_new_headers(&indexed_headers, &tip)?)
        };
        new_headers.last().map(|tip| {
            info!("{:?} ({} left to index)", tip, new_headers.len());
        });
        {
            let mut timer = Timer::new();
            let mut stats = Stats::new();
            let mut bar = util::new_progress_bar(new_headers.len());
            bar.message("Blocks: ");
            let mut buf = BufferedWriter::new(store);
            let headers_map: HashMap<Sha256dHash, &HeaderEntry> =
                HashMap::from_iter(new_headers.iter().map(|h| (*h.hash(), h)));
            for chunk in new_headers.chunks(100) {
                if let Some(sig) = waiter.poll() {
                    bail!("indexing interrupted by {:?}", sig);
                }
                // Download new blocks
                let hashes: Vec<Sha256dHash> = chunk.into_iter().map(|h| *h.hash()).collect();
                let batch = daemon.getblocks(&hashes)?;
                timer.tick("get");
                for block in &batch {
                    let expected_hash = block.bitcoin_hash();
                    let header = headers_map
                        .get(&expected_hash)
                        .expect(&format!("missing header for block {}", expected_hash));

                    // Index it
                    let rows = index_block(block, header.height());
                    timer.tick("index");

                    // Write to DB
                    buf.write(rows);
                    timer.tick("write");
                    stats.update(block);
                }
                if bar.add(batch.len() as u64) % 10000 == 0 {
                    debug!("index update {:?} {:?}", stats, timer);
                }
            }
            buf.flush(); // make sure no row is left behind
            timer.tick("write");
            bar.finish();
            debug!("index update {:?} {:?}", stats, timer);
        }
        self.headers.write().unwrap().apply(new_headers);
        assert_eq!(tip, *self.headers.read().unwrap().tip());
        Ok(tip)
    }
}

struct BufferedWriter<'a> {
    batch: Vec<Row>,
    start: Instant,
    store: &'a WriteStore,
}

impl<'a> BufferedWriter<'a> {
    fn new(store: &'a WriteStore) -> BufferedWriter {
        BufferedWriter {
            batch: vec![],
            start: Instant::now(),
            store,
        }
    }

    fn write(&mut self, mut rows: Vec<Row>) {
        self.batch.append(&mut rows);
        if self.batch.len() > 10_000_000 || self.start.elapsed() > Duration::from_secs(60) {
            self.store.write(self.batch.split_off(0));
            self.start = Instant::now();
        }
    }

    fn flush(&mut self) {
        self.store.write(self.batch.split_off(0));
        self.start = Instant::now();
        self.store.flush(); // sync DB to disk
    }
}
