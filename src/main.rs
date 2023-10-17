use bitcoin::absolute::LockTime;
use bitcoin::psbt::{Psbt, PsbtSighashType};
use bitcoin::sighash::EcdsaSighashType;
use bitcoin::Network::Testnet;
use bitcoin::{
    Address, Amount, Network, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness,
};
use bitcoincore_rpc::json::{ListUnspentResultEntry, SigHashType};
use bitcoincore_rpc::{Auth, Client, RpcApi};
use lazy_static::lazy_static;
use std::env;
use std::str::FromStr;

const NETWORK: Network = Testnet;
const PRICE: u64 = 1900;

const SERVICE_FEE: u64 = 1000;

lazy_static! {
    static ref SELLET_ADDRESS: Address = Address::from_str(&env::var("SELLER_ADDRESS").unwrap())
        .unwrap()
        .require_network(NETWORK)
        .unwrap();
    static ref FULL_NODE: Client = rpc_client(
        env::var("BITCOIN_RPC_URL").unwrap(),
        env::var("BITCOIN_RPC_USER").unwrap(),
        env::var("BITCOIN_RPC_PASS").unwrap(),
    );
    static ref SELLLER_NODE: Client = rpc_client(
        env::var("SELLER_RPC_URL").unwrap(),
        env::var("SELLER_RPC_USER").unwrap(),
        env::var("SELLER_RPC_PASS").unwrap(),
    );
    static ref BUYER_NODE: Client = rpc_client(
        env::var("BUYER_RPC_URL").unwrap(),
        env::var("BUYER_RPC_USER").unwrap(),
        env::var("BUYER_RPC_PASS").unwrap(),
    );
}

fn main() {
    dotenv::from_path(".env").unwrap();
    let (seller_psbt, inscription_tx_out) = create_seller_psbt();
    println!("seller_psbt: {}", seller_psbt);
    if seller_psbt.is_empty() {
        println!("seller_psbt should not empty");
        return;
    }

    let buyer_psbt = create_buyer_psbt(seller_psbt, inscription_tx_out);
    println!("buyer_psbt: {}", buyer_psbt);
    if buyer_psbt.is_empty() {
        println!("buyer_psbt should not empty");
        return;
    }

    let raw_buying_tx = BUYER_NODE
        .finalize_psbt(&buyer_psbt, None)
        .unwrap()
        .hex
        .unwrap();

    let buying_txid = BUYER_NODE.send_raw_transaction(&raw_buying_tx).unwrap();
    println!(
        "inscription buying tx was succesfully send: {:?}",
        &buying_txid
    );
}

fn rpc_client(rpc_url: String, user: String, pass: String) -> Client {
    Client::new(&rpc_url, Auth::UserPass(user, pass)).unwrap()
}

fn create_seller_psbt() -> (String, TxOut) {
    let inscription_utxo = OutPoint::from_str(&env::var("SELLER_UTXO").unwrap()).unwrap();
    let tx = FULL_NODE
        .get_raw_transaction(&inscription_utxo.txid, None)
        .unwrap();

    let inscription_output = tx.output[inscription_utxo.vout as usize].clone();

    let tx_sell = Transaction {
        version: 2,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: inscription_utxo.txid,
                vout: inscription_utxo.vout,
            },
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::default(),
        }],
        output: vec![
            TxOut {
                value: PRICE,
                script_pubkey: inscription_output.script_pubkey,
            },
        ],
    };

    let mut psbt = Psbt::from_unsigned_tx(tx_sell).unwrap();

    psbt.inputs[0].non_witness_utxo = Some(tx.clone());
    psbt.inputs[0].sighash_type = Some(PsbtSighashType::from(
        EcdsaSighashType::SinglePlusAnyoneCanPay,
    ));

    let processed_seller_psbt = SELLLER_NODE
        .wallet_process_psbt(
            &psbt.to_string(),
            Some(true),
            Some(SigHashType::from(EcdsaSighashType::SinglePlusAnyoneCanPay)),
            None,
        )
        .unwrap();

    (
        processed_seller_psbt.psbt,
        tx.output[inscription_utxo.vout as usize].clone(),
    )
}

fn create_buyer_psbt(seller_psbt: String, inscription_tx_out: TxOut) -> String {
    let buyer = Address::from_str(&env::var("BUYER_ADDRESS").unwrap())
        .unwrap()
        .require_network(NETWORK)
        .unwrap();

    if BUYER_NODE.get_balance(None, None).unwrap() < Amount::from_sat(PRICE) {
        println!("buyer doesn't have enough funds");
        return Default::default();
    }

    let sorted_spendable_utxos = get_buyer_spendable_utxos(&buyer);

    if sorted_spendable_utxos.len() == 0 {
        println!("buyer doesn't have any spendable utxos");
        return Default::default();
    }

    let dummy_utxo = retrieve_dummy_utxo(&buyer, &sorted_spendable_utxos);
    let buyer_address = dummy_utxo
        .clone()
        .address
        .unwrap()
        .require_network(NETWORK)
        .unwrap();

    let seller_psbt = Psbt::from_str(&seller_psbt).unwrap();
    let seller_psbt_extracted_tx = seller_psbt.clone().extract_tx();
    let reversed_sorted_utxos = sorted_spendable_utxos
        .clone()
        .into_iter()
        .rev()
        .collect::<Vec<_>>();

    let mut purchase_tx = Transaction {
        version: 2,
        lock_time: LockTime::ZERO,
        input: vec![
            TxIn {
                previous_output: OutPoint {
                    txid: dummy_utxo.txid,
                    vout: dummy_utxo.vout,
                },
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::default(),
            },
            TxIn {
                previous_output: seller_psbt_extracted_tx.input[0].previous_output.clone(),
                script_sig: seller_psbt_extracted_tx.input[0].script_sig.clone(),
                sequence: seller_psbt_extracted_tx.input[0].sequence.clone(),
                witness: Witness::default(),
            },
        ],

        output: vec![
            TxOut {
                value: inscription_tx_out.value + dummy_utxo.amount.to_sat(),
                script_pubkey: buyer_address.script_pubkey(),
            },
            seller_psbt_extracted_tx.output[0].clone(),
        ],
    };

    // payment
    let mut payment_utxos_value = 0;
    let required_payment_value = PRICE + SERVICE_FEE + 1000 + 180 * 2 + 3 * 34 + 10;
    let mut selected_payment_utxos: Vec<ListUnspentResultEntry> = Vec::new();

    for utxo in reversed_sorted_utxos {
        selected_payment_utxos.push(utxo.clone());
        purchase_tx.input.push(TxIn {
            previous_output: OutPoint {
                txid: utxo.txid,
                vout: utxo.vout,
            },
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::default(),
        });
        payment_utxos_value += utxo.amount.to_sat();
        if payment_utxos_value >= required_payment_value {
            break;
        }
    }

    if payment_utxos_value < PRICE {
        println!("buyer doesn't have enough funds");
        return Default::default();
    }

    purchase_tx.output.push(TxOut {
        value: SERVICE_FEE,
        script_pubkey: Address::from_str(&env::var("MARKET_PLACE_ADDRESS").unwrap())
            .unwrap()
            .require_network(NETWORK)
            .unwrap()
            .script_pubkey(),
    });


    purchase_tx.output.push(TxOut {
        value: 1000,
        script_pubkey: buyer_address.script_pubkey(),
    });

    purchase_tx.output.push(TxOut {
        value: payment_utxos_value - required_payment_value,
        script_pubkey: buyer_address.script_pubkey(),
    });

    let mut buyer_psbt = Psbt::from_unsigned_tx(purchase_tx.clone()).unwrap();

    buyer_psbt.inputs[0].non_witness_utxo = Some(
        BUYER_NODE
            .get_raw_transaction(&dummy_utxo.txid, None)
            .unwrap(),
    );

    buyer_psbt.inputs[1] = seller_psbt.inputs[0].clone();

    selected_payment_utxos
        .iter()
        .enumerate()
        .for_each(|(i, utxo)| {
            buyer_psbt.inputs[i + 2].non_witness_utxo =
                Some(BUYER_NODE.get_raw_transaction(&utxo.txid, None).unwrap());
        });

    let processed_buyer_psbt = BUYER_NODE
        .wallet_process_psbt(&buyer_psbt.to_string(), Some(true), None, None)
        .unwrap();

    processed_buyer_psbt.psbt
}

fn get_buyer_spendable_utxos(buyer: &Address) -> Vec<ListUnspentResultEntry> {
    let unspent_utxos = BUYER_NODE
        .list_unspent(None, None, Some(&[buyer]), Some(true), None)
        .unwrap();

    // del utxos has inscription
    let mut sorted_spendable_utxos = unspent_utxos
        .into_iter()
        .filter(|x| is_utxo_inscription(x) == false)
        .collect::<Vec<_>>();
    sorted_spendable_utxos.sort_by_key(|x| x.amount);
    sorted_spendable_utxos
}

fn is_utxo_inscription(utxo: &ListUnspentResultEntry) -> bool {
    let explorer_url = std::env::var("ORD_EXPLORER").unwrap()
        + "output/"
        + &utxo.txid.to_string()
        + ":"
        + &utxo.vout.to_string();
    let resp = reqwest::blocking::get(explorer_url)
        .unwrap()
        .text()
        .unwrap();
    if resp.contains("inscription") {
        true
    } else {
        false
    }
}

fn retrieve_dummy_utxo(
    buyer: &Address,
    utxos: &Vec<ListUnspentResultEntry>,
) -> ListUnspentResultEntry {
    let potential_dummy_utxos = &utxos
        .iter()
        .filter(|utxo| utxo.amount <= Amount::from_sat(1000))
        .collect::<Vec<&ListUnspentResultEntry>>();

    let dummy_utxo = if potential_dummy_utxos.len() == 0 {
        let dummy_address = utxos[0]
            .clone()
            .address
            .unwrap()
            .require_network(NETWORK)
            .unwrap();

        let mut dummy_psbt = Psbt::from_unsigned_tx(Transaction {
            version: 2,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: utxos[0].txid,
                    vout: utxos[0].vout,
                },
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::default(),
            }],
            output: vec![
                TxOut {
                    value: 1000,
                    script_pubkey: dummy_address.script_pubkey(),
                },
                TxOut {
                    value: utxos[0].amount.to_sat() - 1000 - 258,
                    script_pubkey: dummy_address.script_pubkey(),
                },
            ],
        })
            .unwrap();

        dummy_psbt.inputs[0].non_witness_utxo = Some(
            BUYER_NODE
                .get_raw_transaction(&utxos[0].txid, None)
                .unwrap(),
        );

        let dummy_psbt_string = &dummy_psbt.to_string();
        let processed_dummy_psbt = BUYER_NODE
            .wallet_process_psbt(dummy_psbt_string, Some(true), None, None)
            .unwrap();
        let processed_dummy_psbt_string = &processed_dummy_psbt.psbt;
        let dummy_raw_tx = BUYER_NODE
            .finalize_psbt(processed_dummy_psbt_string, None)
            .unwrap()
            .hex
            .unwrap();

        let dummy_txid = BUYER_NODE.send_raw_transaction(&dummy_raw_tx).unwrap();
        println!("created dummy {:?}", &dummy_txid);
        let unspent_utxos = BUYER_NODE
            .list_unspent(None, None, Some(&[&buyer]), Some(true), None)
            .unwrap();
        let mut sorted_utxos = unspent_utxos.clone();
        sorted_utxos.sort_by_key(|x| x.amount);
        let potential_dummy_utxos = &sorted_utxos
            .iter()
            .filter(|utxo| utxo.amount <= Amount::from_sat(1000))
            .collect::<Vec<&ListUnspentResultEntry>>();
        potential_dummy_utxos[0].clone()
    } else {
        potential_dummy_utxos[0].clone()
    };

    dummy_utxo
}
