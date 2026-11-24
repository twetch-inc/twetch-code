//! The durable store: one redb file, every change one transaction.

use crate::model::{outpoint_key, parse_outpoint, Coin, CoinState, Item, Kind, Limits, Outpoint, State};
use anyhow::{Context, Result};
use redb::{Database, ReadableTable, TableDefinition, WriteTransaction};
use serde::{de::DeserializeOwned, Serialize};
use std::path::Path;

const ITEMS: TableDefinition<&str, &[u8]> = TableDefinition::new("items");
/// seq -> id of everything not finished.
const ACTIVE: TableDefinition<u64, &str> = TableDefinition::new("active");
/// seq -> id of everything.
const RECENT: TableDefinition<u64, &str> = TableDefinition::new("recent");
const COINS: TableDefinition<&str, &[u8]> = TableDefinition::new("coins");
/// txids of ours that can never confirm.
const DEAD: TableDefinition<&str, ()> = TableDefinition::new("dead");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
/// day -> micro-USD committed.
const DAYS: TableDefinition<&str, u64> = TableDefinition::new("days");

const SEQ: &str = "seq";
const LIMITS: &str = "limits";
pub const ACCOUNT: &str = "account";

pub struct Db {
    inner: Database,
}

/// One write transaction.
pub struct Tx<'a>(&'a WriteTransaction);

impl Db {
    /// # Errors
    /// When the file is unreadable or another process has it open.
    pub fn open(path: &Path) -> Result<Self> {
        let inner = Database::create(path).with_context(|| format!("open {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        let w = inner.begin_write()?;
        w.open_table(ITEMS)?;
        w.open_table(ACTIVE)?;
        w.open_table(RECENT)?;
        w.open_table(COINS)?;
        w.open_table(DEAD)?;
        w.open_table(META)?;
        w.open_table(DAYS)?;
        w.commit()?;
        Ok(Self { inner })
    }

    /// Run `f` in one transaction: all of it is stored, or none.
    ///
    /// # Errors
    /// What `f` returns, or a failed commit.
    pub fn write<T>(&self, f: impl FnOnce(&Tx) -> Result<T>) -> Result<T> {
        let w = self.inner.begin_write()?;
        let out = f(&Tx(&w))?;
        w.commit()?;
        Ok(out)
    }

    /// # Errors
    /// On a storage failure.
    pub fn item(&self, id: &str) -> Result<Option<Item>> {
        let r = self.inner.begin_read()?;
        let t = r.open_table(ITEMS)?;
        t.get(id)?.map(|v| decode(v.value())).transpose()
    }

    /// Everything unfinished, oldest first.
    ///
    /// # Errors
    /// On a storage failure.
    pub fn active(&self) -> Result<Vec<Item>> {
        let r = self.inner.begin_read()?;
        let (active, items) = (r.open_table(ACTIVE)?, r.open_table(ITEMS)?);
        let mut out = Vec::new();
        for row in active.iter()? {
            if let Some(v) = items.get(row?.1.value())? {
                out.push(decode(v.value())?);
            }
        }
        Ok(out)
    }

    /// # Errors
    /// On a storage failure.
    pub fn recent(&self, limit: usize) -> Result<Vec<Item>> {
        let r = self.inner.begin_read()?;
        let (recent, items) = (r.open_table(RECENT)?, r.open_table(ITEMS)?);
        let mut out = Vec::with_capacity(limit);
        for row in recent.iter()?.rev().take(limit) {
            if let Some(v) = items.get(row?.1.value())? {
                out.push(decode(v.value())?);
            }
        }
        Ok(out)
    }

    /// # Errors
    /// On a storage failure.
    pub fn coins(&self) -> Result<Vec<(Outpoint, Coin)>> {
        let r = self.inner.begin_read()?;
        let t = r.open_table(COINS)?;
        let mut out = Vec::new();
        for row in t.iter()? {
            let (k, v) = row?;
            let op = parse_outpoint(k.value()).context("bad coin key")?;
            out.push((op, decode(v.value())?));
        }
        Ok(out)
    }

    /// Coins we may spend now, largest first.
    ///
    /// # Errors
    /// On a storage failure.
    pub fn available(&self) -> Result<Vec<(Outpoint, u64)>> {
        let mut out: Vec<_> = self.coins()?.into_iter().filter(|(_, c)| c.state == CoinState::Available).map(|(op, c)| (op, c.sats)).collect();
        out.sort_by_key(|c| std::cmp::Reverse(c.1));
        Ok(out)
    }

    /// # Errors
    /// On a storage failure.
    pub fn is_dead(&self, txid: &str) -> Result<bool> {
        let r = self.inner.begin_read()?;
        let t = r.open_table(DEAD)?;
        Ok(t.get(txid)?.is_some())
    }

    /// # Errors
    /// On a storage failure.
    pub fn spent_on(&self, day: &str) -> Result<u64> {
        let r = self.inner.begin_read()?;
        let t = r.open_table(DAYS)?;
        Ok(t.get(day)?.map_or(0, |v| v.value()))
    }

    /// # Errors
    /// On a storage failure.
    pub fn limits(&self) -> Result<Limits> {
        Ok(self.meta(LIMITS)?.unwrap_or_default())
    }

    /// # Errors
    /// On a storage failure.
    pub fn set_limits(&self, limits: Limits) -> Result<()> {
        self.write(|t| t.set_meta(LIMITS, &limits))
    }

    /// # Errors
    /// On a storage failure.
    pub fn meta<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        let r = self.inner.begin_read()?;
        let t = r.open_table(META)?;
        t.get(key)?.map(|v| decode(v.value())).transpose()
    }
}

impl Tx<'_> {
    /// # Errors
    /// On a storage failure.
    pub fn set_meta<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        let bytes = serde_json::to_vec(value)?;
        self.0.open_table(META)?.insert(key, bytes.as_slice())?;
        Ok(())
    }

    /// # Errors
    /// On a storage failure.
    pub fn next_seq(&self) -> Result<u64> {
        let mut t = self.0.open_table(META)?;
        let last: u64 = t.get(SEQ)?.map(|v| decode(v.value())).transpose()?.unwrap_or(0);
        let next = last + 1;
        t.insert(SEQ, serde_json::to_vec(&next)?.as_slice())?;
        Ok(next)
    }

    /// # Errors
    /// On a storage failure.
    pub fn item(&self, id: &str) -> Result<Option<Item>> {
        let t = self.0.open_table(ITEMS)?;
        let v = t.get(id)?.map(|v| decode(v.value())).transpose();
        v
    }

    /// Store an item and keep the unfinished list true.
    ///
    /// # Errors
    /// On a storage failure.
    pub fn put(&self, item: &Item) -> Result<()> {
        let bytes = serde_json::to_vec(item)?;
        self.0.open_table(ITEMS)?.insert(item.id.as_str(), bytes.as_slice())?;
        self.0.open_table(RECENT)?.insert(item.seq, item.id.as_str())?;
        let mut active = self.0.open_table(ACTIVE)?;
        if item.state.is_active() {
            active.insert(item.seq, item.id.as_str())?;
        } else {
            active.remove(item.seq)?;
        }
        Ok(())
    }

    /// Remove an item that was never signed.
    ///
    /// # Errors
    /// On a storage failure, or when it was signed after all.
    pub fn forget(&self, item: &Item) -> Result<()> {
        let stored = self.item(&item.id)?.context("no such payment")?;
        anyhow::ensure!(stored.tx_hex.is_empty() && stored.old_txids.is_empty(), "{} was signed", item.id);
        self.0.open_table(ITEMS)?.remove(item.id.as_str())?;
        self.0.open_table(RECENT)?.remove(item.seq)?;
        self.0.open_table(ACTIVE)?.remove(item.seq)?;
        Ok(())
    }

    /// A new item with the next sequence number.
    ///
    /// # Errors
    /// On a storage failure.
    pub fn new_item(&self, id: String, kind: Kind, state: State, sats: u64, to: &str) -> Result<Item> {
        let seq = self.next_seq()?;
        let id = if id.ends_with(':') { format!("{id}{seq}") } else { id };
        let item = Item {
            id,
            kind,
            seq,
            state,
            created_ms: crate::model::now_ms(),
            model: String::new(),
            sats,
            usd_micros: 0,
            to: to.to_string(),
            txid: String::new(),
            tx_hex: String::new(),
            fee: 0,
            inputs: Vec::new(),
            change: 0,
            change_vout: 0,
            day: String::new(),
            approved: false,
            rebuilt: false,
            old_txids: Vec::new(),
            note: String::new(),
        };
        self.put(&item)?;
        Ok(item)
    }

    /// # Errors
    /// On a storage failure.
    pub fn coin(&self, op: &Outpoint) -> Result<Option<Coin>> {
        let t = self.0.open_table(COINS)?;
        let v = t.get(outpoint_key(op).as_str())?.map(|v| decode(v.value())).transpose();
        v
    }

    /// # Errors
    /// On a storage failure.
    pub fn put_coin(&self, op: &Outpoint, coin: &Coin) -> Result<()> {
        let bytes = serde_json::to_vec(coin)?;
        self.0.open_table(COINS)?.insert(outpoint_key(op).as_str(), bytes.as_slice())?;
        Ok(())
    }

    /// # Errors
    /// On a storage failure.
    pub fn set_coin_state(&self, op: &Outpoint, state: CoinState) -> Result<()> {
        if let Some(mut c) = self.coin(op)? {
            c.state = state;
            self.put_coin(op, &c)?;
        }
        Ok(())
    }

    /// Take the coins for a signed transaction.
    ///
    /// # Errors
    /// When one is not ours to spend any more.
    pub fn spend(&self, inputs: &[Outpoint]) -> Result<()> {
        for op in inputs {
            let c = self.coin(op)?.context("unknown coin")?;
            anyhow::ensure!(c.state == CoinState::Available, "coin {} is not available", outpoint_key(op));
            self.set_coin_state(op, CoinState::Spent)?;
        }
        Ok(())
    }

    /// # Errors
    /// On a storage failure.
    pub fn mark_dead(&self, txid: &str) -> Result<()> {
        self.0.open_table(DEAD)?.insert(txid, ())?;
        Ok(())
    }

    /// # Errors
    /// On a storage failure.
    pub fn add_day(&self, day: &str, micros: u64) -> Result<()> {
        let mut t = self.0.open_table(DAYS)?;
        let have = t.get(day)?.map_or(0, |v| v.value());
        t.insert(day, have + micros)?;
        Ok(())
    }

    /// # Errors
    /// On a storage failure.
    pub fn sub_day(&self, day: &str, micros: u64) -> Result<()> {
        let mut t = self.0.open_table(DAYS)?;
        let have = t.get(day)?.map_or(0, |v| v.value());
        t.insert(day, have.saturating_sub(micros))?;
        Ok(())
    }

    /// Forget the previous wallet's coins and history (restoring a phrase).
    ///
    /// # Errors
    /// On a storage failure.
    pub fn clear_wallet(&self) -> Result<()> {
        self.0.delete_table(ITEMS)?;
        self.0.delete_table(ACTIVE)?;
        self.0.delete_table(RECENT)?;
        self.0.delete_table(COINS)?;
        self.0.delete_table(DEAD)?;
        self.0.open_table(ITEMS)?;
        self.0.open_table(ACTIVE)?;
        self.0.open_table(RECENT)?;
        self.0.open_table(COINS)?;
        self.0.open_table(DEAD)?;
        Ok(())
    }
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    Ok(serde_json::from_slice(bytes)?)
}
