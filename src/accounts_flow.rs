//! Account events and async handling: signing in by dropping in a key (or a
//! passphrase-protected file), making a new wallet, managing wallets, moving
//! SOL. Keys live in memory only; nothing about the person is stored.

use crate::account::{self, Account, Parsed};
use crate::app::{fetch_err, After, App, Load, SendReview, P};
use crate::crypto::{base58, base64_encode};
use crate::host;
use crate::iq;
use crate::json::{self, Json};
use crate::net;
use crate::solana::{self, b58, parse_pk};
use crate::ui;

/// Where earlier versions kept an account in the browser: unencrypted
/// ("Use this browser"), or as a remembered encrypted file. Nothing reads
/// them now; the account page offers to download and delete them.
pub const LEGACY_DEVICE: &str = "iqtables:v1:device";
pub const LEGACY_FILE: &str = "iqtables:v1:account";
/// Flags earlier versions kept (nothing secret), cleared on start.
pub const LEGACY_FLAGS: [&str; 3] = ["iqtables:v1:pk-hint", "iqtables:v1:signed-out", "iqtables:v1:remember"];

/// An account an earlier version left in this browser, if any.
pub fn legacy_account() -> Option<(String, &'static str)> {
    for k in [LEGACY_DEVICE, LEGACY_FILE] {
        if let Some(v) = host::storage_get(k).filter(|s| !s.is_empty()) {
            let name = json::parse(&v).ok().map(|j| j.get("name").str_or("Account")).unwrap_or_else(|| "Account".into());
            return Some((name, k));
        }
    }
    None
}

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    host::random(&mut b);
    b
}

impl App {
    pub fn account_event(&mut self, kind: &str, action: &str, arg: &str, val: &str) -> bool {
        match (kind, action) {
            ("file", "drop-file") | ("file", "account-file") => self.on_file(arg, val),
            (_, "make-wallet") => self.make_wallet(),
            (_, "legacy-download") => {
                let Some(v) = host::storage_get(arg).filter(|s| !s.is_empty()) else { return true };
                let file = if arg == LEGACY_DEVICE {
                    // an unencrypted account: hand it over in the file format this version opens
                    match json::parse(&v).ok().and_then(|j| Account::from_payload(&j.get("name").str_or("Account"), j.get("payload")).ok()) {
                        Some(a) => a.to_file([0; 12]),
                        None => {
                            self.err("That saved account couldn't be read.");
                            return true;
                        }
                    }
                } else {
                    v
                };
                let fname =
                    if arg == LEGACY_DEVICE { "account-from-this-browser-UNENCRYPTED.iqaccount.json" } else { "account-from-this-browser.iqaccount.json" };
                host::download(fname, "application/json", file.as_bytes());
                self.ok(format!("Saved {} — drop it on this page to sign in with it.", fname));
            }
            (_, "legacy-delete") => {
                if arg == LEGACY_DEVICE || arg == LEGACY_FILE {
                    host::storage_set(arg, "");
                    self.ok("Removed from this browser.");
                }
            }
            (_, "panel") => {
                self.panel = if self.panel == arg { String::new() } else { arg.to_string() };
                self.send_review = None;
                if !self.panel.is_empty() {
                    self.fetch_all_balances();
                }
            }
            (_, "add-funds") => {
                self.panel = "add".into();
                self.fetch_all_balances();
                host::set_hash("#/account");
            }
            (_, "send-review") => self.send_review_start(),
            (_, "send-cancel") => self.send_review = None,
            (_, "send-confirm") => {
                let Some(r) = self.send_review.take() else { return true };
                let Some(from) = self.account.as_ref().and_then(|a| a.main()).map(|w| w.address()) else { return true };
                self.form.remove("send-to");
                self.form.remove("send-amt");
                self.panel.clear();
                self.transfer(&from, &r.to, r.lamports, After::Balances);
            }
            ("file", "drop-too-big") => self.err(format!("{} is too big to be an account or key file", arg)),
            ("file", "import-keys") => self.on_file(arg, val),
            (_, "import-keys-text") => {
                let t = self.form.remove("keys-text").unwrap_or_default();
                self.on_file("pasted keys", &t);
            }
            (_, "account-menu") => self.account_menu = !self.account_menu,
            (_, "unlock-account") => self.begin(P::Unlock, "Unlocking your account…"),
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
            (_, "save-account") => self.save_account(),
            (_, "logout") => {
                let working = self.run.as_ref().map(|r| r.busy()).unwrap_or(false)
                    || self.attach_status.is_some()
                    || self.crowd.work.as_ref().map(|w| !w.finished).unwrap_or(false)
                    || self.uploads.values().any(|b| !b.idle());
                if working {
                    self.err("Wait for the save or upload to finish (or stop it) before signing out.");
                } else {
                    // everything holding a key goes: the account, a stopped
                    // save and its parts, crowdfunding work
                    self.run = None;
                    self.uploads.clear();
                    self.crowd.work = None;
                    self.panel.clear();
                    self.send_review = None;
                    self.account = None;
                    self.locked = None;
                    self.account_menu = false;
                    self.reveal_key = None;
                    self.balances.clear();
                    self.ok("Signed out. Your key was only in this tab's memory; it's gone now.");
                }
            }
            (_, "new-wallet") => {
                let label = self.form.remove("wlabel").unwrap_or_default();
                let Some(a) = self.account.as_mut() else { return true };
                let label = if label.trim().is_empty() { format!("Wallet {}", a.next_index + 1) } else { label.trim().to_string() };
                let w = a.new_wallet(&label, "");
                let addr = w.address();
                self.fetch_balance(&addr);
                self.ok(format!("New wallet \"{}\" · {}", label, solana::short(&addr)));
            }
            (_, "wallet-label") => {
                if let Some(w) = self.account.as_mut().and_then(|a| a.find_mut(arg)) {
                    w.label = val.trim().to_string();
                }
            }
            (_, "wallet-remove") => {
                if let Some(a) = self.account.as_mut() {
                    if let Some(i) = a.wallets.iter().position(|w| w.address() == arg && w.kind == account::Kind::Imported) {
                        a.wallets.remove(i);
                    }
                }
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
                    self.err("Sign in first");
                    return true;
                };
                let w = a.new_wallet(&format!("db: {}", name), &format!("Official wallet of database \"{}\"", name));
                let addr = w.address();
                self.drafts[i].wallet = Some(addr.clone());
                self.save_drafts();
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

    // ------------------------------------------------------------ accounts

    /// A new wallet: its key is downloaded as a Solana key file (the same
    /// format the Solana CLI and IQ's git tool use), then it's signed in.
    fn make_wallet(&mut self) {
        if self.account.is_some() {
            self.err("Sign out first to make a new wallet.");
            return;
        }
        let kp = solana::Keypair::from_seed(random::<32>());
        let mut sk = kp.seed.to_vec();
        sk.extend_from_slice(&kp.pubkey);
        let file = format!("[{}]", sk.iter().map(|b| b.to_string()).collect::<Vec<_>>().join(","));
        let addr = b58(&kp.pubkey);
        let fname = format!("wallet-{}.json", addr.chars().take(8).collect::<String>());
        host::download(&fname, "application/json", file.as_bytes());
        let Some(a) = Account::from_keys(vec![("Main".into(), kp)]) else { return };
        self.login(a);
        self.ok(format!("New wallet {} — its key was downloaded as {}. That file is the only way into this wallet: keep a copy somewhere safe, and drop it in to sign in next time.", solana::short(&addr), fname));
    }

    // ------------------------------------------------------------ money

    fn send_review_start(&mut self) {
        let to = self.form.get("send-to").cloned().unwrap_or_default().trim().to_string();
        let amt = self.form.get("send-amt").cloned().unwrap_or_default();
        let Some(main) = self.account.as_ref().and_then(|a| a.main()).map(|w| w.address()) else { return };
        let bal = self.balances.get(&main).and_then(|b| b.ready().copied()).unwrap_or(0);
        let lamports = if amt.trim().eq_ignore_ascii_case("max") || amt.trim().eq_ignore_ascii_case("all") {
            bal.saturating_sub(iq::TX_FEE)
        } else {
            match ui::parse_sol(&amt) {
                Some(l) if l > 0 => l,
                _ => return self.err("Enter how much SOL to send, e.g. 0.25"),
            }
        };
        if lamports + iq::TX_FEE > bal {
            return self.err(format!("That's more than your balance ({})", ui::sol(bal)));
        }
        let lower = to.to_ascii_lowercase();
        if lower.ends_with(".sol") && lower.len() > 4 {
            // look the name up through IQ's gateway
            self.busy_note = Some(format!("Looking up {}…", lower));
            // who receives money is always asked of IQ's own gateway, never a
            // gateway set in Settings (another site on the same domain could
            // have changed those)
            let gw = if self.settings.cluster == "devnet" { crate::app::DEV_GATEWAY } else { crate::app::MAIN_GATEWAY };
            let id = self.nid();
            self.pending.insert(id, P::Sns { name: lower.clone(), lamports });
            host::fetch(id, "GET", &format!("{}/sns/{}", gw, crate::app::pct_encode(&lower)), "", "");
            return;
        }
        if parse_pk(&to).is_none() {
            return self.err("Enter a Solana address or a name like alice.sol");
        }
        if to == main {
            return self.err("That's your own address");
        }
        self.send_review = Some(SendReview { label: solana::short(&to), to, lamports });
    }

    /// Move SOL between wallets (or to anyone), then run `after`.
    pub fn transfer(&mut self, from: &str, to: &str, lamports: u64, after: After) {
        let params = json::parse("[{\"commitment\":\"confirmed\"}]").unwrap();
        self.rpc("getLatestBlockhash", params, P::TransferHash { from: from.into(), to: to.into(), lamports, after });
    }

    fn on_file(&mut self, name: &str, text: &str) {
        let parsed = account::parse_file(text);
        if let (Some(a), Ok(Parsed::Locked(..) | Parsed::Account(_))) = (self.account.as_ref(), &parsed) {
            let msg = format!("You're signed in as {}. Sign out first to switch wallets (or add keys under Account → Advanced).", a.name);
            self.err(msg);
            return;
        }
        match parsed {
            Ok(Parsed::Locked(n, v)) => {
                self.locked = Some((n.clone(), v));
                self.ok(format!("\"{}\" loaded — enter its passphrase to sign in.", n));
                if self.route != crate::app::Route::Account {
                    self.keep_toast = true;
                    host::set_hash("#/account");
                }
            }
            Ok(Parsed::Account(a)) => {
                self.err(
                    "That file is NOT encrypted. You're signed in — download a passphrase-protected copy under Account → Advanced, then delete the old file.",
                );
                self.login(a);
            }
            Ok(Parsed::Keys(keys)) => {
                let n = keys.len();
                if self.account.is_none() {
                    let Some(a) = Account::from_keys(keys) else { return };
                    let who = a.main().map(|w| solana::short(&w.address())).unwrap_or_default();
                    self.login(a);
                    self.ok(format!("Signed in as {}. Your key stays in this tab's memory only.", who));
                } else {
                    let a = self.account.as_mut().unwrap();
                    let mut added = 0;
                    for (l, kp) in keys {
                        let label = if l.trim().is_empty() { format!("Key {}", a.wallets.len() + 1) } else { l };
                        if a.import(kp, &label) {
                            added += 1;
                        }
                    }
                    self.fetch_all_balances();
                    self.ok(format!(
                        "Added {} wallet(s) for this session{}.",
                        added,
                        if added < n { format!(" ({} already here)", n - added) } else { String::new() }
                    ));
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
        for k in ["unlock-pass", "pass1", "pass2", "keys-text"] {
            self.form.remove(k);
        }
        self.rescan_from(None);
        self.fetch_all_balances();
        self.files.clear();
        // drafts made before signing in are saved with this wallet
        let main = self.account.as_ref().and_then(|a| a.main()).map(|w| w.address());
        for d in self.drafts.iter_mut().filter(|d| d.wallet.is_none()) {
            d.wallet = main.clone();
        }
        self.save_drafts();
        if self.route == crate::app::Route::Account {
            self.keep_toast = true;
            host::set_hash("#/mine");
        } else if self.route == crate::app::Route::Mine {
            self.load_mine();
        }
        if self.toast.is_none() {
            self.ok(format!("Signed in as {}", name));
        }
    }

    /// Download the account's keys as a passphrase-protected file.
    fn save_account(&mut self) {
        let Some(a) = self.account.as_mut() else { return };
        if a.seal.is_none() {
            self.err("Choose a passphrase first.");
            return;
        }
        let file = a.to_file(random::<12>());
        let fname = format!("{}.iqaccount.json", a.name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' }).collect::<String>());
        host::download(&fname, "application/json", file.as_bytes());
        self.ok(format!("Saved {} — it holds every wallet here, protected by your passphrase. Drop it in to sign in.", fname));
    }

    /// Look for derived wallets that have been used but aren't listed (made
    /// after the file was saved, or on another device): scan ahead until a
    /// whole window is unused.
    fn rescan_from(&mut self, start: Option<u32>) {
        let Some(a) = self.account.as_ref() else { return };
        let start = start.unwrap_or(a.next_index);
        let cands: Vec<(u32, String)> = a.scan_from(start).into_iter().map(|(i, kp)| (i, b58(&kp.pubkey))).collect();
        let mut addrs = vec![];
        for (_, w) in &cands {
            addrs.push(json::s(w));
            addrs.push(json::s(&b58(&iq::user_inventory_pda(&parse_pk(w).unwrap()))));
        }
        let params = Json::Arr(vec![
            Json::Arr(addrs),
            json::obj(vec![("encoding", json::s("base64")), ("dataSlice", json::obj(vec![("offset", json::n(0)), ("length", json::n(0))]))]),
        ]);
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
            let params = Json::Arr(vec![
                Json::Arr(list),
                json::obj(vec![("encoding", json::s("base64")), ("dataSlice", json::obj(vec![("offset", json::n(0)), ("length", json::n(0))]))]),
            ]);
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
        self.transfer(from, to, lamports, After::Balances);
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
            P::SetPass => {
                self.busy = None;
                let pass = self.form.remove("pass1").unwrap_or_default();
                self.form.remove("pass2");
                if let Some(a) = self.account.as_mut() {
                    a.set_passphrase(&pass, random::<16>());
                }
                self.save_account();
            }
            P::Sns { name, lamports } => {
                self.busy_note = None;
                let v = if http_ok { json::parse(&text).ok() } else { None };
                let pick = |k: &str| v.as_ref().and_then(|v| v.get(k).str().map(String::from)).filter(|a| parse_pk(a).is_some());
                match pick("record").or_else(|| pick("owner")) {
                    Some(to) => self.send_review = Some(SendReview { label: format!("{} ({})", name, solana::short(&to)), to, lamports }),
                    None => self.err(format!("Couldn't find who owns {}", name)),
                }
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
                        self.ok(format!("Recovered {} wallet(s) made from this key.", found.len()));
                        // keep looking past the last one found
                        let next = cands.last().map(|(i, _)| i + 1);
                        self.rescan_from(next);
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
            P::TransferHash { from, to, lamports, after } => {
                let bh = res().ok().and_then(|r| r.get("value").get("blockhash").str().and_then(base58::decode32));
                let (Some(bh), Some(kp), Some(dest)) = (bh, self.keypair(&from), parse_pk(&to)) else {
                    self.after_failed(&after, "Couldn't prepare the transfer (no blockhash from the RPC)".into());
                    return true;
                };
                let msg = solana::compile(&kp.pubkey, &[solana::system_transfer(&kp.pubkey, &dest, lamports)], bh);
                let (raw, _) = solana::legacy_signed(&msg, &kp.seed);
                let params = json::parse(&format!("[\"{}\",{{\"encoding\":\"base64\",\"preflightCommitment\":\"confirmed\"}}]", base64_encode(&raw))).unwrap();
                self.rpc("sendTransaction", params, P::TransferSent(after));
                self.ok(format!("Sending {} to {}…", ui::sol(lamports), solana::short(&to)));
            }
            P::TransferSent(after) => match res() {
                Ok(r) => {
                    let sig = r.str_or("");
                    let now = host::now_ms();
                    self.timer(1200, P::ConfirmTick { what: "Transfer".into(), sig, since: now, after });
                    return false;
                }
                Err(e) => self.after_failed(&after, format!("Transfer failed: {}", e)),
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
