//! Account events and async handling: logging in by dropping a file,
//! unlocking, creating and saving accounts, managing wallets, moving SOL.

use crate::account::{self, Account, Origin, Parsed};
use crate::app::{fetch_err, After, App, Load, SendReview, P, K_ACCOUNT, K_REMEMBER};
use crate::crypto::{aead, base58, base64_decode, base64_encode, hex, sha2, unhex};
use crate::host;
use crate::iq;
use crate::json::{self, Json};
use crate::net;
use crate::solana::{self, b58, parse_pk};
use crate::ui;

pub const K_DEVICE: &str = "iqtables:v1:device";
pub const K_PK_HINT: &str = "iqtables:v1:pk-hint";
pub const K_SIGNED_OUT: &str = "iqtables:v1:signed-out";
const K_PK_STORE: &str = "iqtables:v1:pk:";

/// The PRF input every passkey evaluates (frozen: changing it changes every
/// passkey account's wallets).
fn prf_salt() -> [u8; 32] {
    sha2::sha256_parts(&[b"iq-tables/prf-salt/v1"])
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
            (_, "passkey-create") | (_, "passkey-signin") => {
                let create = action == "passkey-create";
                let id = self.nid();
                self.pending.insert(id, P::Passkey(create));
                let req = json::obj(vec![
                    ("mode", json::s(if create { "create" } else { "get" })),
                    ("salt", json::s(&base64_encode(&prf_salt()))),
                    ("name", json::s("IQ Tables")),
                ]);
                self.busy = Some(if create { "Follow your device's prompt to create your account…".into() } else { "Follow your device's prompt to sign in…".into() });
                host::passkey(id, &req.to_string());
            }
            (_, "browser-account") => self.create_browser_account(),
            (_, "device-signin") => {
                host::storage_set(K_SIGNED_OUT, "");
                if !self.auto_login() {
                    self.err("There's no account saved in this browser.");
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
                    let origin = self.account.as_ref().map(|a| a.origin.clone());
                    if origin == Some(Origin::File) && self.account.as_ref().map(|a| a.dirty).unwrap_or(false) && self.unsaved_keys {
                        self.err("You have imported keys that aren't saved in your account file. Save it first (or they'll be lost).");
                        return true;
                    }
                    if origin == Some(Origin::Browser) {
                        host::storage_set(K_SIGNED_OUT, "1");
                    }
                    self.panel.clear();
                    self.send_review = None;
                    self.account = None;
                    self.locked = None;
                    self.account_menu = false;
                    self.reveal_key = None;
                    self.ok("Signed out.");
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

    // ------------------------------------------------------------ accounts

    /// Sign back in to an account kept in this browser (no prompt).
    pub fn auto_login(&mut self) -> bool {
        if host::storage_get(K_SIGNED_OUT).as_deref() == Some("1") {
            return false;
        }
        let Some(v) = host::storage_get(K_DEVICE).filter(|s| !s.is_empty()).and_then(|s| json::parse(&s).ok()) else { return false };
        match Account::from_store_json(&v) {
            Ok(mut a) => {
                a.origin = Origin::Browser;
                a.dirty = false;
                self.account = Some(a);
                self.rescan_from(None);
                self.fetch_all_balances();
                true
            }
            Err(_) => false,
        }
    }

    fn create_browser_account(&mut self) {
        if let Some(v) = host::storage_get(K_DEVICE).filter(|s| !s.is_empty()) {
            // never overwrite a wallet that may hold money
            if json::parse(&v).is_ok() {
                host::storage_set(K_SIGNED_OUT, "");
                self.auto_login();
                self.ok("Signed back in to the account saved in this browser.");
                self.after_login_route();
                return;
            }
        }
        let mut a = Account::new("My account", random::<32>());
        a.origin = Origin::Browser;
        host::storage_set(K_SIGNED_OUT, "");
        self.login(a);
        self.ok("Your account is ready. It's saved in this browser — once it holds money, download a backup from the Account page.");
    }

    fn after_login_route(&mut self) {
        if self.route == crate::app::Route::Account {
            self.keep_toast = true;
            host::set_hash("#/mine");
        }
    }

    fn passkey_result(&mut self, create: bool, text: &str) {
        self.busy = None;
        let v = json::parse(text).unwrap_or(Json::Null);
        let prf = v.get("prf").str().and_then(base64_decode).filter(|p| p.len() >= 32);
        let cred = v.get("cred").str_or("");
        if let (Some(prf), false) = (prf, cred.is_empty()) {
            let master = sha2::sha256_parts(&[b"iq-tables/passkey-master/v1", &prf]);
            let store_key = sha2::sha256_parts(&[b"iq-tables/passkey-store/v1", &prf]);
            host::storage_set(K_PK_HINT, "1");
            // the wallet list this browser remembers for this passkey
            let stored = host::storage_get(&format!("{}{}", K_PK_STORE, cred)).and_then(|s| json::parse(&s).ok()).and_then(|e| {
                let iv = unhex(e.get("iv").str()?)?;
                let ct = unhex(e.get("ct").str()?)?;
                let iv: [u8; 12] = iv.try_into().ok()?;
                let plain = aead::gcm_decrypt(&store_key, &iv, &ct)?;
                json::parse(&String::from_utf8(plain).ok()?).ok()
            });
            let mut a = match stored.and_then(|v| Account::from_store_json(&v).ok()).filter(|a| a.master == master) {
                Some(a) => a,
                None => Account::new("My account", master),
            };
            a.origin = Origin::Passkey { cred, store_key };
            let fresh = create;
            self.login(a);
            self.ok(if fresh { "Your account is ready — your passkey is your login on every device where it's synced." } else { "Signed in." });
            return;
        }
        let err = v.get("error").str_or("");
        if v.get("unsupported").bool() == Some(true) {
            if create {
                self.create_browser_account();
                self.ok("This browser can't use passkeys here, so your account is saved in this browser instead. Once it holds money, download a backup from the Account page.");
            } else {
                self.err("This browser can't use passkeys here. If you made your account in this browser, use \"Continue with this browser\"; otherwise sign in with your backup file.");
            }
        } else if err == "NotAllowedError" || err == "AbortError" {
            self.err("Cancelled.");
        } else {
            self.err(format!("Your device couldn't {} the passkey: {}", if create { "create" } else { "use" }, v.get("message").str_or(&err)));
        }
    }

    /// Save the account wherever its origin keeps it.
    pub fn persist_account(&mut self) {
        let Some(a) = self.account.as_ref() else { return };
        match &a.origin {
            Origin::File => {
                if self.remember && a.seal.is_some() {
                    host::storage_set(K_ACCOUNT, &a.to_file(random::<12>()));
                }
            }
            Origin::Browser => host::storage_set(K_DEVICE, &a.store_json()),
            Origin::Passkey { cred, store_key } => {
                let iv = random::<12>();
                let ct = aead::gcm_encrypt(store_key, &iv, a.store_json().as_bytes());
                let e = json::obj(vec![("iv", json::s(&hex(&iv))), ("ct", json::s(&hex(&ct)))]);
                host::storage_set(&format!("{}{}", K_PK_STORE, cred), &e.to_string());
            }
        }
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
            self.get(&format!("/sns/{}", crate::app::pct_encode(&lower)), P::Sns { name: lower, lamports });
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
        self.persist_account();
        self.rescan_from(None);
        self.fetch_all_balances();
        self.files.clear();
        // drafts made before signing in get their own wallet now
        let names: Vec<(usize, String)> = self.drafts.iter().enumerate().filter(|(_, d)| d.wallet.is_none()).map(|(i, d)| (i, d.name.clone())).collect();
        for (i, n) in names {
            if let Some(a) = self.account.as_mut() {
                let w = a.new_wallet(&format!("db: {}", n), &format!("Wallet of database \"{}\"", n));
                self.drafts[i].wallet = Some(w.address());
            }
        }
        self.save_drafts();
        self.persist_account();
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

    /// Keep the account where it lives (browser, passkey store, or the
    /// remembered encrypted file).
    pub fn store_on_device(&mut self) {
        self.persist_account();
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
            P::Passkey(create) => self.passkey_result(create, &text),
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
                        self.persist_account();
                        let file = self.account.as_ref().map(|a| a.origin == Origin::File).unwrap_or(false);
                        if file {
                            self.ok(format!("Recovered {} wallet(s) created after this file was saved. Save the account file to keep their labels.", found.len()));
                        }
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
                    self.err("Couldn't prepare the transfer (no blockhash from the RPC)");
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
