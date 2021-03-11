use std::cell::RefCell;
use std::collections::HashSet;

use chrono::Utc;
#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};
use sqlite::{Connection, State, Statement};

use crate::{Block, Bytes, Keystore, Transaction};
use crate::blockchain::constants::{BLOCK_DIFFICULTY, CHAIN_VERSION, LOCKER_BLOCK_COUNT, LOCKER_BLOCK_INTERVAL, LOCKER_BLOCK_START, LOCKER_DIFFICULTY};
use crate::blockchain::enums::BlockQuality;
use crate::blockchain::enums::BlockQuality::*;
use crate::blockchain::hash_utils::*;
use crate::settings::Settings;

const DB_NAME: &str = "blockchain.db";
const SQL_CREATE_TABLES: &str = "CREATE TABLE blocks (
                                 'id' BIGINT NOT NULL PRIMARY KEY,
                                 'timestamp' BIGINT NOT NULL,
                                 'version' INT,
                                 'difficulty' INTEGER,
                                 'random' INTEGER,
                                 'nonce' INTEGER,
                                 'transaction' TEXT,
                                 'prev_block_hash' BINARY,
                                 'hash' BINARY,
                                 'pub_key' BINARY,
                                 'signature' BINARY);
            CREATE INDEX block_index ON blocks (id);
            CREATE TABLE transactions (id INTEGER PRIMARY KEY AUTOINCREMENT, identity BINARY, confirmation BINARY, method TEXT, data TEXT, pub_key BINARY);
            CREATE INDEX ids ON transactions (identity);";
const SQL_ADD_BLOCK: &str = "INSERT INTO blocks (id, timestamp, version, difficulty, random, nonce, 'transaction',\
                          prev_block_hash, hash, pub_key, signature) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?);";
const SQL_GET_LAST_BLOCK: &str = "SELECT * FROM blocks ORDER BY id DESC LIMIT 1;";
const SQL_ADD_TRANSACTION: &str = "INSERT INTO transactions (identity, confirmation, method, data, pub_key) VALUES (?, ?, ?, ?, ?)";
const SQL_GET_BLOCK_BY_ID: &str = "SELECT * FROM blocks WHERE id=? LIMIT 1;";
const SQL_GET_LAST_FULL_BLOCK: &str = "SELECT * FROM blocks WHERE `transaction`<>'' ORDER BY id DESC LIMIT 1;";
const SQL_GET_PUBLIC_KEY_BY_ID: &str = "SELECT pub_key FROM transactions WHERE identity = ? ORDER BY id DESC LIMIT 1;";
const SQL_GET_ID_BY_ID: &str = "SELECT identity FROM transactions WHERE identity = ? ORDER BY id DESC LIMIT 1;";
const SQL_GET_TRANSACTION_BY_ID: &str = "SELECT * FROM transactions WHERE identity = ? ORDER BY id DESC LIMIT 1;";

pub struct Chain {
    origin: Bytes,
    pub version: u32,
    pub blocks: Vec<Block>,
    last_block: Option<Block>,
    last_full_block: Option<Block>,
    max_height: u64,
    db: Connection,
    zones: RefCell<HashSet<String>>,
}

impl Chain {
    pub fn new(settings: &Settings) -> Self {
        let origin = settings.get_origin();

        let db = sqlite::open(DB_NAME).expect("Unable to open blockchain DB");
        let mut chain = Chain {
            origin,
            version: CHAIN_VERSION,
            blocks: Vec::new(),
            last_block: None,
            last_full_block: None,
            max_height: 0,
            db,
            zones: RefCell::new(HashSet::new()),
        };
        chain.init_db();
        chain
    }

    /// Reads options from DB or initializes and writes them to DB if not found
    fn init_db(&mut self) {
        // Trying to get last block from DB to check its version
        let block: Option<Block> = match self.db.prepare(SQL_GET_LAST_BLOCK) {
            Ok(mut statement) => {
                let mut result = None;
                while statement.next().unwrap() == State::Row {
                    match Self::get_block_from_statement(&mut statement) {
                        None => {
                            error!("Something wrong with block in DB!");
                            panic!();
                        }
                        Some(block) => {
                            debug!("Loaded last block: {:?}", &block);
                            result = Some(block);
                            break;
                        }
                    }
                }
                result
            }
            Err(_) => {
                info!("No blockchain database found. Creating new.");
                self.db.execute(SQL_CREATE_TABLES).expect("Error creating blocks table");
                None
            }
        };
        // If some block loaded we check its version and determine if we need some migration
        if let Some(block) = block {
            self.max_height = block.index;
            if self.version > block.version {
                self.migrate_db(block.version, self.version);
            } else if self.version < block.version {
                error!("Version downgrade {}->{} is not supported!", block.version, self.version);
                panic!();
            }
            // Cache some info
            self.last_block = Some(block.clone());
            if block.transaction.is_some() {
                self.last_full_block = Some(block);
            } else {
                self.last_full_block = self.get_last_full_block();
            }
        }
    }

    fn migrate_db(&mut self, from: u32, to: u32) {
        debug!("Migrating DB from {} to {}", from, to);
    }

    pub fn add_block(&mut self, block: Block) {
        info!("Adding block:\n{:?}", &block);
        self.blocks.push(block.clone());
        self.last_block = Some(block.clone());
        if block.transaction.is_some() {
            self.last_full_block = Some(block.clone());
        }
        let transaction = block.transaction.clone();
        if self.add_block_to_table(block).is_ok() {
            if let Some(transaction) = transaction {
                self.add_transaction_to_table(&transaction).expect("Error adding transaction");
            }
        }
    }

    /// Adds block to blocks table
    fn add_block_to_table(&mut self, block: Block) -> sqlite::Result<State> {
        let mut statement = self.db.prepare(SQL_ADD_BLOCK)?;
        statement.bind(1, block.index as i64)?;
        statement.bind(2, block.timestamp as i64)?;
        statement.bind(3, block.version as i64)?;
        statement.bind(4, block.difficulty as i64)?;
        statement.bind(5, block.random as i64)?;
        statement.bind(6, block.nonce as i64)?;
        match &block.transaction {
            None => { statement.bind(7, "")?; }
            Some(transaction) => {
                statement.bind(7, transaction.to_string().as_str())?;
            }
        }
        statement.bind(8, block.prev_block_hash.as_slice())?;
        statement.bind(9, block.hash.as_slice())?;
        statement.bind(10, block.pub_key.as_slice())?;
        statement.bind(11, block.signature.as_slice())?;
        statement.next()
    }

    /// Adds transaction to transactions table
    fn add_transaction_to_table(&mut self, t: &Transaction) -> sqlite::Result<State> {
        let mut statement = self.db.prepare(SQL_ADD_TRANSACTION)?;
        statement.bind(1, t.identity.as_slice())?;
        statement.bind(2, t.confirmation.as_slice())?;
        statement.bind(3, t.method.as_ref() as &str)?;
        statement.bind(4, t.data.as_ref() as &str)?;
        statement.bind(5, t.pub_key.as_slice())?;
        statement.next()
    }

    pub fn get_block(&self, index: u64) -> Option<Block> {
        match self.db.prepare(SQL_GET_BLOCK_BY_ID) {
            Ok(mut statement) => {
                statement.bind(1, index as i64).expect("Error in bind");
                while statement.next().unwrap() == State::Row {
                    return match Self::get_block_from_statement(&mut statement) {
                        None => {
                            error!("Something wrong with block in DB!");
                            None
                        }
                        Some(block) => {
                            trace!("Loaded block: {:?}", &block);
                            Some(block)
                        }
                    };
                }
                None
            }
            Err(_) => {
                warn!("Can't find requested block {}", index);
                None
            }
        }
    }

    /// Gets last block that has a Transaction within
    pub fn get_last_full_block(&self) -> Option<Block> {
        match self.db.prepare(SQL_GET_LAST_FULL_BLOCK) {
            Ok(mut statement) => {
                while statement.next().unwrap() == State::Row {
                    return match Self::get_block_from_statement(&mut statement) {
                        None => {
                            error!("Something wrong with block in DB!");
                            None
                        }
                        Some(block) => {
                            trace!("Got last full block: {:?}", &block);
                            Some(block)
                        }
                    };
                }
                None
            }
            Err(e) => {
                warn!("Can't find any full blocks: {}", e);
                None
            }
        }
    }

    /// Checks if any domain is available to mine for this client (pub_key)
    pub fn is_domain_available(&self, domain: &str, keystore: &Keystore) -> bool {
        if domain.is_empty() {
            return false;
        }
        let identity_hash = hash_identity(domain, None);
        if !self.is_id_available(&identity_hash, &keystore.get_public()) {
            return false;
        }

        let parts: Vec<&str> = domain.rsplitn(2, ".").collect();
        if parts.len() > 1 {
            // We do not support third level domains
            if parts.last().unwrap().contains(".") {
                return false;
            }
            return self.is_zone_in_blockchain(parts.first().unwrap());
        }
        true
    }

    /// Checks if this identity is free or is owned by the same pub_key
    pub fn is_id_available(&self, identity: &Bytes, public_key: &Bytes) -> bool {
        let mut statement = self.db.prepare(SQL_GET_PUBLIC_KEY_BY_ID).unwrap();
        statement.bind(1, identity.as_slice()).expect("Error in bind");
        while let State::Row = statement.next().unwrap() {
            let pub_key = Bytes::from_bytes(statement.read::<Vec<u8>>(0).unwrap().as_slice());
            if !pub_key.eq(public_key) {
                return false;
            }
        }
        true
    }

    /// Checks if some zone exists in our blockchain
    pub fn is_zone_in_blockchain(&self, zone: &str) -> bool {
        if self.zones.borrow().contains(zone) {
            return true;
        }

        // Checking for existing zone in DB
        let identity_hash = hash_identity(zone, None);
        let mut statement = self.db.prepare(SQL_GET_ID_BY_ID).unwrap();
        statement.bind(1, identity_hash.as_slice()).expect("Error in bind");
        while let State::Row = statement.next().unwrap() {
            // If there is such a zone
            self.zones.borrow_mut().insert(zone.to_owned());
            return true;
        }
        false
    }

    /// Gets full Transaction info for any domain. Used by DNS part.
    pub fn get_domain_transaction(&self, domain: &str) -> Option<Transaction> {
        if domain.is_empty() {
            return None;
        }
        let identity_hash = hash_identity(domain, None);

        let mut statement = self.db.prepare(SQL_GET_TRANSACTION_BY_ID).unwrap();
        statement.bind(1, identity_hash.as_slice()).expect("Error in bind");
        while let State::Row = statement.next().unwrap() {
            let identity = Bytes::from_bytes(statement.read::<Vec<u8>>(1).unwrap().as_slice());
            let confirmation = Bytes::from_bytes(statement.read::<Vec<u8>>(2).unwrap().as_slice());
            let method = statement.read::<String>(3).unwrap();
            let data = statement.read::<String>(4).unwrap();
            let pub_key = Bytes::from_bytes(statement.read::<Vec<u8>>(5).unwrap().as_slice());
            let transaction = Transaction { identity, confirmation, method, data, pub_key };
            debug!("Found transaction for domain {}: {:?}", domain, &transaction);
            if transaction.check_identity(domain) {
                return Some(transaction);
            }
        }
        None
    }

    pub fn get_domain_info(&self, domain: &str) -> Option<String> {
        match self.get_domain_transaction(domain) {
            None => { None }
            Some(transaction) => { Some(transaction.data) }
        }
    }

    pub fn last_block(&self) -> Option<Block> {
        self.last_block.clone()
    }

    pub fn height(&self) -> u64 {
        match self.last_block {
            None => { 0u64 }
            Some(ref block) => {
                block.index
            }
        }
    }

    pub fn last_hash(&self) -> Bytes {
        match &self.last_block {
            None => { Bytes::default() }
            Some(block) => { block.hash.clone() }
        }
    }

    pub fn max_height(&self) -> u64 {
        self.max_height
    }

    pub fn update_max_height(&mut self, height: u64) {
        if height > self.max_height {
            self.max_height = height;
        }
    }

    /// Check if this block can be added to our blockchain
    pub fn check_new_block(&self, block: &Block) -> BlockQuality {
        let timestamp = Utc::now().timestamp();
        if block.timestamp > timestamp {
            warn!("Ignoring block from the future:\n{:?}", &block);
            return Bad;
        }
        let difficulty = match block.transaction {
            None => { LOCKER_DIFFICULTY }
            Some(_) => { BLOCK_DIFFICULTY }
        };
        if block.difficulty < difficulty {
            warn!("Block difficulty is lower than needed");
            return Bad;
        }
        if !hash_is_good(block.hash.as_slice(), block.difficulty as usize) {
            warn!("Ignoring block with low difficulty:\n{:?}", &block);
            return Bad;
        }
        if !check_block_hash(block) {
            warn!("Block {:?} has wrong hash! Ignoring!", &block);
            return Bad;
        }
        if !check_block_signature(&block) {
            warn!("Block {:?} has wrong signature! Ignoring!", &block);
            return Bad;
        }
        if let Some(transaction) = &block.transaction {
            if !self.is_id_available(&transaction.identity, &block.pub_key) {
                warn!("Block {:?} is trying to spoof an identity!", &block);
                return Bad;
            }
        }
        match &self.last_block {
            None => {
                if !block.is_genesis() {
                    warn!("Block is from the future, how is this possible?");
                    return Future;
                }
                if !self.origin.is_zero() && block.hash != self.origin {
                    warn!("Mining gave us a bad block:\n{:?}", &block);
                    return Bad;
                }
            }
            Some(last_block) => {
                if block.timestamp < last_block.timestamp && block.index > last_block.index {
                    warn!("Ignoring block with timestamp/index collision:\n{:?}", &block);
                    return Bad;
                }
                if last_block.index + 1 < block.index {
                    warn!("Block is from the future, how is this possible?");
                    return Future;
                }
                if block.index <= last_block.index {
                    if last_block.hash == block.hash {
                        warn!("Ignoring block {}, we already have it", block.index);
                        return Twin;
                    }
                    if let Some(my_block) = self.get_block(block.index) {
                        return if my_block.hash != block.hash {
                            warn!("Got forked block {} with hash {:?} instead of {:?}", block.index, block.hash, last_block.hash);
                            Fork
                        } else {
                            warn!("Ignoring block {}, we already have it", block.index);
                            Twin
                        };
                    }
                }
                if block.transaction.is_none() {
                    if let Some(locker) = self.get_block_locker(&last_block, block.timestamp) {
                        if locker != block.pub_key {
                            warn!("Ignoring block {}, as wrong locker", block.index);
                            return Bad;
                        }
                    }
                }
            }
        }

        Good
    }

    /// Gets a public key of a node that needs to mine "locker" block above this block
    pub fn get_block_locker(&self, block: &Block, timestamp: i64) -> Option<Bytes> {
        if block.hash.is_empty() || block.hash.is_zero() {
            return None;
        }
        if block.index < LOCKER_BLOCK_START {
            return None;
        }
        match self.get_last_full_block() {
            Some(b) => {
                if b.index + LOCKER_BLOCK_COUNT <= block.index {
                    trace!("Block {} is locked enough", b.index);
                    return None;
                }
            }
            None => {}
        }
        // How many 5 min intervals have passed since this block?
        let intervals = ((timestamp - block.timestamp) / LOCKER_BLOCK_INTERVAL) as u64;
        let tail = block.hash.get_tail_u64();
        let start_index = 1 + ((tail + tail * intervals) % (block.index - 2));
        for index in start_index..block.index {
            if let Some(b) = self.get_block(index) {
                if b.pub_key != block.pub_key {
                    trace!("Locker block for block {} must be mined by owner of block {} block_hash: {:?}", block.index, b.index, block.hash);
                    return Some(b.pub_key);
                }
            }
        }
        None
    }

    fn get_block_from_statement(statement: &mut Statement) -> Option<Block> {
        let index = statement.read::<i64>(0).unwrap() as u64;
        let timestamp = statement.read::<i64>(1).unwrap();
        let version = statement.read::<i64>(2).unwrap() as u32;
        let difficulty = statement.read::<i64>(3).unwrap() as u32;
        let random = statement.read::<i64>(4).unwrap() as u32;
        let nonce = statement.read::<i64>(5).unwrap() as u64;
        let transaction = Transaction::from_json(&statement.read::<String>(6).unwrap());
        let prev_block_hash = Bytes::from_bytes(statement.read::<Vec<u8>>(7).unwrap().as_slice());
        let hash = Bytes::from_bytes(statement.read::<Vec<u8>>(8).unwrap().as_slice());
        let pub_key = Bytes::from_bytes(statement.read::<Vec<u8>>(9).unwrap().as_slice());
        let signature = Bytes::from_bytes(statement.read::<Vec<u8>>(10).unwrap().as_slice());
        Some(Block::from_all_params(index, timestamp, version, difficulty, random, nonce, prev_block_hash, hash, pub_key, signature, transaction))
    }
}