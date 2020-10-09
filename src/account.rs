//
// Copyright 2018-2019 Tamas Blummer
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
//!
//! # Accounts
//!
//! Accounts compatible with BIP32, BIP39, BIP44, BIP49, BIP84
//!
use bitcoin::hashes::{hash160, Hash};
use bitcoin::util::bip32::ExtendedPubKey;
use bitcoin::{
    blockdata::script::Builder,
    blockdata::{
        opcodes::all,
        transaction::{SigHashType, TxOut},
    },
    network::constants::Network,
    util::bip143,
    util::bip32::{ChildNumber, ExtendedPrivKey},
    Address, OutPoint, PrivateKey, PublicKey, Script, Transaction,
};
use crypto::{
    aes, blockmodes, buffer,
    buffer::{BufferResult, ReadBuffer, WriteBuffer},
    digest::Digest,
    sha2::Sha256,
};
use rand::{thread_rng, RngCore};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use context::SecpContext;
use error::Error;
use sss::{ShamirSecretSharing, Share};

use crate::mnemonic::Mnemonic;

/// chose your security level
#[derive(Copy, Clone)]
pub enum MasterKeyEntropy {
    Sufficient = 16,
    Double = 32,
    Paranoid = 64,
}

/// A masterAccount is the root of an account hierarchy
pub struct MasterAccount {
    master_public: ExtendedPubKey,
    encrypted: Vec<u8>,
    accounts: HashMap<(u32, u32), Account>,
    birth: u64,
}

impl MasterAccount {
    /// create a new random master account
    /// the information that leads to private key is stored encrypted with passphrase
    pub fn new(
        entropy: MasterKeyEntropy,
        network: Network,
        passphrase: &str,
    ) -> Result<MasterAccount, Error> {
        let mut random = vec![0u8; entropy as usize];
        thread_rng().fill_bytes(random.as_mut_slice());
        let seed = Seed(random);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        Self::from_seed(&seed, now, network, passphrase)
    }

    /// Restore from encrypted store
    pub fn from_encrypted(
        encrypted: &[u8],
        public_master_key: ExtendedPubKey,
        birth: u64,
    ) -> MasterAccount {
        let encrypted = encrypted.to_vec();
        MasterAccount {
            master_public: public_master_key,
            encrypted,
            accounts: HashMap::new(),
            birth,
        }
    }

    /// A watch only master. You will not be able to sign with this.
    pub fn watch_only(public_master_key: ExtendedPubKey, birth: u64) -> MasterAccount {
        MasterAccount {
            master_public: public_master_key,
            encrypted: Vec::new(),
            accounts: HashMap::new(),
            birth,
        }
    }

    /// Restore from BIP39 mnemonic
    pub fn from_mnemonic(
        mnemonic: &Mnemonic,
        birth: u64,
        network: Network,
        passphrase: &str,
        pd_passphrase: Option<&str>,
    ) -> Result<MasterAccount, Error> {
        let seed = mnemonic.to_seed(pd_passphrase);
        Self::from_seed(&seed, birth, network, passphrase)
    }

    /// Restore from Shamir's Secret Shares (SLIP-0039)
    pub fn from_shares(
        shares: &[Share],
        birth: u64,
        network: Network,
        passphrase: &str,
        pd_passphrase: Option<&str>,
    ) -> Result<MasterAccount, Error> {
        let seed = ShamirSecretSharing::combine(shares, pd_passphrase)?;
        Self::from_seed(&seed, birth, network, passphrase)
    }

    pub fn from_seed(
        seed: &Seed,
        birth: u64,
        network: Network,
        passphrase: &str,
    ) -> Result<MasterAccount, Error> {
        let context = SecpContext::new();
        let encrypted = seed.encrypt(passphrase)?;
        let master_key = context.master_private_key(network, &seed)?;
        let public_master_key = context.extended_public_from_private(&master_key);
        Ok(MasterAccount {
            master_public: public_master_key,
            encrypted,
            accounts: HashMap::new(),
            birth,
        })
    }

    pub fn seed(&self, network: Network, passphrase: &str) -> Result<Seed, Error> {
        let context = SecpContext::new();
        let seed = Seed::decrypt(self.encrypted.as_slice(), passphrase)?;
        let master_key = context.master_private_key(network, &seed)?;
        if self.master_public != context.extended_public_from_private(&master_key) {
            return Err(Error::Passphrase);
        }
        Ok(seed)
    }

    pub fn master_public(&self) -> &ExtendedPubKey {
        &self.master_public
    }

    pub fn encrypted(&self) -> &Vec<u8> {
        &self.encrypted
    }

    pub fn birth(&self) -> u64 {
        self.birth
    }

    pub fn get(&self, account: (u32, u32)) -> Option<&Account> {
        self.accounts.get(&account)
    }

    pub fn get_mut(&mut self, account: (u32, u32)) -> Option<&mut Account> {
        self.accounts.get_mut(&account)
    }

    pub fn accounts(&self) -> &HashMap<(u32, u32), Account> {
        &self.accounts
    }

    pub fn get_scripts<'a>(&'a self) -> impl Iterator<Item = (Script, KeyDerivation)> + 'a {
        self.accounts.iter().flat_map(|((an, sub), a)| {
            a.get_scripts().map(move |(kix, s, tweak, csv)| {
                (
                    s,
                    KeyDerivation {
                        account: *an,
                        sub: *sub,
                        kix,
                        tweak,
                        csv,
                    },
                )
            })
        })
    }

    pub fn add_account(&mut self, account: Account) {
        self.accounts.insert(
            (account.account_number, account.sub_account_number),
            account,
        );
    }

    pub fn sign<R>(
        &self,
        transaction: &mut Transaction,
        hash_type: SigHashType,
        resolver: &R,
        unlocker: &mut Unlocker,
    ) -> Result<usize, Error>
    where
        R: Fn(&OutPoint) -> Option<TxOut>,
    {
        let mut n_signatures = 0;
        for (_, a) in self.accounts.iter() {
            n_signatures += a.sign(transaction, hash_type, resolver, unlocker)?;
        }
        Ok(n_signatures)
    }
}

/// calculator of private keys
pub struct Unlocker {
    master_private: ExtendedPrivKey,
    network: Network,
    context: Arc<SecpContext>,
    cached: HashMap<
        AccountAddressType,
        (
            ExtendedPrivKey,
            HashMap<
                u32,
                (
                    ExtendedPrivKey,
                    HashMap<u32, (ExtendedPrivKey, HashMap<u32, ExtendedPrivKey>)>,
                ),
            >,
        ),
    >,
}

impl Unlocker {
    /// decrypt encrypted seed of a master account
    /// check result if master_public is provided
    pub fn new(
        encrypted: &[u8],
        passphrase: &str,
        network: Network,
        master_public: Option<&ExtendedPubKey>,
    ) -> Result<Unlocker, Error> {
        let seed = Seed::decrypt(encrypted, passphrase)?;
        let context = Arc::new(SecpContext::new());
        let master_private = context.master_private_key(network, &seed)?;
        if let Some(master_public) = master_public {
            if network != master_public.network {
                return Err(Error::Network);
            }
            if context.extended_public_from_private(&master_private) != *master_public {
                return Err(Error::Passphrase);
            }
        }
        Ok(Unlocker {
            master_private,
            network,
            context,
            cached: HashMap::new(),
        })
    }

    pub fn new_for_master(master: &MasterAccount, passphrase: &str) -> Result<Unlocker, Error> {
        Self::new(
            master.encrypted(),
            passphrase,
            master.master_public.network,
            Some(&master.master_public),
        )
    }

    pub fn master_private(&self) -> &ExtendedPrivKey {
        &self.master_private
    }

    pub fn sub_account_key(
        &mut self,
        address_type: AccountAddressType,
        account: u32,
        sub_account: u32,
    ) -> Result<ExtendedPrivKey, Error> {
        let by_purpose = self.cached.entry(address_type).or_insert((
            self.context.private_child(
                &self.master_private,
                ChildNumber::Hardened {
                    index: address_type.as_u32(),
                },
            )?,
            HashMap::new(),
        ));
        let coin_type = match self.network {
            Network::Bitcoin => 0,
            Network::Testnet => 1,
            Network::Regtest => 1,
        };
        let by_coin_type = by_purpose.1.entry(coin_type).or_insert((
            self.context
                .private_child(&by_purpose.0, ChildNumber::Hardened { index: coin_type })?,
            HashMap::new(),
        ));
        let by_account = by_coin_type.1.entry(account).or_insert((
            self.context
                .private_child(&by_coin_type.0, ChildNumber::Hardened { index: account })?,
            HashMap::new(),
        ));
        Ok(self
            .context
            .private_child(&by_account.0, ChildNumber::Normal { index: sub_account })?)
    }

    pub fn unlock(
        &mut self,
        address_type: AccountAddressType,
        account: u32,
        sub_account: u32,
        index: u32,
        tweak: Option<Vec<u8>>,
    ) -> Result<PrivateKey, Error> {
        let sub_account_key = self.sub_account_key(address_type, account, sub_account)?;
        let mut key = self
            .context
            .private_child(&sub_account_key, ChildNumber::Normal { index })?
            .private_key;
        if let Some(tweak) = tweak {
            self.context.tweak_add(&mut key, tweak.as_slice())?;
        }
        Ok(key)
    }

    pub fn context(&self) -> Arc<SecpContext> {
        self.context.clone()
    }
}

/// Key derivation detail information
/// coordinates of a key as defined in BIP32 and BIP44
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyDerivation {
    /// m / purpose' / coin_type' / account' / sub / kix
    pub account: u32,
    /// m / purpose' / coin_type' / account' / sub / kix
    pub sub: u32,
    /// m / purpose' / coin_type' / account' / sub / kix
    pub kix: u32,
    /// optional additive tweak to private key
    pub tweak: Option<Vec<u8>>,
    /// optional number of blocks this can not be spent after confirmation (OP_CSV)
    pub csv: Option<u16>,
}

/// Address type an account is using
#[derive(Copy, Clone, Hash, Eq, PartialEq)]
pub enum AccountAddressType {
    /// legacy pay to public key hash (BIP44)
    P2PKH,
    /// transitional segwit pay to public key hash in legacy format (BIP49)
    P2SHWPKH,
    /// native segwit pay to public key hash in bech format (BIP84)
    P2WPKH,
    /// native segwit pay to script
    /// do not use 44, 49 or 84 for this parameter, to avoid confusion with above types
    /// Only supports scripts that can be spent with following witness:
    /// <signature> <scriptCode>
    P2WSH(u32),
}

impl AccountAddressType {
    pub fn as_u32(&self) -> u32 {
        match self {
            AccountAddressType::P2PKH => 44,
            AccountAddressType::P2SHWPKH => 49,
            AccountAddressType::P2WPKH => 84,
            AccountAddressType::P2WSH(n) => *n,
        }
    }

    pub fn from_u32(n: u32) -> AccountAddressType {
        match n {
            44 => AccountAddressType::P2PKH,
            49 => AccountAddressType::P2SHWPKH,
            84 => AccountAddressType::P2WPKH,
            n => AccountAddressType::P2WSH(n),
        }
    }
}

pub struct Account {
    address_type: AccountAddressType,
    account_number: u32,
    sub_account_number: u32,
    context: Arc<SecpContext>,
    master_public: ExtendedPubKey,
    instantiated: Vec<InstantiatedKey>,
    next: u32,
    look_ahead: u32,
    network: Network,
}

impl Account {
    pub fn new(
        unlocker: &mut Unlocker,
        address_type: AccountAddressType,
        account_number: u32,
        sub_account_number: u32,
        look_ahead: u32,
    ) -> Result<Account, Error> {
        let context = Arc::new(SecpContext::new());
        let master_private =
            unlocker.sub_account_key(address_type, account_number, sub_account_number)?;
        let pubic_key = context.extended_public_from_private(&master_private);
        let mut sub = Account {
            address_type,
            account_number,
            sub_account_number,
            context,
            master_public: pubic_key,
            instantiated: Vec::new(),
            next: 0,
            look_ahead,
            network: pubic_key.network,
        };
        sub.do_look_ahead(None)?;
        Ok(sub)
    }

    pub fn new_from_storage(
        address_type: AccountAddressType,
        account_number: u32,
        sub_account_number: u32,
        master_public: ExtendedPubKey,
        instantiated: Vec<InstantiatedKey>,
        next: u32,
        look_ahead: u32,
        network: Network,
    ) -> Account {
        let context = Arc::new(SecpContext::new());
        Account {
            address_type,
            account_number,
            sub_account_number,
            context,
            master_public,
            instantiated,
            next,
            look_ahead,
            network,
        }
    }

    pub fn address_type(&self) -> AccountAddressType {
        self.address_type
    }

    pub fn account_number(&self) -> u32 {
        self.account_number
    }

    pub fn sub_account_number(&self) -> u32 {
        self.sub_account_number
    }

    pub fn master_public(&self) -> &ExtendedPubKey {
        &self.master_public
    }

    pub fn next(&self) -> u32 {
        self.next
    }

    pub fn look_ahead(&self) -> u32 {
        self.look_ahead
    }

    pub fn network(&self) -> Network {
        self.network
    }

    pub fn instantiated(&self) -> &Vec<InstantiatedKey> {
        &self.instantiated
    }

    /// look ahead from last seen
    pub fn do_look_ahead(&mut self, seen: Option<u32>) -> Result<Vec<(u32, Script)>, Error> {
        use std::cmp::max;

        if let Some(seen) = seen {
            self.next = max(self.next, seen + 1);
        }

        let seen = seen.unwrap_or(0);
        let have = self.instantiated.len() as u32;
        let need = max(seen + self.look_ahead, have) - have;
        let mut new = Vec::new();
        for i in 0..need {
            new.push((
                have + i,
                self.instantiate_more()?.address.script_pubkey().clone(),
            ));
        }
        Ok(new)
    }

    fn instantiate_more(&mut self) -> Result<&InstantiatedKey, Error> {
        let kix = self.instantiated.len() as u32;

        let scripter = |public: &PublicKey, _| match self.address_type {
            AccountAddressType::P2SHWPKH => Builder::new()
                .push_opcode(all::OP_DUP)
                .push_opcode(all::OP_HASH160)
                .push_slice(&hash160::Hash::hash(public.to_bytes().as_slice())[..])
                .push_opcode(all::OP_EQUALVERIFY)
                .push_opcode(all::OP_CHECKSIG)
                .into_script(),
            AccountAddressType::P2WPKH => Builder::new()
                .push_opcode(all::OP_DUP)
                .push_opcode(all::OP_HASH160)
                .push_slice(&hash160::Hash::hash(public.to_bytes().as_slice())[..])
                .push_opcode(all::OP_EQUALVERIFY)
                .push_opcode(all::OP_CHECKSIG)
                .into_script(),
            _ => Script::new(),
        };
        let instantiated = InstantiatedKey::new(
            self.address_type,
            self.network,
            &self.master_public,
            None,
            kix,
            scripter,
            None,
            self.context.clone(),
        )?;

        let len = self.instantiated.len();
        self.instantiated.push(instantiated);
        Ok(&self.instantiated[len])
    }

    /// create a new key
    pub fn next_key(&mut self) -> Result<&InstantiatedKey, Error> {
        match self.address_type {
            AccountAddressType::P2WSH(_) => {
                return Err(Error::Unsupported(
                    "next_key can not be used for P2WSH accounts",
                ))
            }
            _ => {}
        }
        self.instantiate_more()?;
        let key = &self.instantiated[self.next as usize];
        self.next += 1;
        Ok(&key)
    }

    pub fn compute_base_public_key(&self, kix: u32) -> Result<PublicKey, Error> {
        Ok(self
            .context
            .public_child(&self.master_public, ChildNumber::Normal { index: kix })?
            .public_key)
    }

    /// get a previously instantiated key
    pub fn get_key(&self, kix: u32) -> Option<&InstantiatedKey> {
        self.instantiated.get(kix as usize)
    }

    pub fn add_script_key<W>(
        &mut self,
        scripter: W,
        tweak: Option<&[u8]>,
        csv: Option<u16>,
    ) -> Result<u32, Error>
    where
        W: FnOnce(&PublicKey, Option<u16>) -> Script,
    {
        match self.address_type {
            AccountAddressType::P2WSH(_) => {}
            _ => {
                return Err(Error::Unsupported(
                    "add_script_key can only be used for P2WSH accounts",
                ))
            }
        }
        let kix = self.instantiated.len() as u32;
        let instantiated = InstantiatedKey::new(
            self.address_type,
            self.network,
            &self.master_public,
            tweak,
            kix,
            scripter,
            csv,
            self.context.clone(),
        )?;
        self.instantiated.push(instantiated);
        Ok(kix)
    }

    pub fn used(&self) -> usize {
        self.next as usize
    }

    // get all pubkey scripts of this account
    pub fn get_scripts<'a>(
        &'a self,
    ) -> impl Iterator<Item = (u32, Script, Option<Vec<u8>>, Option<u16>)> + 'a {
        self.instantiated.iter().enumerate().map(|(kix, i)| {
            (
                kix as u32,
                i.address.script_pubkey().clone(),
                i.tweak.clone(),
                i.csv.clone(),
            )
        })
    }

    /// sign a transaction with keys in this account works for types except P2WSH
    pub fn sign<R>(
        &self,
        transaction: &mut Transaction,
        hash_type: SigHashType,
        resolver: R,
        unlocker: &mut Unlocker,
    ) -> Result<usize, Error>
    where
        R: Fn(&OutPoint) -> Option<TxOut>,
    {
        let mut signed = 0;
        let txclone = transaction.clone();
        let mut bip143hasher: Option<bip143::SighashComponents> = None;
        for (ix, input) in transaction.input.iter_mut().enumerate() {
            if let Some(spend) = resolver(&input.previous_output) {
                if let Some((kix, instantiated)) = self
                    .instantiated
                    .iter()
                    .enumerate()
                    .find(|(_, i)| i.address.script_pubkey() == spend.script_pubkey)
                {
                    let pk = unlocker.unlock(
                        self.address_type,
                        self.account_number,
                        self.sub_account_number,
                        kix as u32,
                        instantiated.tweak.clone(),
                    )?;
                    match self.address_type {
                        AccountAddressType::P2PKH => {
                            let sighash = txclone.signature_hash(
                                ix,
                                &instantiated.address.script_pubkey(),
                                hash_type.as_u32(),
                            );
                            let signature = self.context.sign(&sighash[..], &pk)?.serialize_der();
                            let mut with_hashtype = signature.to_vec();
                            with_hashtype.push(hash_type.as_u32() as u8);
                            input.script_sig = Builder::new()
                                .push_slice(with_hashtype.as_slice())
                                .push_slice(instantiated.public.to_bytes().as_slice())
                                .into_script();
                            input.witness.clear();
                            signed += 1;
                        }
                        AccountAddressType::P2WPKH => {
                            if hash_type.as_u32() & SigHashType::All.as_u32() == 0 {
                                return Err(Error::Unsupported("can only sig all inputs for now"));
                            }
                            input.script_sig = Script::new();
                            let hasher =
                                bip143hasher.unwrap_or(bip143::SighashComponents::new(&txclone));
                            let sighash = hasher.sighash_all(
                                &txclone.input[ix],
                                &instantiated.script_code,
                                spend.value,
                            );
                            bip143hasher = Some(hasher);
                            let signature = self.context.sign(&sighash[..], &pk)?.serialize_der();
                            let mut with_hashtype = signature.to_vec();
                            with_hashtype.push(hash_type.as_u32() as u8);
                            input.witness.clear();
                            input.witness.push(with_hashtype);
                            input.witness.push(instantiated.public.to_bytes());
                            signed += 1;
                        }
                        AccountAddressType::P2SHWPKH => {
                            if hash_type.as_u32() & SigHashType::All.as_u32() == 0 {
                                return Err(Error::Unsupported("can only sig all inputs for now"));
                            }
                            input.script_sig = Builder::new()
                                .push_slice(
                                    &Builder::new()
                                        .push_int(0)
                                        .push_slice(
                                            &hash160::Hash::hash(
                                                instantiated.public.to_bytes().as_slice(),
                                            )[..],
                                        )
                                        .into_script()[..],
                                )
                                .into_script();
                            let hasher =
                                bip143hasher.unwrap_or(bip143::SighashComponents::new(&txclone));
                            let sighash = hasher.sighash_all(
                                &txclone.input[ix],
                                &instantiated.script_code,
                                spend.value,
                            );
                            bip143hasher = Some(hasher);
                            let signature = self.context.sign(&sighash[..], &pk)?.serialize_der();
                            let mut with_hashtype = signature.to_vec();
                            with_hashtype.push(hash_type.as_u32() as u8);
                            input.witness.clear();
                            input.witness.push(with_hashtype);
                            input.witness.push(instantiated.public.to_bytes());
                            signed += 1;
                        }
                        AccountAddressType::P2WSH(_) => {
                            if hash_type.as_u32() & SigHashType::All.as_u32() == 0 {
                                return Err(Error::Unsupported("can only sig all inputs for now"));
                            }
                            input.script_sig = Script::new();
                            let hasher =
                                bip143hasher.unwrap_or(bip143::SighashComponents::new(&txclone));
                            let sighash = hasher.sighash_all(
                                &txclone.input[ix],
                                &instantiated.script_code,
                                spend.value,
                            );
                            bip143hasher = Some(hasher);
                            let signature = self.context.sign(&sighash[..], &pk)?.serialize_der();
                            let mut with_hashtype = signature.to_vec();
                            with_hashtype.push(hash_type.as_u32() as u8);
                            input.witness.clear();
                            input.witness.push(with_hashtype);
                            input.witness.push(instantiated.script_code.to_bytes());
                            signed += 1;
                        }
                    }
                }
            }
        }
        Ok(signed)
    }
}

/// instantiated key of an account
#[derive(Clone, Serialize, Deserialize)]
pub struct InstantiatedKey {
    pub public: PublicKey,
    pub script_code: Script,
    pub address: Address,
    pub tweak: Option<Vec<u8>>,
    pub csv: Option<u16>,
}

impl InstantiatedKey {
    pub fn new<W>(
        address_type: AccountAddressType,
        network: Network,
        master: &ExtendedPubKey,
        tweak: Option<&[u8]>,
        kix: u32,
        scripter: W,
        csv: Option<u16>,
        context: Arc<SecpContext>,
    ) -> Result<InstantiatedKey, Error>
    where
        W: FnOnce(&PublicKey, Option<u16>) -> Script,
    {
        let mut public = context
            .public_child(master, ChildNumber::Normal { index: kix })?
            .public_key;
        if let Some(tweak) = tweak {
            context.tweak_exp_add(&mut public, tweak)?;
        }
        let script_code = scripter(&public, csv);
        let address = match address_type {
            AccountAddressType::P2PKH => Address::p2pkh(&public, network),
            AccountAddressType::P2SHWPKH => Address::p2shwpkh(&public, network),
            AccountAddressType::P2WPKH => Address::p2wpkh(&public, network),
            AccountAddressType::P2WSH(_) => Address::p2wsh(&script_code, network),
        };
        Ok(InstantiatedKey {
            public,
            script_code,
            address,
            tweak: tweak.map(|t| t.to_vec()),
            csv,
        })
    }
}

/// seed of the master key
#[derive(Clone, Eq, PartialEq, Debug)]
pub struct Seed(pub Vec<u8>);

impl Seed {
    /// encrypt seed
    /// encryption algorithm: AES256(Sha256(passphrase), ECB, PKCS padding
    pub fn encrypt(&self, passphrase: &str) -> Result<Vec<u8>, Error> {
        let mut key = [0u8; 32];
        let mut sha2 = Sha256::new();
        sha2.input(passphrase.as_bytes());
        sha2.result(&mut key);

        let mut encryptor =
            aes::ecb_encryptor(aes::KeySize::KeySize256, &key, blockmodes::PkcsPadding {});
        let mut encrypted = Vec::new();
        let mut reader = buffer::RefReadBuffer::new(self.0.as_slice());
        let mut buffer = [0u8; 1024];
        let mut writer = buffer::RefWriteBuffer::new(&mut buffer);
        loop {
            let result = encryptor.encrypt(&mut reader, &mut writer, true)?;
            encrypted.extend(
                writer
                    .take_read_buffer()
                    .take_remaining()
                    .iter()
                    .map(|i| *i),
            );
            match result {
                BufferResult::BufferUnderflow => break,
                BufferResult::BufferOverflow => {}
            }
        }
        Ok(encrypted)
    }

    /// decrypt seed
    /// decryption algorithm: AES256(Sha256(passphrase), ECB, PKCS padding
    pub fn decrypt(encrypted: &[u8], passphrase: &str) -> Result<Seed, Error> {
        let mut key = [0u8; 32];
        let mut sha2 = Sha256::new();
        sha2.input(passphrase.as_bytes());
        sha2.result(&mut key);

        let mut decrypted = Vec::new();
        let mut reader = buffer::RefReadBuffer::new(encrypted);
        let mut buffer = [0u8; 1024];
        let mut writer = buffer::RefWriteBuffer::new(&mut buffer);
        let mut decryptor =
            aes::ecb_decryptor(aes::KeySize::KeySize256, &key, blockmodes::PkcsPadding {});
        loop {
            let result = decryptor.decrypt(&mut reader, &mut writer, true)?;
            decrypted.extend(
                writer
                    .take_read_buffer()
                    .take_remaining()
                    .iter()
                    .map(|i| *i),
            );
            match result {
                BufferResult::BufferUnderflow => break,
                BufferResult::BufferOverflow => {}
            }
        }

        Ok(Seed(decrypted))
    }
}

#[cfg(test)]
mod test {
    use std::collections::HashMap;
    use std::fs::File;
    use std::io::Read;
    use std::path::PathBuf;

    use bitcoin::blockdata::opcodes::all;
    use bitcoin::blockdata::script::Builder;
    use bitcoin::blockdata::transaction::{OutPoint, TxIn, TxOut};
    use bitcoin::network::constants::Network;
    use bitcoin::util::{address::AddressType, bip32::ChildNumber, psbt::serialize::Deserialize};
    use hex::decode;
    use rand::Rng;
    use serde_json::Value;

    use super::*;

    const PASSPHRASE: &str = "correct horse battery staple";
    const RBF: u32 = 0xffffffff - 2;

    #[test]
    fn seed_encrypt_decrypt() {
        let mut secret = [0u8; 32];
        thread_rng().fill(&mut secret);
        let seed = Seed(secret.to_vec());
        assert_eq!(
            Seed::decrypt(seed.encrypt("whatever").unwrap().as_slice(), "whatever").unwrap(),
            seed
        );
    }

    #[test]
    fn test_pkh() {
        let mut master =
            MasterAccount::new(MasterKeyEntropy::Sufficient, Network::Bitcoin, PASSPHRASE).unwrap();
        let mut unlocker = Unlocker::new_for_master(&master, PASSPHRASE).unwrap();
        let account = Account::new(&mut unlocker, AccountAddressType::P2PKH, 0, 0, 10).unwrap();
        master.add_account(account);
        let account = master.get_mut((0, 0)).unwrap();
        let i = account.next_key().unwrap();
        let source = i.address.clone();
        let target = i.address.clone();
        let input_transaction = Transaction {
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: bitcoin::Txid::default(),
                    vout: 0,
                },
                sequence: RBF,
                witness: Vec::new(),
                script_sig: Script::new(),
            }],
            output: vec![TxOut {
                script_pubkey: source.script_pubkey(),
                value: 5000000000,
            }],
            lock_time: 0,
            version: 2,
        };
        let txid = input_transaction.txid();

        let mut spending_transaction = Transaction {
            input: vec![TxIn {
                previous_output: OutPoint { txid, vout: 0 },
                sequence: RBF,
                witness: Vec::new(),
                script_sig: Script::new(),
            }],
            output: vec![TxOut {
                script_pubkey: target.script_pubkey(),
                value: 5000000000,
            }],
            lock_time: 0,
            version: 2,
        };

        let mut spent = HashMap::new();
        spent.insert(input_transaction.txid(), input_transaction.clone());

        assert_eq!(
            master
                .sign(
                    &mut spending_transaction,
                    SigHashType::All,
                    &(|_| Some(input_transaction.output[0].clone())),
                    &mut unlocker
                )
                .unwrap(),
            1
        );

        spending_transaction
            .verify(|point| {
                spent
                    .get(&point.txid)
                    .and_then(|t| t.output.get(point.vout as usize).cloned())
            })
            .unwrap();
    }

    #[test]
    fn test_wpkh() {
        let mut master =
            MasterAccount::new(MasterKeyEntropy::Sufficient, Network::Bitcoin, PASSPHRASE).unwrap();
        let mut unlocker = Unlocker::new_for_master(&master, PASSPHRASE).unwrap();
        let account = Account::new(&mut unlocker, AccountAddressType::P2WPKH, 0, 0, 10).unwrap();
        master.add_account(account);
        let account = master.get_mut((0, 0)).unwrap();
        let i = account.next_key().unwrap();
        let source = i.address.clone();
        let target = i.address.clone();

        let input_transaction = Transaction {
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: bitcoin::Txid::default(),
                    vout: 0,
                },
                sequence: RBF,
                witness: Vec::new(),
                script_sig: Script::new(),
            }],
            output: vec![TxOut {
                script_pubkey: source.script_pubkey(),
                value: 5000000000,
            }],
            lock_time: 0,
            version: 2,
        };
        let txid = input_transaction.txid();

        let mut spending_transaction = Transaction {
            input: vec![TxIn {
                previous_output: OutPoint { txid, vout: 0 },
                sequence: RBF,
                witness: Vec::new(),
                script_sig: Script::new(),
            }],
            output: vec![TxOut {
                script_pubkey: target.script_pubkey(),
                value: 5000000000,
            }],
            lock_time: 0,
            version: 2,
        };

        let mut spent = HashMap::new();
        spent.insert(txid, input_transaction.clone());

        assert_eq!(
            master
                .sign(
                    &mut spending_transaction,
                    SigHashType::All,
                    &(|_| Some(input_transaction.output[0].clone())),
                    &mut unlocker
                )
                .unwrap(),
            1
        );

        spending_transaction
            .verify(|point| {
                spent
                    .get(&point.txid)
                    .and_then(|t| t.output.get(point.vout as usize).cloned())
            })
            .unwrap();
    }

    #[test]
    fn test_shwpkh() {
        let mut master =
            MasterAccount::new(MasterKeyEntropy::Sufficient, Network::Bitcoin, PASSPHRASE).unwrap();
        let mut unlocker = Unlocker::new_for_master(&master, PASSPHRASE).unwrap();
        let account = Account::new(&mut unlocker, AccountAddressType::P2SHWPKH, 0, 0, 10).unwrap();
        master.add_account(account);
        let account = master.get_mut((0, 0)).unwrap();
        let i = account.next_key().unwrap();
        let source = i.address.clone();
        let target = i.address.clone();

        let input_transaction = Transaction {
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: bitcoin::Txid::default(),
                    vout: 0,
                },
                sequence: RBF,
                witness: Vec::new(),
                script_sig: Script::new(),
            }],
            output: vec![TxOut {
                script_pubkey: source.script_pubkey(),
                value: 5000000000,
            }],
            lock_time: 0,
            version: 2,
        };

        let txid = input_transaction.txid();

        let mut spending_transaction = Transaction {
            input: vec![TxIn {
                previous_output: OutPoint { txid, vout: 0 },
                sequence: RBF,
                witness: Vec::new(),
                script_sig: Script::new(),
            }],
            output: vec![TxOut {
                script_pubkey: target.script_pubkey(),
                value: 5000000000,
            }],
            lock_time: 0,
            version: 2,
        };

        let mut spent = HashMap::new();
        spent.insert(txid, input_transaction.clone());

        assert_eq!(
            master
                .sign(
                    &mut spending_transaction,
                    SigHashType::All,
                    &(|_| Some(input_transaction.output[0].clone())),
                    &mut unlocker
                )
                .unwrap(),
            1
        );

        spending_transaction
            .verify(|point| {
                spent
                    .get(&point.txid)
                    .and_then(|t| t.output.get(point.vout as usize).cloned())
            })
            .unwrap();
    }

    #[test]
    fn test_wsh() {
        let mut master =
            MasterAccount::new(MasterKeyEntropy::Sufficient, Network::Bitcoin, PASSPHRASE).unwrap();
        let mut unlocker = Unlocker::new_for_master(&master, PASSPHRASE).unwrap();
        let account =
            Account::new(&mut unlocker, AccountAddressType::P2WSH(4711), 0, 0, 0).unwrap();
        master.add_account(account);

        {
            let account = master.get_mut((0, 0)).unwrap();
            let scripter = |pk: &PublicKey, _| {
                Builder::new()
                    .push_slice(pk.to_bytes().as_slice())
                    .push_opcode(all::OP_CHECKSIG)
                    .into_script()
            };
            account
                .add_script_key(scripter, Some(&[0x01; 32]), None)
                .unwrap();
        }

        let account = master.get((0, 0)).unwrap();
        let source = account.get_key(0).unwrap().address.clone();
        let target = account.get_key(0).unwrap().address.clone();
        let input_transaction = Transaction {
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: bitcoin::Txid::default(),
                    vout: 0,
                },
                sequence: RBF,
                witness: Vec::new(),
                script_sig: Script::new(),
            }],
            output: vec![TxOut {
                script_pubkey: source.script_pubkey(),
                value: 5000000000,
            }],
            lock_time: 0,
            version: 2,
        };
        let txid = input_transaction.txid();

        let mut spending_transaction = Transaction {
            input: vec![TxIn {
                previous_output: OutPoint { txid, vout: 0 },
                sequence: RBF,
                witness: Vec::new(),
                script_sig: Script::new(),
            }],
            output: vec![TxOut {
                script_pubkey: target.script_pubkey(),
                value: 5000000000,
            }],
            lock_time: 0,
            version: 2,
        };

        let mut spent = HashMap::new();
        spent.insert(input_transaction.txid(), input_transaction.clone());

        assert_eq!(
            master
                .sign(
                    &mut spending_transaction,
                    SigHashType::All,
                    &(|_| Some(input_transaction.output[0].clone())),
                    &mut unlocker
                )
                .unwrap(),
            1
        );

        spending_transaction
            .verify(|point| {
                spent
                    .get(&point.txid)
                    .and_then(|t| t.output.get(point.vout as usize).cloned())
            })
            .unwrap();
    }

    const CSV: u16 = 10;

    #[test]
    fn test_wsh_csv() {
        let mut master =
            MasterAccount::new(MasterKeyEntropy::Sufficient, Network::Bitcoin, PASSPHRASE).unwrap();
        let mut unlocker = Unlocker::new_for_master(&master, PASSPHRASE).unwrap();
        let account =
            Account::new(&mut unlocker, AccountAddressType::P2WSH(4711), 0, 0, 0).unwrap();
        master.add_account(account);

        {
            let account = master.get_mut((0, 0)).unwrap();
            let scripter = |pk: &PublicKey, csv: Option<u16>| {
                Builder::new()
                    .push_int(csv.unwrap() as i64)
                    .push_opcode(all::OP_CSV)
                    .push_opcode(all::OP_DROP)
                    .push_slice(pk.to_bytes().as_slice())
                    .push_opcode(all::OP_CHECKSIG)
                    .into_script()
            };
            account
                .add_script_key(scripter, Some(&[0x01; 32]), Some(CSV))
                .unwrap();
        }

        let account = master.get((0, 0)).unwrap();
        let source = account.get_key(0).unwrap().address.clone();
        let target = account.get_key(0).unwrap().address.clone();
        let input_transaction = Transaction {
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: bitcoin::Txid::default(),
                    vout: 0,
                },
                sequence: RBF,
                witness: Vec::new(),
                script_sig: Script::new(),
            }],
            output: vec![TxOut {
                script_pubkey: source.script_pubkey(),
                value: 5000000000,
            }],
            lock_time: 0,
            version: 2,
        };
        let txid = input_transaction.txid();

        let mut spending_transaction = Transaction {
            input: vec![TxIn {
                previous_output: OutPoint { txid, vout: 0 },
                sequence: CSV as u32,
                witness: Vec::new(),
                script_sig: Script::new(),
            }],
            output: vec![TxOut {
                script_pubkey: target.script_pubkey(),
                value: 5000000000,
            }],
            lock_time: 0,
            version: 2,
        };

        let mut spent = HashMap::new();
        spent.insert(input_transaction.txid(), input_transaction.clone());

        assert_eq!(
            master
                .sign(
                    &mut spending_transaction,
                    SigHashType::All,
                    &(|_| Some(input_transaction.output[0].clone())),
                    &mut unlocker
                )
                .unwrap(),
            1
        );

        spending_transaction
            .verify(|point| {
                spent
                    .get(&point.txid)
                    .and_then(|t| t.output.get(point.vout as usize).cloned())
            })
            .unwrap();

        let mut spending_transaction = Transaction {
            input: vec![TxIn {
                previous_output: OutPoint { txid, vout: 0 },
                sequence: (CSV - 1) as u32, // this one should not be able to spend
                witness: Vec::new(),
                script_sig: Script::new(),
            }],
            output: vec![TxOut {
                script_pubkey: target.script_pubkey(),
                value: 5000000000,
            }],
            lock_time: 0,
            version: 2,
        };

        assert_eq!(
            master
                .sign(
                    &mut spending_transaction,
                    SigHashType::All,
                    &(|_| Some(input_transaction.output[0].clone())),
                    &mut unlocker
                )
                .unwrap(),
            1
        );

        assert!(spending_transaction
            .verify(|point| spent
                .get(&point.txid)
                .and_then(|t| t.output.get(point.vout as usize).cloned()))
            .is_err());
    }

    #[test]
    fn crosscheck_with_hardware_wallet() {
        let words = "announce damage viable ticket engage curious yellow ten clock finish burden orient faculty rigid smile host offer affair suffer slogan mercy another switch park";
        let mnemonic = Mnemonic::from_str(words).unwrap();
        let master =
            MasterAccount::from_mnemonic(&mnemonic, 0, Network::Bitcoin, PASSPHRASE, None).unwrap();
        let mut unlocker = Unlocker::new_for_master(&master, PASSPHRASE).unwrap();
        let account = Account::new(&mut unlocker, AccountAddressType::P2SHWPKH, 0, 0, 10).unwrap();
        // this should be address of m/49'/0'/0'/0/0
        assert_eq!(
            account.get_key(0).unwrap().address.to_string(),
            "3L8V8mDQVUySGwCqiB2x8fdRRMGWyyF4YP"
        );
        let account = Account::new(&mut unlocker, AccountAddressType::P2WPKH, 0, 0, 10).unwrap();
        // this should be address of m/84'/0'/0'/0/0
        assert_eq!(
            account.get_key(0).unwrap().address.to_string(),
            "bc1qlz2h9scgalmqj43d36f58dcxrrl7udu999gcp2"
        );
    }

    #[test]
    fn bip32_tests() {
        let context = super::SecpContext::new();

        let mut d = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        d.push("tests/BIP32.json");
        let mut file = File::open(d).unwrap();
        let mut data = String::new();
        file.read_to_string(&mut data).unwrap();
        let json: Value = serde_json::from_str(&data).unwrap();
        let tests = json.as_array().unwrap();
        for test in tests {
            let seed = Seed(decode(test["seed"].as_str().unwrap()).unwrap());
            let master_private = context.master_private_key(Network::Bitcoin, &seed).unwrap();
            assert_eq!(
                test["private"].as_str().unwrap(),
                master_private.to_string()
            );
            assert_eq!(
                test["public"].as_str().unwrap(),
                context
                    .extended_public_from_private(&master_private)
                    .to_string()
            );
            for d in test["derived"].as_array().unwrap() {
                let mut key = master_private.clone();
                for l in d["locator"].as_array().unwrap() {
                    let sequence = l["sequence"].as_u64().unwrap();
                    let private = l["private"].as_bool().unwrap();
                    let child = if private {
                        ChildNumber::Hardened {
                            index: sequence as u32,
                        }
                    } else {
                        ChildNumber::Normal {
                            index: sequence as u32,
                        }
                    };
                    key = context.private_child(&key.clone(), child).unwrap();
                }
                assert_eq!(d["private"].as_str().unwrap(), key.to_string());
                assert_eq!(
                    d["public"].as_str().unwrap(),
                    context.extended_public_from_private(&key).to_string()
                );
            }
        }
    }

    #[test]
    fn witness_multisig_tests() {
        let mnemonic = Mnemonic::from_str(
            "upgrade chronic stay north wedding dawn suffer silver behave bitter differ tourist",
        )
        .unwrap();
        let mut master =
            MasterAccount::from_mnemonic(&mnemonic, 0, Network::Bitcoin, "", None).unwrap();
        let mut unlocker = Unlocker::new_for_master(&master, "").unwrap();
        let account =
            Account::new(&mut unlocker, AccountAddressType::P2WSH(4711), 0, 0, 0).unwrap();
        master.add_account(account);
        {
            let account = master.get_mut((0, 0)).unwrap();
            const CSV: u16 = 0; // 0 blocks relative lock
            let scripter = |_pk: &PublicKey, _csv: Option<u16>| {
                Builder::new()
                    .push_int(3)
                    .push_slice(
                        &hex::decode(
                            "020e1e9e13a2c6178c0e3cba7f6a6bb18c5363e74f35b7572a620ae72b90685680",
                        )
                        .unwrap(),
                    )
                    .push_slice(
                        &hex::decode(
                            "02264f891c836d99272164b962e511a017e1a1b835d5f30bdc8691c42634e59351",
                        )
                        .unwrap(),
                    )
                    .push_slice(
                        &hex::decode(
                            "02e0bbf72ffa17d4c0654515c8541df02958c3ef82bc09a825c1c55aa8a892afbb",
                        )
                        .unwrap(),
                    )
                    .push_slice(
                        &hex::decode(
                            "031f26b9d6ac186f65ef0a1b093371e99e8e17aa6d3636f7634920479df5ae5a77",
                        )
                        .unwrap(),
                    )
                    .push_slice(
                        &hex::decode(
                            "031f5bd798116f9701794da6904ca0d26d06e24ba920785e48671c91c35dad756e",
                        )
                        .unwrap(),
                    )
                    .push_slice(
                        &hex::decode(
                            "036eb794bc2b512233ec9fc8cd60e2e4ba31eb1f49796cb380ecd9898147a9ca86",
                        )
                        .unwrap(),
                    )
                    .push_slice(
                        &hex::decode(
                            "03a0e5c6c0212cf6d0c60867b9107d461c5e5790b032f6a5d33b3319332f1d9130",
                        )
                        .unwrap(),
                    )
                    .push_int(7)
                    .push_opcode(all::OP_CHECKMULTISIG)
                    .into_script()
                    .to_v0_p2wsh()
            };
            account
                .add_script_key(scripter, Some(&[0x01; 32]), Some(CSV))
                .unwrap();
        }
        let account = master.get_mut((0, 0)).unwrap();
        let address = Address::p2sh(&account.get_key(0).unwrap().script_code, Network::Bitcoin);
        assert_eq!(address.to_string(), "325g8XuPdyYav4bZK8k4dA62hQpExMQT6C");

        let prevtx = "020000000001011ad267e6f3a56c2ef77cb469eace3bfffbc631e6c3feb888c5cae1d0f5287232000000002322002055c6e17463500c1c6a91790fd812e3234fecd223dcbb5125c059c6ebd812c3f6fdffffff02d007000000000000160014dd049c8c51a34cf81f74869c09f912c3b9acbf86e4a200000000000017a91404490b65f9dc2e67dbb3e5f4d7835d4582fcc4d9870500473044022013813bf372c1004591042f461c80d748007a8bb26fd38a74dc0798372848b62e02206b368e43361de6c1e894e30cf4c12285723a5cc7ead30f2dad7c01381865d2030147304402205d5181d1f67b07e337280b1463a5a7a79b6eb1faccc13b6bc1185e5fe24ce07802201774fb5cf186646686d732d3edc51b25513ecee74e702a6565e25aabe94779b401473044022079875689c9dccbb3717da3dc0a67331d1e80657ac6645e40bc6ef831aa386e8c022040da3befd5ca11decafdf96365a01e92b177c34858842f9988f3754c10db5d6101f15321020e1e9e13a2c6178c0e3cba7f6a6bb18c5363e74f35b7572a620ae72b906856802102264f891c836d99272164b962e511a017e1a1b835d5f30bdc8691c42634e593512102e0bbf72ffa17d4c0654515c8541df02958c3ef82bc09a825c1c55aa8a892afbb21031f26b9d6ac186f65ef0a1b093371e99e8e17aa6d3636f7634920479df5ae5a7721031f5bd798116f9701794da6904ca0d26d06e24ba920785e48671c91c35dad756e21036eb794bc2b512233ec9fc8cd60e2e4ba31eb1f49796cb380ecd9898147a9ca862103a0e5c6c0212cf6d0c60867b9107d461c5e5790b032f6a5d33b3319332f1d913057aee6cf0900";
        let prevtx = hex::decode(prevtx).unwrap();
        let prevtx = Transaction::deserialize(&prevtx).unwrap();
        let tx = "0200000001bbc8d87eba45f99fa950c4cb513f23c07b0941e1f9058a4d1fcb52c98595c2ec0100000000000000000210270000000000001976a914bda5fbf75d67de006aa4768970113ca8a1d0727688acec7700000000000017a91404490b65f9dc2e67dbb3e5f4d7835d4582fcc4d98700000000";
        let tx = hex::decode(tx).unwrap();
        let mut tx = Transaction::deserialize(&tx).unwrap();

        let spend = &prevtx.output[1];
        assert_eq!(spend.value, 41700);

        let addr_spend = Address::from_script(&spend.script_pubkey, Network::Bitcoin).unwrap();
        let addr_account = Address::p2sh(&account.instantiated[0].script_code, Network::Bitcoin);

        assert_eq!(
            addr_spend.to_string(),
            "325g8XuPdyYav4bZK8k4dA62hQpExMQT6C"
        );
        assert_eq!(
            addr_account.to_string(),
            "325g8XuPdyYav4bZK8k4dA62hQpExMQT6C"
        );
        assert_eq!(
            addr_spend.address_type().unwrap(),
            AddressType::P2sh
        );
        assert_eq!(
            addr_account.address_type().unwrap(),
            AddressType::P2sh
        );

        let spend_script = spend.script_pubkey.to_v0_p2wsh();
        let account_script = account.instantiated[0].address.script_pubkey();
        assert_eq!(
            spend_script.is_witness_program(),
            account_script.is_witness_program()
        );
        assert_eq!(
            spend_script.is_v0_p2wsh(),
            account_script.is_v0_p2wsh()
        );
        assert_eq!(
            spend_script.is_p2sh(),
            account_script.is_p2sh()
        );

        // This assert is essentially replicating the comparison in the find function of src/accounts.rs line 652
        // That is where the signing skips at the moment.
        assert_eq!(
            spend_script,
            account_script
        );

        master
            .sign(
                &mut tx,
                SigHashType::All,
                &(|_| Some(prevtx.output[1].clone())),
                &mut unlocker,
            )
            .unwrap();
    }
}
