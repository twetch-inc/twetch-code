//! The one thread that holds the recovery phrase and signs.
//!
//! It signs three things: a payment of the inference envelope to [`PAY_TO`],
//! the key-mint message, and a withdraw the user asked for on the wallet page.

use crate::db::{Db, ACCOUNT};
use crate::model::Outpoint;
use crate::secrets::{SecretStore, MNEMONIC};
use crate::{ACTION, FEE_PER_KB, PAY_TO, PAY_TO_PUBKEY};
use anyhow::{anyhow, bail, ensure, Context, Result};
use bitcoin::Transaction;
use std::sync::{mpsc, Arc, RwLock};
use tokio::sync::oneshot;
use twetch_crypto::tx::{self, BuiltTx, Utxo};
use twetch_crypto::wallet::{address_to_p2pkh_hex, Wallet};
use zeroize::Zeroizing;

/// A signed transaction and what it does to our coins.
#[derive(Debug, Clone)]
pub struct Signed {
    pub tx_hex: String,
    pub txid: String,
    pub fee: u64,
    pub inputs: Vec<Outpoint>,
    pub change: u64,
    pub change_vout: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Ready,
    /// The store names a wallet the keychain no longer has.
    Missing,
    /// The keychain would not answer.
    Locked(String),
}

#[derive(Clone, Debug)]
pub struct Account {
    pub pubkey: String,
    pub address: String,
    pub script_hex: String,
    pub status: Status,
}

pub enum Amount {
    Sats(u64),
    Max,
}

enum Job {
    Pay { coins: Vec<(Outpoint, u64)>, sats: u64, to: String, reply: oneshot::Sender<Result<Signed>> },
    Withdraw { coins: Vec<(Outpoint, u64)>, amount: Amount, to: String, reply: oneshot::Sender<Result<Signed>> },
    KeyMessage { name: String, ts: i64, reply: oneshot::Sender<Result<String>> },
    Reveal { reply: oneshot::Sender<Result<Zeroizing<String>>> },
    Restore { phrase: Zeroizing<String>, db: Arc<Db>, reply: oneshot::Sender<Result<Account>> },
}

#[derive(Clone)]
pub struct Signer {
    jobs: mpsc::Sender<Job>,
    account: Arc<RwLock<Account>>,
}

impl Signer {
    /// Open the wallet, making a new one only when neither the store nor the
    /// keychain has ever had one.
    ///
    /// # Errors
    /// On a storage failure, or when the signing key is not the pinned one.
    pub fn open(secrets: Arc<dyn SecretStore>, db: &Db) -> Result<Self> {
        let derived = tx::address_from_pubkey_hex(PAY_TO_PUBKEY).map_err(|e| anyhow!(e))?;
        ensure!(derived == PAY_TO, "the pinned payee key and address disagree");
        let known: Option<String> = db.meta(ACCOUNT)?;
        let (phrase, account) = match (secrets.get(MNEMONIC), known) {
            (Ok(Some(phrase)), known) => {
                let acct = describe(&phrase)?;
                match known {
                    None => db.write(|t| t.set_meta(ACCOUNT, &acct.pubkey))?,
                    Some(k) => ensure!(k == acct.pubkey, "the keychain holds another wallet than this data folder"),
                }
                (Some(phrase), acct)
            }
            (Ok(None), None) => {
                let phrase = generate()?;
                let acct = describe(&phrase)?;
                secrets.set(MNEMONIC, &phrase)?;
                // Read it back: a wallet that cannot be reopened must not take deposits.
                let back = secrets.get(MNEMONIC)?.context("the keychain did not keep the recovery phrase")?;
                ensure!(*back == *phrase, "the keychain did not keep the recovery phrase");
                db.write(|t| t.set_meta(ACCOUNT, &acct.pubkey))?;
                (Some(phrase), acct)
            }
            (Ok(None), Some(pubkey)) => (None, closed(pubkey, Status::Missing)),
            (Err(e), known) => (None, closed(known.unwrap_or_default(), Status::Locked(e.to_string()))),
        };
        let account = Arc::new(RwLock::new(account));
        let (jobs, inbox) = mpsc::channel();
        let shared = account.clone();
        std::thread::Builder::new().name("signer".into()).spawn(move || work(&inbox, phrase, &secrets, &shared))?;
        Ok(Self { jobs, account })
    }

    #[must_use]
    pub fn account(&self) -> Account {
        self.account.read().expect("account").clone()
    }

    async fn ask<T>(&self, make: impl FnOnce(oneshot::Sender<Result<T>>) -> Job) -> Result<T> {
        let (reply, answer) = oneshot::channel();
        self.jobs.send(make(reply)).map_err(|_| anyhow!("the signer stopped"))?;
        answer.await.map_err(|_| anyhow!("the signer stopped"))?
    }

    /// A payment of the contract's envelope. Any payee but the pinned one is refused.
    ///
    /// # Errors
    /// When the payee is wrong, the coins do not cover it, or the built
    /// transaction is not exactly the payment asked for.
    pub async fn pay(&self, coins: Vec<(Outpoint, u64)>, sats: u64, to: &str) -> Result<Signed> {
        let to = to.to_string();
        self.ask(|reply| Job::Pay { coins, sats, to, reply }).await
    }

    /// # Errors
    /// When the address is bad or the coins do not cover it.
    pub async fn withdraw(&self, coins: Vec<(Outpoint, u64)>, amount: Amount, to: &str) -> Result<Signed> {
        let to = to.to_string();
        self.ask(|reply| Job::Withdraw { coins, amount, to, reply }).await
    }

    /// The signature that mints a gateway key.
    ///
    /// # Errors
    /// When the wallet is not open.
    pub async fn key_message(&self, name: &str, ts: i64) -> Result<String> {
        let name = name.to_string();
        self.ask(|reply| Job::KeyMessage { name, ts, reply }).await
    }

    /// The recovery phrase, for the user's own eyes.
    ///
    /// # Errors
    /// When the wallet is not open.
    pub async fn reveal(&self) -> Result<Zeroizing<String>> {
        self.ask(|reply| Job::Reveal { reply }).await
    }

    /// Replace the wallet with a backed-up phrase. The caller checks the old one is empty.
    ///
    /// # Errors
    /// When the phrase is bad or cannot be stored.
    pub async fn restore(&self, phrase: Zeroizing<String>, db: Arc<Db>) -> Result<Account> {
        self.ask(|reply| Job::Restore { phrase, db, reply }).await
    }
}

fn closed(pubkey: String, status: Status) -> Account {
    let address = tx::address_from_pubkey_hex(&pubkey).unwrap_or_default();
    let script_hex = address_to_p2pkh_hex(&address).unwrap_or_default();
    Account { pubkey, address, script_hex, status }
}

fn generate() -> Result<Zeroizing<String>> {
    let mut entropy = Zeroizing::new([0u8; 16]);
    getrandom::getrandom(entropy.as_mut()).map_err(|e| anyhow!("no entropy: {e}"))?;
    Ok(Zeroizing::new(bip39::Mnemonic::from_entropy(entropy.as_ref())?.to_string()))
}

fn describe(phrase: &str) -> Result<Account> {
    let w = Wallet::from_mnemonic(phrase).map_err(|_| anyhow!("that is not a valid recovery phrase"))?;
    let address = w.account_address().map_err(|e| anyhow!(e))?;
    Ok(Account {
        pubkey: w.account_pubkey_hex().map_err(|e| anyhow!(e))?,
        script_hex: address_to_p2pkh_hex(&address).map_err(|e| anyhow!(e))?,
        address,
        status: Status::Ready,
    })
}

fn work(inbox: &mpsc::Receiver<Job>, mut phrase: Option<Zeroizing<String>>, secrets: &Arc<dyn SecretStore>, account: &RwLock<Account>) {
    while let Ok(job) = inbox.recv() {
        let open = || -> Result<Wallet> {
            let p = phrase.as_ref().ok_or_else(|| anyhow!("the wallet is not open: its recovery phrase is not in the keychain"))?;
            Wallet::from_mnemonic(p).map_err(|_| anyhow!("the recovery phrase in the keychain is not valid"))
        };
        // A caller that gave up is not an error.
        match job {
            Job::Pay { coins, sats, to, reply } => {
                let _ = reply.send(open().and_then(|w| pay(&w, &coins, sats, &to)));
            }
            Job::Withdraw { coins, amount, to, reply } => {
                let _ = reply.send(open().and_then(|w| withdraw(&w, &coins, &amount, &to)));
            }
            Job::KeyMessage { name, ts, reply } => {
                let _ = reply.send(open().and_then(|w| key_message(&w, &name, ts)));
            }
            Job::Reveal { reply } => {
                let _ = reply.send(open().map(|_| phrase.clone().unwrap_or_default()));
            }
            Job::Restore { phrase: new, db, reply } => {
                let done = restore(&new, secrets.as_ref(), &db);
                if let Ok(a) = &done {
                    twetch_crypto::tx::clear_key_cache();
                    *account.write().expect("account") = a.clone();
                    phrase = Some(Zeroizing::new(normal(&new)));
                }
                let _ = reply.send(done);
            }
        }
    }
    twetch_crypto::tx::clear_key_cache();
}

fn normal(phrase: &str) -> String {
    phrase.split_whitespace().map(str::to_lowercase).collect::<Vec<_>>().join(" ")
}

fn restore(phrase: &str, secrets: &dyn SecretStore, db: &Db) -> Result<Account> {
    let phrase = Zeroizing::new(normal(phrase));
    let acct = describe(&phrase)?;
    secrets.set(MNEMONIC, &phrase)?;
    secrets.delete(crate::secrets::GATEWAY_KEY)?;
    db.write(|t| {
        t.clear_wallet()?;
        t.set_meta(ACCOUNT, &acct.pubkey)
    })?;
    Ok(acct)
}

fn key_message(w: &Wallet, name: &str, ts: i64) -> Result<String> {
    let pubkey = w.account_pubkey_hex().map_err(|e| anyhow!(e))?;
    let sk = w.account_secret_key().map_err(|e| anyhow!(e))?;
    let msg = format!("twetch-inference-key\n{pubkey}\n{name}\n{ts}");
    Ok(twetch_crypto::sign::bsm_sign(&sk, msg.as_bytes()))
}

fn utxos(coins: &[(Outpoint, u64)], script_hex: &str) -> Vec<Utxo> {
    coins.iter().map(|((txid, vout), sats)| Utxo { txid: txid.clone(), vout: *vout, satoshis: *sats, script_hex: script_hex.to_string() }).collect()
}

fn pay(w: &Wallet, coins: &[(Outpoint, u64)], sats: u64, to: &str) -> Result<Signed> {
    ensure!(to == PAY_TO, "refused: {to} is not Twetch's payment address");
    ensure!(sats > 0, "nothing to pay");
    let address = w.account_address().map_err(|e| anyhow!(e))?;
    let pubkey = w.account_pubkey_hex().map_err(|e| anyhow!(e))?;
    let script = address_to_p2pkh_hex(&address).map_err(|e| anyhow!(e))?;
    let utxos = utxos(coins, &script);
    let built = tx::build_pay_split_action_tx(w, &utxos, ACTION, "pay", "account", &pubkey, &[(PAY_TO.to_string(), sats)], &address, FEE_PER_KB)
        .map_err(|e| anyhow!(e))?;
    let signed = check(built, coins, &script, &address_to_p2pkh_hex(PAY_TO).map_err(|e| anyhow!(e))?, Some(sats), true)?;
    let parsed = parse(&signed.tx_hex)?;
    let author = tx::aip_verified_signer_tx(&parsed).map_err(|e| anyhow!(e))?;
    ensure!(author == address, "the payment is not signed by this account");
    Ok(signed)
}

fn withdraw(w: &Wallet, coins: &[(Outpoint, u64)], amount: &Amount, to: &str) -> Result<Signed> {
    let address = w.account_address().map_err(|e| anyhow!(e))?;
    let script = address_to_p2pkh_hex(&address).map_err(|e| anyhow!(e))?;
    let to_script = address_to_p2pkh_hex(to).map_err(|_| anyhow!("that is not a BSV address"))?;
    let utxos = utxos(coins, &script);
    let (built, sats) = match amount {
        Amount::Sats(s) => {
            ensure!(*s > 0, "enter an amount");
            (tx::build_send_tx(w, &utxos, to, *s, &address, FEE_PER_KB), Some(*s))
        }
        Amount::Max => (tx::build_send_max_tx(w, &utxos, to, FEE_PER_KB), None),
    };
    check(built.map_err(|e| anyhow!(e))?, coins, &script, &to_script, sats, false)
}

fn parse(tx_hex: &str) -> Result<Transaction> {
    Ok(bitcoin::consensus::deserialize(&hex::decode(tx_hex)?)?)
}

/// Read the finished bytes back: our coins in, the payee and our change out,
/// a fee that is only a fee, and nothing else.
fn check(built: BuiltTx, coins: &[(Outpoint, u64)], own: &str, payee: &str, sats: Option<u64>, envelope: bool) -> Result<Signed> {
    let t = parse(&built.tx_hex)?;
    let mut inputs = Vec::with_capacity(t.input.len());
    let mut total_in = 0u64;
    for i in &t.input {
        let op = (i.previous_output.txid.to_string(), i.previous_output.vout);
        let coin = coins.iter().find(|(c, _)| *c == op).context("the transaction spends a coin that is not ours")?;
        total_in += coin.1;
        inputs.push(op);
    }
    let first = usize::from(envelope);
    if envelope {
        let o = t.output.first().context("no outputs")?;
        ensure!(o.value.to_sat() == 0 && o.script_pubkey.as_bytes().starts_with(&[0x00, 0x6a]), "no envelope");
    }
    let pay = t.output.get(first).context("no payee")?;
    ensure!(hex::encode(pay.script_pubkey.as_bytes()) == payee, "wrong payee");
    if let Some(s) = sats {
        ensure!(pay.value.to_sat() == s, "wrong amount");
    }
    let (change, change_vout) = match t.output.get(first + 1) {
        None => (0, 0),
        Some(o) if hex::encode(o.script_pubkey.as_bytes()) == own => (o.value.to_sat(), u32::try_from(first + 1)?),
        Some(_) => bail!("the change does not come back to this wallet"),
    };
    ensure!(t.output.len() <= first + 2, "an output nobody asked for");
    let out: u64 = t.output.iter().map(|o| o.value.to_sat()).sum();
    let fee = total_in.checked_sub(out).context("outputs exceed inputs")?;
    let size = (built.tx_hex.len() / 2) as u64;
    ensure!(fee <= size * FEE_PER_KB * 2 / 1000 + 100, "the network fee is too high");
    let txid = t.compute_txid().to_string();
    Ok(Signed { tx_hex: built.tx_hex, txid, fee, inputs, change, change_vout })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::MemorySecrets;

    const PHRASE: &str = "book fit fly ketchup also elevator scout mind edit fatal where rookie";

    fn wallet() -> Wallet {
        Wallet::from_mnemonic(PHRASE).unwrap()
    }

    fn coins(sats: &[u64]) -> Vec<(Outpoint, u64)> {
        sats.iter().enumerate().map(|(i, s)| ((format!("{:064x}", i + 1), 0), *s)).collect()
    }

    #[test]
    fn pays_only_the_pinned_address() {
        let w = wallet();
        let s = pay(&w, &coins(&[100_000]), 1234, PAY_TO).unwrap();
        let t = parse(&s.tx_hex).unwrap();
        assert_eq!(tx::action_payout_to_pubkey_tx(&t, PAY_TO_PUBKEY), 1234);
        assert_eq!(s.change, 100_000 - 1234 - s.fee);
        assert_eq!(s.change_vout, 2);
        for other in [w.account_address().unwrap().as_str(), "1BoatSLRHtKNngkdXEeobR76b53LETtpyT", "", "1J82Vbt4Uz8kKr2Xb3Kro9WtCh6udoctCj"] {
            assert!(pay(&w, &coins(&[100_000]), 1234, other).unwrap_err().to_string().contains("refused"));
        }
        assert!(pay(&w, &coins(&[100_000]), 0, PAY_TO).is_err());
        assert!(pay(&w, &coins(&[1000]), 1234, PAY_TO).unwrap_err().to_string().contains("insufficient"));
    }

    #[test]
    fn many_coins_and_no_change() {
        let w = wallet();
        let s = pay(&w, &coins(&[500, 700, 900, 400]), 1580, PAY_TO).unwrap();
        assert!(s.inputs.len() >= 3);
        assert!(s.fee * 1000 >= s.tx_hex.len() as u64 / 2 * 100, "at least 100 sat/kB");
        let all = withdraw(&w, &coins(&[500, 700]), &Amount::Max, "1BoatSLRHtKNngkdXEeobR76b53LETtpyT").unwrap();
        assert_eq!((all.change, all.inputs.len()), (0, 2));
        assert!(withdraw(&w, &coins(&[5000]), &Amount::Sats(10), "not an address").is_err());
    }

    #[test]
    fn a_tampered_transaction_is_caught() {
        let w = wallet();
        let cs = coins(&[100_000]);
        let own = address_to_p2pkh_hex(&w.account_address().unwrap()).unwrap();
        let payee = address_to_p2pkh_hex(PAY_TO).unwrap();
        let build = |to: &str, sats: u64, change: &str, fee: u64| {
            tx::build_pay_split_action_tx(&w, &utxos(&cs, &own), ACTION, "pay", "account", "x", &[(to.into(), sats)], change, fee).unwrap()
        };
        let me = w.account_address().unwrap();
        assert!(check(build(PAY_TO, 10, &me, 105), &cs, &own, &payee, Some(10), true).is_ok());
        assert!(check(build(PAY_TO, 11, &me, 105), &cs, &own, &payee, Some(10), true).is_err());
        assert!(check(build(&me, 10, &me, 105), &cs, &own, &payee, Some(10), true).is_err());
        assert!(check(build(PAY_TO, 10, "1BoatSLRHtKNngkdXEeobR76b53LETtpyT", 105), &cs, &own, &payee, Some(10), true).is_err());
        assert!(check(build(PAY_TO, 10, &me, 50_000), &cs, &own, &payee, Some(10), true).is_err());
        assert!(check(build(PAY_TO, 10, &me, 105), &coins(&[]), &own, &payee, Some(10), true).is_err());
    }

    #[test]
    fn a_wallet_is_made_once_and_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("w.redb")).unwrap();
        let secrets = Arc::new(MemorySecrets::default());
        let a = Signer::open(secrets.clone(), &db).unwrap().account();
        assert_eq!(a.status, Status::Ready);
        assert_eq!(secrets.get(MNEMONIC).unwrap().unwrap().split(' ').count(), 12);
        assert_eq!(Signer::open(secrets.clone(), &db).unwrap().account().pubkey, a.pubkey);
        // A locked keychain is not an empty one.
        secrets.locked.store(true, std::sync::atomic::Ordering::SeqCst);
        let locked = Signer::open(secrets.clone(), &db).unwrap().account();
        assert!(matches!(locked.status, Status::Locked(_)));
        assert_eq!(locked.address, a.address);
        secrets.locked.store(false, std::sync::atomic::Ordering::SeqCst);
        // A lost phrase never becomes a silent new wallet.
        secrets.delete(MNEMONIC).unwrap();
        let lost = Signer::open(secrets.clone(), &db).unwrap().account();
        assert_eq!((lost.status, lost.address), (Status::Missing, a.address));
        assert!(secrets.get(MNEMONIC).unwrap().is_none());
        // Somebody else's phrase in the keychain is refused.
        secrets.set(MNEMONIC, PHRASE).unwrap();
        assert!(Signer::open(secrets, &db).is_err());
    }
}
