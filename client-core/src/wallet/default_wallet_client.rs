use bit_vec::BitVec;
use std::collections::BTreeSet;

use parity_scale_codec::Encode;
use secp256k1::schnorrsig::SchnorrSignature;
use secstr::SecUtf8;
#[cfg(not(debug_assertions))]
use zxcvbn::{feedback::Feedback, zxcvbn as estimate_password_strength};

use chain_core::common::{Proof, H256};
use chain_core::init::address::RedeemAddress;
use chain_core::init::coin::Coin;
use chain_core::state::account::StakedStateAddress;
use chain_core::tx::data::address::ExtendedAddr;
use chain_core::tx::data::attribute::TxAttributes;
use chain_core::tx::data::input::{str2txid, TxoPointer};
use chain_core::tx::data::output::TxOut;
use chain_core::tx::data::{Tx, TxId};
use chain_core::tx::witness::tree::RawPubkey;
use chain_core::tx::witness::{TxInWitness, TxWitness};
use chain_core::tx::{TransactionId, TxAux};
use client_common::tendermint::types::{AbciQueryExt, BlockExt, BroadcastTxResponse};
use client_common::tendermint::{Client, UnauthorizedClient};
use client_common::{
    Error, ErrorKind, PrivateKey, PublicKey, Result, ResultExt, SignedTransaction, Storage,
    Transaction, TransactionInfo,
};

use crate::service::*;
use crate::transaction_builder::UnauthorizedWalletTransactionBuilder;
use crate::types::{
    AddressType, BalanceChange, TransactionChange, TransactionPending, WalletBalance, WalletKind,
};
use crate::wallet::syncer_logic::create_transaction_change;
use crate::{
    InputSelectionStrategy, Mnemonic, MultiSigWalletClient, UnspentTransactions, WalletClient,
    WalletTransactionBuilder,
};
use client_common::tendermint::types::Time;

/// Default implementation of `WalletClient` based on `Storage` and `Index`
#[derive(Debug, Default, Clone)]
pub struct DefaultWalletClient<S, C, T>
where
    S: Storage,
    C: Client,
    T: WalletTransactionBuilder,
{
    key_service: KeyService<S>,
    hd_key_service: HdKeyService<S>,
    wallet_service: WalletService<S>,
    wallet_state_service: WalletStateService<S>,
    root_hash_service: RootHashService<S>,
    multi_sig_session_service: MultiSigSessionService<S>,

    tendermint_client: C,
    transaction_builder: T,
}

impl<S, C, T> DefaultWalletClient<S, C, T>
where
    S: Storage,
    C: Client,
    T: WalletTransactionBuilder,
{
    /// Creates a new instance of `DefaultWalletClient`
    pub fn new(storage: S, tendermint_client: C, transaction_builder: T) -> Self {
        Self {
            key_service: KeyService::new(storage.clone()),
            hd_key_service: HdKeyService::new(storage.clone()),
            wallet_service: WalletService::new(storage.clone()),
            wallet_state_service: WalletStateService::new(storage.clone()),
            root_hash_service: RootHashService::new(storage.clone()),
            multi_sig_session_service: MultiSigSessionService::new(storage),
            tendermint_client,
            transaction_builder,
        }
    }
}

impl<S> DefaultWalletClient<S, UnauthorizedClient, UnauthorizedWalletTransactionBuilder>
where
    S: Storage,
{
    /// Creates a new read-only instance of `DefaultWalletClient`
    pub fn new_read_only(storage: S) -> Self {
        Self::new(
            storage,
            UnauthorizedClient,
            UnauthorizedWalletTransactionBuilder,
        )
    }
}

impl<S, C, T> WalletClient for DefaultWalletClient<S, C, T>
where
    S: Storage,
    C: Client,
    T: WalletTransactionBuilder,
{
    #[inline]
    fn wallets(&self) -> Result<Vec<String>> {
        self.wallet_service.names()
    }

    fn new_wallet(
        &self,
        name: &str,
        passphrase: &SecUtf8,
        wallet_kind: WalletKind,
    ) -> Result<Option<Mnemonic>> {
        #[cfg(not(debug_assertions))]
        check_passphrase_strength(name, passphrase)?;

        match wallet_kind {
            WalletKind::Basic => {
                let private_key = PrivateKey::new()?;
                let view_key = PublicKey::from(&private_key);

                self.key_service
                    .add_keypair(&private_key, &view_key, passphrase)?;

                self.wallet_service
                    .create(name, passphrase, view_key)
                    .map(|_| None)
            }
            WalletKind::HD => {
                let mnemonic = Mnemonic::new();

                self.hd_key_service
                    .add_mnemonic(name, &mnemonic, passphrase)?;

                let (public_key, private_key) = self.hd_key_service.generate_keypair(
                    name,
                    passphrase,
                    HDAccountType::Viewkey,
                )?;

                self.key_service
                    .add_keypair(&private_key, &public_key, passphrase)?;

                self.wallet_service.create(name, passphrase, public_key)?;

                Ok(Some(mnemonic))
            }
        }
    }

    fn restore_wallet(&self, name: &str, passphrase: &SecUtf8, mnemonic: &Mnemonic) -> Result<()> {
        #[cfg(not(debug_assertions))]
        check_passphrase_strength(name, passphrase)?;

        self.hd_key_service
            .add_mnemonic(name, mnemonic, passphrase)?;

        let (public_key, private_key) =
            self.hd_key_service
                .generate_keypair(name, passphrase, HDAccountType::Viewkey)?;

        self.key_service
            .add_keypair(&private_key, &public_key, passphrase)?;

        self.wallet_service.create(name, passphrase, public_key)
    }

    fn restore_basic_wallet(
        &self,
        name: &str,
        passphrase: &SecUtf8,
        view_key_priv: &PrivateKey,
    ) -> Result<()> {
        let view_key = PublicKey::from(view_key_priv);
        self.key_service
            .add_keypair(&view_key_priv, &view_key, passphrase)?;
        self.wallet_service.create(name, passphrase, view_key)
    }

    #[inline]
    fn view_key(&self, name: &str, passphrase: &SecUtf8) -> Result<PublicKey> {
        self.wallet_service.view_key(name, passphrase)
    }

    #[inline]
    fn view_key_private(&self, name: &str, passphrase: &SecUtf8) -> Result<PrivateKey> {
        self.key_service
            .private_key(&self.wallet_service.view_key(name, passphrase)?, passphrase)?
            .err_kind(ErrorKind::InvalidInput, || "private view key not found")
    }

    #[inline]
    fn public_keys(&self, name: &str, passphrase: &SecUtf8) -> Result<BTreeSet<PublicKey>> {
        self.wallet_service.public_keys(name, passphrase)
    }

    #[inline]
    fn staking_keys(&self, name: &str, passphrase: &SecUtf8) -> Result<BTreeSet<PublicKey>> {
        self.wallet_service.staking_keys(name, passphrase)
    }

    #[inline]
    fn root_hashes(&self, name: &str, passphrase: &SecUtf8) -> Result<BTreeSet<H256>> {
        self.wallet_service.root_hashes(name, passphrase)
    }

    #[inline]
    fn staking_addresses(
        &self,
        name: &str,
        passphrase: &SecUtf8,
    ) -> Result<BTreeSet<StakedStateAddress>> {
        self.wallet_service.staking_addresses(name, passphrase)
    }

    #[inline]
    fn transfer_addresses(
        &self,
        name: &str,
        passphrase: &SecUtf8,
    ) -> Result<BTreeSet<ExtendedAddr>> {
        self.wallet_service.transfer_addresses(name, passphrase)
    }

    #[inline]
    fn find_staking_key(
        &self,
        name: &str,
        passphrase: &SecUtf8,
        redeem_address: &RedeemAddress,
    ) -> Result<Option<PublicKey>> {
        self.wallet_service
            .find_staking_key(name, passphrase, redeem_address)
    }

    #[inline]
    fn find_root_hash(
        &self,
        name: &str,
        passphrase: &SecUtf8,
        address: &ExtendedAddr,
    ) -> Result<Option<H256>> {
        self.wallet_service
            .find_root_hash(name, passphrase, address)
    }

    #[inline]
    fn private_key(
        &self,
        passphrase: &SecUtf8,
        public_key: &PublicKey,
    ) -> Result<Option<PrivateKey>> {
        self.key_service.private_key(public_key, passphrase)
    }

    fn new_public_key(
        &self,
        name: &str,
        passphrase: &SecUtf8,
        address_type: Option<AddressType>,
    ) -> Result<PublicKey> {
        let (public_key, private_key) = if self.hd_key_service.has_wallet(name)? {
            self.hd_key_service.generate_keypair(
                name,
                passphrase,
                address_type
                    .chain(|| {
                        (
                            ErrorKind::InvalidInput,
                            "Address type is needed when creating address for HD wallet",
                        )
                    })?
                    .into(),
            )?
        } else {
            let private_key = PrivateKey::new()?;
            let public_key = PublicKey::from(&private_key);

            (public_key, private_key)
        };

        self.key_service
            .add_keypair(&private_key, &public_key, passphrase)?;

        self.wallet_service
            .add_public_key(name, passphrase, &public_key)?;

        Ok(public_key)
    }

    fn new_staking_address(&self, name: &str, passphrase: &SecUtf8) -> Result<StakedStateAddress> {
        let (staking_key, private_key) = if self.hd_key_service.has_wallet(name)? {
            self.hd_key_service
                .generate_keypair(name, passphrase, HDAccountType::Staking)?
        } else {
            let private_key = PrivateKey::new()?;
            let public_key = PublicKey::from(&private_key);

            (public_key, private_key)
        };

        self.key_service
            .add_keypair(&private_key, &staking_key, passphrase)?;

        self.wallet_service
            .add_staking_key(name, passphrase, &staking_key)?;

        Ok(StakedStateAddress::BasicRedeem(RedeemAddress::from(
            &staking_key,
        )))
    }

    fn new_transfer_address(&self, name: &str, passphrase: &SecUtf8) -> Result<ExtendedAddr> {
        let (public_key, private_key) = if self.hd_key_service.has_wallet(name)? {
            self.hd_key_service
                .generate_keypair(name, passphrase, HDAccountType::Transfer)?
        } else {
            let private_key = PrivateKey::new()?;
            let public_key = PublicKey::from(&private_key);

            (public_key, private_key)
        };

        self.key_service
            .add_keypair(&private_key, &public_key, passphrase)?;

        self.wallet_service
            .add_public_key(name, passphrase, &public_key)?;

        self.new_multisig_transfer_address(
            name,
            passphrase,
            vec![public_key.clone()],
            public_key,
            1,
        )
    }

    fn new_watch_staking_address(
        &self,
        name: &str,
        passphrase: &SecUtf8,
        public_key: &PublicKey,
    ) -> Result<StakedStateAddress> {
        self.wallet_service
            .add_staking_key(name, passphrase, public_key)?;

        Ok(StakedStateAddress::BasicRedeem(RedeemAddress::from(
            public_key,
        )))
    }

    fn new_watch_transfer_address(
        &self,
        name: &str,
        passphrase: &SecUtf8,
        public_key: &PublicKey,
    ) -> Result<ExtendedAddr> {
        self.new_multisig_transfer_address(
            name,
            passphrase,
            vec![public_key.clone()],
            public_key.clone(),
            1,
        )
    }

    fn new_multisig_transfer_address(
        &self,
        name: &str,
        passphrase: &SecUtf8,
        public_keys: Vec<PublicKey>,
        self_public_key: PublicKey,
        m: usize,
    ) -> Result<ExtendedAddr> {
        if !public_keys.contains(&self_public_key) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Signer public keys does not contain self public key",
            ));
        }

        let (root_hash, multi_sig_address) =
            self.root_hash_service
                .new_root_hash(public_keys, self_public_key, m, passphrase)?;

        self.wallet_service
            .add_root_hash(name, passphrase, root_hash)?;

        Ok(multi_sig_address.into())
    }

    fn generate_proof(
        &self,
        name: &str,
        passphrase: &SecUtf8,
        address: &ExtendedAddr,
        public_keys: Vec<PublicKey>,
    ) -> Result<Proof<RawPubkey>> {
        // To verify if the passphrase is correct or not
        self.wallet_service.view_key(name, passphrase)?;

        match address {
            ExtendedAddr::OrTree(ref address) => {
                self.root_hash_service
                    .generate_proof(address, public_keys, passphrase)
            }
        }
    }

    fn required_cosigners(
        &self,
        name: &str,
        passphrase: &SecUtf8,
        root_hash: &H256,
    ) -> Result<usize> {
        // To verify if the passphrase is correct or not
        self.wallet_service.view_key(name, passphrase)?;

        self.root_hash_service
            .required_signers(root_hash, passphrase)
    }

    #[inline]
    fn balance(&self, name: &str, passphrase: &SecUtf8) -> Result<WalletBalance> {
        // Check if wallet exists
        self.wallet_service.view_key(name, passphrase)?;
        self.wallet_state_service.get_balance(name, passphrase)
    }

    fn history(
        &self,
        name: &str,
        passphrase: &SecUtf8,
        offset: usize,
        limit: usize,
        reversed: bool,
    ) -> Result<Vec<TransactionChange>> {
        // Check if wallet exists
        self.wallet_service.view_key(name, passphrase)?;

        let history = self
            .wallet_state_service
            .get_transaction_history(name, passphrase, reversed)?
            .filter(|change| BalanceChange::NoChange != change.balance_change)
            .skip(offset)
            .take(limit)
            .collect::<Vec<_>>();

        Ok(history)
    }

    fn unspent_transactions(
        &self,
        name: &str,
        passphrase: &SecUtf8,
    ) -> Result<UnspentTransactions> {
        // Check if wallet exists
        self.wallet_service.view_key(name, passphrase)?;

        let unspent_transactions = self
            .wallet_state_service
            .get_unspent_transactions(name, passphrase, false)?;

        Ok(UnspentTransactions::new(
            unspent_transactions.into_iter().collect(),
        ))
    }

    fn has_unspent_transactions(
        &self,
        name: &str,
        passphrase: &SecUtf8,
        inputs: &[TxoPointer],
    ) -> Result<bool> {
        // Check if wallet exists
        self.wallet_service.view_key(name, passphrase)?;

        self.wallet_state_service
            .has_unspent_transactions(name, passphrase, inputs)
    }

    #[inline]
    fn output(&self, name: &str, passphrase: &SecUtf8, input: &TxoPointer) -> Result<TxOut> {
        // Check if wallet exists
        self.wallet_service.view_key(name, passphrase)?;

        self.wallet_state_service
            .get_output(name, passphrase, input)
            .and_then(|optional| {
                optional.chain(|| {
                    (
                        ErrorKind::InvalidInput,
                        "Output details not found for given transaction input",
                    )
                })
            })
    }

    fn create_transaction(
        &self,
        name: &str,
        passphrase: &SecUtf8,
        outputs: Vec<TxOut>,
        attributes: TxAttributes,
        input_selection_strategy: Option<InputSelectionStrategy>,
        return_address: ExtendedAddr,
    ) -> Result<(TxAux, Vec<TxoPointer>, Coin)> {
        let mut unspent_transactions = self.unspent_transactions(name, passphrase)?;
        unspent_transactions.apply_all(input_selection_strategy.unwrap_or_default().as_ref());

        self.transaction_builder.build_transfer_tx(
            name,
            passphrase,
            unspent_transactions,
            outputs,
            return_address,
            attributes,
        )
    }

    #[inline]
    fn broadcast_transaction(&self, tx_aux: &TxAux) -> Result<BroadcastTxResponse> {
        self.tendermint_client
            .broadcast_transaction(&tx_aux.encode())
    }

    fn export_plain_tx(&self, name: &str, passphrase: &SecUtf8, txid: &str) -> Result<String> {
        let txid = str2txid(txid).chain(|| (ErrorKind::InvalidInput, "invalid transaction id"))?;
        let public_key = self.view_key(name, passphrase)?;
        let private_key = self
            .private_key(passphrase, &public_key)?
            .chain(|| (ErrorKind::StorageError, "can not find private key"))?;
        let tx = self.transaction_builder.decrypt_tx(txid, &private_key)?;
        // get the block height
        let tx_change = self
            .wallet_state_service
            .get_transaction_history(name, passphrase, false)?
            .filter(|change| BalanceChange::NoChange != change.balance_change)
            .find(|tx_change| tx_change.transaction_id == tx.id())
            .chain(|| {
                (
                    ErrorKind::InvalidInput,
                    "no transaction find by transaction id",
                )
            })?;

        let tx_info = TransactionInfo {
            tx,
            block_height: tx_change.block_height,
        };

        let tx_str = serde_json::to_string(&tx_info)
            .chain(|| (ErrorKind::InvalidInput, "invalid transaction id"))?;
        Ok(base64::encode(&tx_str))
    }

    /// import a plain base64 encoded plain transaction
    fn import_plain_tx(&self, name: &str, passphrase: &SecUtf8, tx_str: &str) -> Result<Coin> {
        let tx_raw = base64::decode(tx_str)
            .chain(|| (ErrorKind::DecryptionError, "Unable to decrypt transaction"))?;
        let tx_info: TransactionInfo = serde_json::from_slice(&tx_raw)
            .chain(|| (ErrorKind::DecryptionError, "Unable to decrypt transaction"))?;
        // check if the output is spent or not
        let v = self
            .tendermint_client
            .query("meta", &tx_info.tx.id().to_vec())?
            .bytes()?;
        let bit_flag = BitVec::from_bytes(&v);
        let spent_flags: Result<Vec<bool>> = tx_info
            .tx
            .outputs()
            .iter()
            .enumerate()
            .map(|(index, _output)| {
                bit_flag
                    .get(index)
                    .chain(|| (ErrorKind::InvalidInput, "check failed in enclave"))
            })
            .collect();
        let mut memento = WalletStateMemento::default();
        // check if tx belongs to the block
        let block = self.tendermint_client.block(tx_info.block_height)?;
        if !block.enclave_transaction_ids()?.contains(&tx_info.tx.id()) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "block height and transaction not match",
            ));
        }
        let wallet = self.wallet_service.get_wallet(name, passphrase)?;

        let wallet_state = self.wallet_service.get_wallet_state(name, passphrase)?;

        let imported_value = import_transaction(
            &wallet,
            &wallet_state,
            &mut memento,
            &tx_info.tx,
            tx_info.block_height,
            block.header.time,
            spent_flags?,
        )
        .chain(|| (ErrorKind::InvalidInput, "import error"))?;

        self.wallet_state_service
            .apply_memento(name, passphrase, &memento)?;
        Ok(imported_value)
    }

    fn get_current_block_height(&self) -> Result<u64> {
        let status = self.tendermint_client.status()?;
        let current_block_height = status.sync_info.latest_block_height.value();
        Ok(current_block_height)
    }

    fn update_tx_pending_state(
        &self,
        name: &str,
        passphrase: &SecUtf8,
        tx_id: TxId,
        tx_pending: TransactionPending,
    ) -> Result<()> {
        let mut wallet_state_memento = WalletStateMemento::default();
        wallet_state_memento.add_pending_transaction(tx_id, tx_pending);
        self.wallet_state_service
            .apply_memento(name, passphrase, &wallet_state_memento)
    }
}

impl<S, C, T> MultiSigWalletClient for DefaultWalletClient<S, C, T>
where
    S: Storage,
    C: Client,
    T: WalletTransactionBuilder,
{
    fn schnorr_signature(
        &self,
        name: &str,
        passphrase: &SecUtf8,
        message: &H256,
        public_key: &PublicKey,
    ) -> Result<SchnorrSignature> {
        // To verify if the passphrase is correct or not
        self.transfer_addresses(name, passphrase)?;

        let private_key = self.private_key(passphrase, public_key)?.chain(|| {
            (
                ErrorKind::InvalidInput,
                format!("Public key ({}) is not owned by current wallet", public_key),
            )
        })?;
        private_key.schnorr_sign(message)
    }

    fn new_multi_sig_session(
        &self,
        name: &str,
        passphrase: &SecUtf8,
        message: H256,
        signer_public_keys: Vec<PublicKey>,
        self_public_key: PublicKey,
    ) -> Result<H256> {
        // To verify if the passphrase is correct or not
        self.transfer_addresses(name, passphrase)?;

        let self_private_key = self.private_key(passphrase, &self_public_key)?.chain(|| {
            (
                ErrorKind::InvalidInput,
                format!(
                    "Self public key ({}) is not owned by current wallet",
                    self_public_key
                ),
            )
        })?;

        self.multi_sig_session_service.new_session(
            message,
            signer_public_keys,
            self_public_key,
            self_private_key,
            passphrase,
        )
    }

    fn nonce_commitment(&self, session_id: &H256, passphrase: &SecUtf8) -> Result<H256> {
        self.multi_sig_session_service
            .nonce_commitment(session_id, passphrase)
    }

    fn add_nonce_commitment(
        &self,
        session_id: &H256,
        passphrase: &SecUtf8,
        nonce_commitment: H256,
        public_key: &PublicKey,
    ) -> Result<()> {
        self.multi_sig_session_service.add_nonce_commitment(
            session_id,
            nonce_commitment,
            public_key,
            passphrase,
        )
    }

    fn nonce(&self, session_id: &H256, passphrase: &SecUtf8) -> Result<PublicKey> {
        self.multi_sig_session_service.nonce(session_id, passphrase)
    }

    fn add_nonce(
        &self,
        session_id: &H256,
        passphrase: &SecUtf8,
        nonce: &PublicKey,
        public_key: &PublicKey,
    ) -> Result<()> {
        self.multi_sig_session_service
            .add_nonce(session_id, &nonce, public_key, passphrase)
    }

    fn partial_signature(&self, session_id: &H256, passphrase: &SecUtf8) -> Result<H256> {
        self.multi_sig_session_service
            .partial_signature(session_id, passphrase)
    }

    fn add_partial_signature(
        &self,
        session_id: &H256,
        passphrase: &SecUtf8,
        partial_signature: H256,
        public_key: &PublicKey,
    ) -> Result<()> {
        self.multi_sig_session_service.add_partial_signature(
            session_id,
            partial_signature,
            public_key,
            passphrase,
        )
    }

    fn signature(&self, session_id: &H256, passphrase: &SecUtf8) -> Result<SchnorrSignature> {
        self.multi_sig_session_service
            .signature(session_id, passphrase)
    }

    fn transaction(
        &self,
        name: &str,
        session_id: &H256,
        passphrase: &SecUtf8,
        unsigned_transaction: Tx,
    ) -> Result<TxAux> {
        if unsigned_transaction.inputs.len() != 1 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Multi-Sig Signing is only supported for transactions with only one input",
            ));
        }

        let output_to_spend = self.output(name, passphrase, &unsigned_transaction.inputs[0])?;
        let root_hash = self
            .wallet_service
            .find_root_hash(name, passphrase, &output_to_spend.address)?
            .chain(|| {
                (
                    ErrorKind::IllegalInput,
                    "Output address is not owned by current wallet; cannot spend output in given transaction",
                )
            })?;
        let public_keys = self
            .multi_sig_session_service
            .public_keys(session_id, passphrase)?;

        let proof = self
            .root_hash_service
            .generate_proof(&root_hash, public_keys, passphrase)?;
        let signature = self.signature(session_id, passphrase)?;

        let witness = TxWitness::from(vec![TxInWitness::TreeSig(signature, proof)]);
        let signed_transaction =
            SignedTransaction::TransferTransaction(unsigned_transaction, witness);

        self.transaction_builder.obfuscate(signed_transaction)
    }
}

#[cfg(not(debug_assertions))]
fn check_passphrase_strength(name: &str, passphrase: &SecUtf8) -> Result<()> {
    // `estimate_password_strength` returns a score between `0-4`. Any score less than 3 should be considered too
    // weak.
    let password_entropy = estimate_password_strength(passphrase.unsecure(), &[name])
        .chain(|| (ErrorKind::IllegalInput, "Blank passphrase"))?;

    if password_entropy.score() < 3 {
        return Err(Error::new(
            ErrorKind::IllegalInput,
            format!(
                "Weak passphrase: {}",
                parse_feedback(password_entropy.feedback().as_ref())
            ),
        ));
    }

    Ok(())
}

#[cfg(not(debug_assertions))]
fn parse_feedback(feedback: Option<&Feedback>) -> String {
    match feedback {
        None => "No feedback available!".to_string(),
        Some(feedback) => {
            let mut feedbacks = Vec::new();

            if let Some(warning) = feedback.warning() {
                feedbacks.push(format!("Warning: {}", warning));
            }

            for suggestion in feedback.suggestions() {
                feedbacks.push(format!("Suggestion: {}", suggestion));
            }

            if feedbacks.is_empty() {
                feedbacks.push("No feedback available!".to_string());
            }

            feedbacks.join(" | ")
        }
    }
}

fn import_transaction(
    wallet: &Wallet,
    wallet_state: &WalletState,
    memento: &mut WalletStateMemento,
    transaction: &Transaction,
    block_height: u64,
    block_time: Time,
    spent_flag: Vec<bool>,
) -> Result<Coin> {
    let transaction_change =
        create_transaction_change(wallet, wallet_state, transaction, block_height, block_time)
            .chain(|| (ErrorKind::InvalidInput, "create transaction change failed"))?;
    let mut value = Coin::zero();
    let transfer_addresses = wallet.transfer_addresses();
    for (i, (output, spent)) in transaction_change
        .outputs
        .iter()
        .zip(spent_flag)
        .enumerate()
    {
        // Only add unspent transaction if output address belongs to current wallet
        if transfer_addresses.contains(&output.address) && !spent {
            memento.add_unspent_transaction(
                TxoPointer::new(transaction_change.transaction_id, i),
                output.clone(),
            );
            value = (value + output.value).chain(|| {
                (
                    ErrorKind::InvalidInput,
                    "invalid coin in outputs of transaction",
                )
            })?;
        }
    }
    memento.add_transaction_change(transaction_change);
    Ok(value)
}
