//! Account events and async handling: logging in by dropping a file,
//! unlocking, creating and saving accounts, managing wallets, moving SOL.

use crate::account::{self, Account, Parsed};
use crate::app::{fetch_err, After, App, Load, P, K_ACCOUNT, K_REMEMBER};
use crate::crypto::{base58, base64_encode};
use crate::host;
use crate::iq;
use crate::json::{self, Json};
use crate::net;
use crate::solana::{self, b58, parse_pk};
use crate::ui;

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    host::random(&mut b);
    b
}

impl App {
    pub fn account_event(&mut self, kind: &str, action: &str, arg: &str, val: &str) -> bool {
        match (kind, action) {
            ("file", "drop-file") | ("file", "account-file") => self.on_file(arg, val),
            ("file", "drop-too-big") => self.err(format!("{} is too big to be an account or key file", arg)),
            ("file", "import-keys") => self.on_file(arg, val),
            (_, "import-keys-text") => {
                let t = self.form.remove("keys-text").unwrap_or_default();
                self.on_file("pasted keys", &t);
            }
            (_, "account-menu") => self.account_menu = !self.account_menu,
            (_, "unlock-account") => self.begin(P::Unlock, "Unlocking your account…"),
            (_, "unlock-remembered") => {
                match host::storage_get(K_ACCOUNT).and_then(|s| json::parse(&s).ok()) {
                    Some(v) => {
                        self.locked = Some((v.get("name").str_or("Account"), v));
                        self.begin(P::Unlock, "Unlocking your account…");
                    }
                    None => self.err("Nothing is remembered on this device."),
                }
            }
            (_, "forget-device") => {
                host::storage_set(K_ACCOUNT, "");
                host::storage_set(K_REMEMBER, "0");
                self.remember = false;
                self.ok("This device no longer keeps your account file.");
            }
            (_, "create-account") => {
                let name = self.form.get("acct-name").cloned().unwrap_or_default().trim().to_string();
                let (p1, p2) = (self.form.get("pass1").cloned().unwrap_or_default(), self.form.get("pass2").cloned().unwrap_or_default());
                if name.is_empty() {
                    self.err("Give the account a name");
                } else if p1.chars().count() < 10 {
                    self.err("Use a passphrase of at least 10 characters — it's the only thing protecting the keys if the file leaks.");
                } else if p1 != p2 {
                    self.err("The passphrases don't match");
                } else {
                    self.begin(P::CreateAccount, "Creating your account…");
                }
            }
            (_, "set-passphrase") => {
                let (p1, p2) = (self.form.get("pass1").cloned().unwrap_or_default(), self.form.get("pass2").cloned().unwrap_or_default());
                if p1.chars().count() < 10 {
                    self.err("Use a passphrase of at least 10 characters");
                } else if p1 != p2 {
                    self.err("The passphrases don't match");
                } else {
                    self.begin(P::SetPass, "Encrypting…");
                }
            }
            (_, "remember") => {
                self.remember = val == "true";
                host::storage_set(K_REMEMBER, if self.remember { "1" } else { "0" });
                if self.remember {
                    self.store_on_device();
                } else {
                    host::storage_set(K_ACCOUNT, "");
                }
            }
            (_, "save-account") => self.save_account(arg == "plain"),
            (_, "logout") => {
                if self.run.as_ref().map(|r| r.busy()).unwrap_or(false) {
                    self.err("Wait for the inscription to finish (or stop it) before logging out.");
                } else {
                    if self.account.as_ref().map(|a| a.dirty).unwrap_or(false) && self.unsaved_keys {
                        self.err("You have imported keys that aren't saved in your account file. Save it first (or they'll be lost).");
                        return true;
                    }
                    self.account = None;
                    self.locked = None;
                    self.account_menu = false;
                    self.reveal_key = None;
                    self.ok("Logged out. Your keys were cleared from this page.");
                }
            }
            (_, "new-wallet") => {
                let label = self.form.remove("wlabel").unwrap_or_default();
                let Some(a) = self.account.as_mut() else { return true };
                let label = if label.trim().is_empty() { format!("Wallet {}", a.next_index + 1) } else { label.trim().to_string() };
                let w = a.new_wallet(&label, "");
                a.dirty = true;
                let addr = w.address();
                self.fetch_balance(&addr);
                self.store_on_device();
                self.ok(format!("New wallet \"{}\" · {}", label, solana::short(&addr)));
            }
            (_, "wallet-label") => {
                if let Some(w) = self.account.as_mut().and_then(|a| a.find_mut(arg)) {
                    w.label = val.trim().to_string();
                }
                if let Some(a) = self.account.as_mut() {
                    a.dirty = true;
                }
                self.store_on_device();
            }
            (_, "wallet-remove") => {
                if let Some(a) = self.account.as_mut() {
                    if let Some(i) = a.wallets.iter().position(|w| w.address() == arg && w.kind == account::Kind::Imported) {
                        a.wallets.remove(i);
                        a.dirty = true;
                    }
                }
                self.store_on_device();
            }
            (_, "refresh-balances") => self.fetch_all_balances(),
            // Enter in the amount field passes that field's key ("amt:<wallet>")
            (_, "transfer") => self.transfer_from(arg.strip_prefix("amt:").unwrap_or(arg)),
            (_, "airdrop") => {
                if self.settings.cluster != "devnet" {
                    self.err("Airdrops only exist on devnet (Settings → Cluster).");
                } else {
                    let params = json::parse(&format!("[\"{}\",1000000000]", arg)).unwrap();
                    self.rpc("requestAirdrop", params, P::Airdrop(arg.to_string()));
                    self.ok("Requesting 1 devnet SOL…");
                }
            }
            (_, "draft-wallet") => {
                if let Some(i) = self.draft_idx(arg) {
                    if self.drafts[i].root_sig.is_some() && self.drafts[i].wallet.is_some() {
                        self.err("This database already exists on chain; its wallet can't change.");
                    } else if !val.is_empty() {
                        self.drafts[i].wallet = Some(val.to_string());
                        self.save_drafts();
                        self.fetch_balance(val);
                    }
                }
            }
            (_, "draft-new-wallet") => {
                let Some(i) = self.draft_idx(arg) else { return true };
                let name = self.drafts[i].name.clone();
                let Some(a) = self.account.as_mut() else {
                    self.err("Log in first");
                    return true;
                };
                let w = a.new_wallet(&format!("db: {}", name), &format!("Official wallet of database \"{}\"", name));
                a.dirty = true;
                let addr = w.address();
                self.drafts[i].wallet = Some(addr.clone());
                self.save_drafts();
                self.store_on_device();
                self.fetch_balance(&addr);
                self.ok(format!("Created dedicated wallet {} for \"{}\"", solana::short(&addr), name));
            }
            (_, "fund-from") => {
                // move SOL from another account wallet into this draft's wallet
                let Some(i) = self.draft_idx(arg) else { return true };
                let Some(to) = self.drafts[i].wallet.clone() else { return true };
                let from = self.form.get(&format!("ffrom:{}", arg)).cloned().unwrap_or_default();
                let amt = self.form.get(&format!("famt:{}", arg)).cloned().unwrap_or_default();
                self.send_sol(&from, &to, &amt);
            }
            _ => return false,
        }
        true
    }

    /// Show a busy state now; do the heavy work on the next tick.
    fn begin(&mut self, p: P, msg: &str) {
        self.busy = Some(msg.to_string());
        self.timer(30, p);
    }

    fn on_file(&mut self, name: &str, text: &str) {
        let parsed = account::parse_file(name, text);
        if let (Some(a), Ok(Parsed::Locked(..) | Parsed::Account(_))) = (self.account.as_ref(), &parsed) {
            let msg = format!("You're logged in as {}. Log out first to switch to another account file (keys can be imported into this one instead).", a.name);
            self.err(msg);
            return;
        }
        match parsed {
            Ok(Parsed::Locked(n, v)) => {
                self.locked = Some((n.clone(), v));
                self.ok(format!("Account file \"{}\" loaded — enter its passphrase to log in.", n));
                if self.route != crate::app::Route::Account {
                    self.keep_toast = true;
                    host::set_hash("#/account");
                }
            }
            Ok(Parsed::Account(a)) => {
                self.err("That account file is NOT encrypted. You're logged in, but set a passphrase and save a new file, then delete the old one.");
                self.login(a);
            }
            Ok(Parsed::Keys(keys)) => {
                let n = keys.len();
                if self.account.is_none() {
                    let mut a = Account::new("Imported keys", random::<32>());
                    a.wallets.clear();
                    a.next_index = 0;
                    for (label, kp) in keys {
                        a.import(kp, &label);
                    }
                    self.unsaved_keys = true;
                    self.login(a);
                    self.ok(format!("Logged in with {} imported key(s). Set a passphrase and save your account file so you don't lose them.", n));
                } else {
                    let a = self.account.as_mut().unwrap();
                    let added = keys.into_iter().filter(|(l, kp)| a.import(kp.clone(), l)).count();
                    if added > 0 {
                        self.unsaved_keys = true;
                    }
                    self.fetch_all_balances();
                    self.ok(format!("Imported {} new wallet(s){} — save your account file to keep them.", added, if added < n { format!(" ({} already present)", n - added) } else { String::new() }));
                }
            }
            Err(e) => self.err(format!("{}: {}", name, e)),
        }
    }

    fn login(&mut self, a: Account) {
        let name = a.name.clone();
        self.account = Some(a);
        self.locked = None;
        self.busy = None;
        for k in ["unlock-pass", "pass1", "pass2"] {
            self.form.remove(k);
        }
        self.store_on_device();
        self.rescan();
        self.fetch_all_balances();
        self.files.clear();
        if self.route == crate::app::Route::Account {
            self.keep_toast = true;
            host::set_hash("#/mine");
        } else if self.route == crate::app::Route::Mine {
            self.load_mine();
        }
        if self.toast.is_none() {
            self.ok(format!("Logged in as {}", name));
        }
    }

    /// Keep the (encrypted) account file in this browser, if allowed.
    pub fn store_on_device(&mut self) {
        if !self.remember {
            return;
        }
        if let Some(a) = self.account.as_ref() {
            if a.seal.is_some() {
                host::storage_set(K_ACCOUNT, &a.to_file(random::<12>()));
            }
        }
    }

    fn save_account(&mut self, plain: bool) {
        let Some(a) = self.account.as_mut() else { return };
        if a.seal.is_none() && !plain {
            self.err("Set a passphrase first (or use the unencrypted export).");
            return;
        }
        let file = if plain {
            let mut c = a.clone();
            c.seal = None;
            c.to_file([0; 12])
        } else {
            a.to_file(random::<12>())
        };
        let fname = format!(
            "{}{}.iqaccount.json",
            a.name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' }).collect::<String>(),
            if plain { "-UNENCRYPTED" } else { "" }
        );
        host::download(&fname, "application/json", file.as_bytes());
        if !plain {
            a.dirty = false;
            self.unsaved_keys = false;
            self.store_on_device();
            self.ok(format!("Saved {} — keep it somewhere safe; it's the key to every wallet in the account.", fname));
        } else {
            self.err(format!("Saved {} UNENCRYPTED. Anyone who gets this file controls every wallet in it.", fname));
        }
    }

    /// Look for wallets created after the file was last saved.
    fn rescan(&mut self) {
        let Some(a) = self.account.as_ref() else { return };
        let cands: Vec<(u32, String)> = a.rescan_candidates().into_iter().map(|(i, kp)| (i, b58(&kp.pubkey))).collect();
        let mut addrs = vec![];
        for (_, w) in &cands {
            addrs.push(json::s(w));
            addrs.push(json::s(&b58(&iq::user_inventory_pda(&parse_pk(w).unwrap()))));
        }
        let params = Json::Arr(vec![Json::Arr(addrs), json::obj(vec![("encoding", json::s("base64")), ("dataSlice", json::obj(vec![("offset", json::n(0)), ("length", json::n(0))]))])]);
        self.rpc("getMultipleAccounts", params, P::Rescan(cands));
    }

    pub fn fetch_all_balances(&mut self) {
        let Some(a) = self.account.as_ref() else { return };
        let addrs = a.addresses();
        if addrs.is_empty() {
            return;
        }
        for w in &addrs {
            self.balances.entry(w.clone()).or_insert(Load::Loading);
        }
        for chunk in addrs.chunks(100) {
            let list = chunk.iter().map(|w| json::s(w)).collect();
            let params = Json::Arr(vec![Json::Arr(list), json::obj(vec![("encoding", json::s("base64")), ("dataSlice", json::obj(vec![("offset", json::n(0)), ("length", json::n(0))]))])]);
            self.rpc("getMultipleAccounts", params, P::Balances(chunk.to_vec()));
        }
    }

    fn transfer_from(&mut self, from: &str) {
        let to_sel = self.form.get(&format!("to:{}", from)).cloned().unwrap_or_default();
        let to_addr = self.form.get(&format!("to2:{}", from)).cloned().unwrap_or_default();
        let to = if !to_addr.trim().is_empty() { to_addr.trim().to_string() } else { to_sel };
        let amt = self.form.get(&format!("amt:{}", from)).cloned().unwrap_or_default();
        self.send_sol(from, &to, &amt);
    }

    fn send_sol(&mut self, from: &str, to: &str, amount: &str) {
        let Some(_) = self.keypair(from) else {
            self.err("Pick a wallet from your account to send from");
            return;
        };
        if parse_pk(to).is_none() {
            self.err("Enter a valid destination address");
            return;
        }
        if from == to {
            self.err("Source and destination are the same wallet");
            return;
        }
        let bal = self.balances.get(from).and_then(|b| b.ready().copied()).unwrap_or(0);
        let lamports = if amount.trim().eq_ignore_ascii_case("all") || amount.trim().eq_ignore_ascii_case("max") {
            bal.saturating_sub(iq::TX_FEE)
        } else {
            match ui::parse_sol(amount) {
                Some(l) if l > 0 => l,
                _ => {
                    self.err("Enter an amount in SOL (e.g. 0.25), or \"all\"");
                    return;
                }
            }
        };
        if lamports + iq::TX_FEE > bal {
            self.err(format!("Not enough SOL: the wallet has {}", ui::sol(bal)));
            return;
        }
        let params = json::parse("[{\"commitment\":\"confirmed\"}]").unwrap();
        self.rpc("getLatestBlockhash", params, P::TransferHash { from: from.into(), to: to.into(), lamports });
    }

    pub fn account_async(&mut self, p: P, ok: bool, status: u32, data: Vec<u8>) -> bool {
        let text = String::from_utf8_lossy(&data).into_owned();
        let http_ok = ok && (200..300).contains(&status);
        let res = || if http_ok { net::rpc_result(&text) } else { Err(fetch_err(ok, status, &text)) };
        match p {
            P::Unlock => {
                self.busy = None;
                let Some((_, file)) = self.locked.clone() else { return true };
                let pass = self.form.remove("unlock-pass").unwrap_or_default();
                match account::unlock(&file, &pass) {
                    Ok(a) => self.login(a),
                    Err(e) => self.err(e),
                }
            }
            P::CreateAccount => {
                self.busy = None;
                let name = self.form.remove("acct-name").unwrap_or_default().trim().to_string();
                let pass = self.form.remove("pass1").unwrap_or_default();
                self.form.remove("pass2");
                let mut a = Account::new(&name, random::<32>());
                a.set_passphrase(&pass, random::<16>());
                let file = a.to_file(random::<12>());
                let fname = format!("{}.iqaccount.json", name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' }).collect::<String>());
                host::download(&fname, "application/json", file.as_bytes());
                a.dirty = false;
                self.login(a);
                self.ok(format!("Account created and saved as {}. That file plus your passphrase is your login — back it up.", fname));
            }
            P::SetPass => {
                self.busy = None;
                let pass = self.form.remove("pass1").unwrap_or_default();
                self.form.remove("pass2");
                if let Some(a) = self.account.as_mut() {
                    a.set_passphrase(&pass, random::<16>());
                }
                self.save_account(false);
            }
            P::Rescan(cands) => {
                if let Ok(v) = res() {
                    let vals = v.get("value").arr().to_vec();
                    let mut found = vec![];
                    for (k, (i, _)) in cands.iter().enumerate() {
                        let used = vals.get(2 * k).map(|x| !x.is_null()).unwrap_or(false) || vals.get(2 * k + 1).map(|x| !x.is_null()).unwrap_or(false);
                        if used {
                            found.push(*i);
                        }
                    }
                    if let Some(a) = self.account.as_mut() {
                        for i in &found {
                            a.recover(*i);
                        }
                    }
                    if !found.is_empty() {
                        self.fetch_all_balances();
                        self.ok(format!("Recovered {} wallet(s) created after this file was saved. Save the account file to keep their labels.", found.len()));
                    }
                }
            }
            P::Balances(addrs) => match res() {
                Ok(v) => {
                    for (i, a) in addrs.iter().enumerate() {
                        let l = v.get("value").idx(i).get("lamports").u64().unwrap_or(0);
                        self.balances.insert(a.clone(), Load::Ready(l));
                    }
                }
                Err(e) => {
                    for a in addrs {
                        self.balances.insert(a, Load::Err(e.clone()));
                    }
                }
            },
            P::TransferHash { from, to, lamports } => {
                let bh = res().ok().and_then(|r| r.get("value").get("blockhash").str().and_then(base58::decode32));
                let (Some(bh), Some(kp), Some(dest)) = (bh, self.keypair(&from), parse_pk(&to)) else {
                    self.err("Couldn't prepare the transfer (no blockhash from the RPC)");
                    return true;
                };
                let msg = solana::compile(&kp.pubkey, &[solana::system_transfer(&kp.pubkey, &dest, lamports)], bh);
                let (raw, _) = solana::legacy_signed(&msg, &kp.seed);
                let params = json::parse(&format!("[\"{}\",{{\"encoding\":\"base64\",\"preflightCommitment\":\"confirmed\"}}]", base64_encode(&raw))).unwrap();
                self.rpc("sendTransaction", params, P::TransferSent);
                self.ok(format!("Sending {} to {}…", ui::sol(lamports), solana::short(&to)));
            }
            P::TransferSent => match res() {
                Ok(r) => {
                    let sig = r.str_or("");
                    let now = host::now_ms();
                    self.timer(1200, P::ConfirmTick { what: "Transfer".into(), sig, since: now, after: After::Balances });
                    return false;
                }
                Err(e) => self.err(format!("Transfer failed: {}", e)),
            },
            P::Airdrop(addr) => match res() {
                Ok(r) => {
                    let sig = r.str_or("");
                    let now = host::now_ms();
                    let _ = addr;
                    self.timer(1500, P::ConfirmTick { what: "Airdrop".into(), sig, since: now, after: After::Balances });
                    return false;
                }
                Err(e) => self.err(format!("Airdrop refused (devnet faucets are rate-limited; try again later or use faucet.solana.com): {}", e)),
            },
            _ => return false,
        }
        true
    }
}
