//! Views for the account: the header button, the account page (log in,
//! create, wallets), "My tables", and the database-wallet card in drafts.

use crate::account::Kind;
use crate::app::{App, Load, Route, K_ACCOUNT};
use crate::host;
use crate::iq;
use crate::json;
use crate::net;
use crate::solana::{self, b58};
use crate::ui::{self, addr, esc};

fn form<'a>(app: &'a App, k: &str) -> &'a str {
    app.form.get(k).map(|s| s.as_str()).unwrap_or("")
}

pub fn balance_text(app: &App, a: &str) -> String {
    match app.balances.get(a) {
        Some(Load::Ready(b)) => ui::sol(*b),
        Some(Load::Loading) => "…".into(),
        Some(Load::Err(_)) => "<span class=\"warn\" title=\"couldn't read balance\">?</span>".into(),
        _ => "—".into(),
    }
}

/// A note when the RPC can't be used from the browser (Solana's public
/// mainnet endpoint answers browsers with 403).
pub fn rpc_hint(app: &App) -> String {
    let bad = app.balances.values().find_map(|b| match b {
        Load::Err(e) => Some(e.clone()),
        _ => None,
    });
    match bad {
        Some(e) => format!(
            "<div class=\"card warnbox small\"><b>Balances and transactions need a Solana RPC that accepts browser requests.</b> {} → set one in <a href=\"#/settings\">Settings</a> (a free Helius, QuickNode or Triton key works). Reading tables still works through IQ's gateway.</div>",
            esc(&e.chars().take(140).collect::<String>())
        ),
        None => String::new(),
    }
}

pub fn total_balance(app: &App) -> Option<u64> {
    let a = app.account.as_ref()?;
    Some(a.addresses().iter().filter_map(|w| app.balances.get(w).and_then(|b| b.ready().copied())).sum())
}

/// Label for an address if it's one of the account's wallets.
pub fn wallet_label(app: &App, a: &str) -> Option<String> {
    app.account.as_ref()?.find(a).map(|w| w.label.clone())
}

/// "label (addr)" when the address is ours, otherwise the short address.
pub fn who(app: &App, a: &str) -> String {
    match wallet_label(app, a) {
        Some(l) => format!("<b>{}</b> {}", esc(&l), addr(a)),
        None => addr(a),
    }
}

pub fn main_balance(app: &App) -> Option<u64> {
    let a = app.account.as_ref()?;
    let m = a.main()?.address();
    app.balances.get(&m).and_then(|b| b.ready().copied())
}

pub fn account_button(app: &App, h: &mut String) {
    h.push_str("<div class=\"wallet\">");
    match (&app.account, &app.locked) {
        (Some(a), _) => {
            let bal = main_balance(app).map(ui::sol).unwrap_or_else(|| "…".into());
            h.push_str(&format!(
                "<button class=\"btn ghostbtn\" data-a=\"account-menu\" aria-expanded=\"{}\" title=\"{}\"><span class=\"dot on\"></span>{}</button>",
                app.account_menu,
                esc(&a.name),
                esc(&bal)
            ));
            if app.account_menu {
                h.push_str(&format!(
                    "<div class=\"menu\"><div><b>{}</b><div class=\"muted small\">Balance {}</div></div><button class=\"btn wide\" data-a=\"add-funds\">Add funds</button><a class=\"btn wide\" href=\"#/account\">Account</a><a class=\"btn wide\" href=\"#/mine\">My tables</a><button class=\"btn wide\" data-a=\"logout\">Sign out</button></div>",
                    esc(&a.name),
                    esc(&bal)
                ));
            }
        }
        (None, Some(_)) => h.push_str("<a class=\"btn primary\" href=\"#/account\">Unlock</a>"),
        (None, None) => h.push_str("<a class=\"btn\" href=\"#/account\">Sign in</a> <a class=\"btn primary\" href=\"#/account\">Create account</a>"),
    }
    h.push_str("</div>");
}

// ------------------------------------------------------------------ account

pub fn account(app: &App, h: &mut String) {
    match &app.account {
        None => logged_out(app, h),
        Some(_) => logged_in(app, h),
    }
}

fn keys_import(h: &mut String, app: &App) {
    h.push_str(&format!(
        "<details><summary>Import existing keys</summary><p class=\"small muted\">Drop or choose a Solana CLI keypair (<code>id.json</code>), a JSON list of them, or a text file with one base58 secret key per line (optionally <code>label: key</code>). You can also paste them here:</p><textarea id=\"keys-text\" rows=\"4\" placeholder=\"treasury: 4Nd1m…\" data-in=\"form\" data-arg=\"keys-text\" spellcheck=\"false\" autocomplete=\"off\">{}</textarea><div class=\"row\"><button class=\"btn\" data-a=\"import-keys-text\">Import pasted keys</button><label class=\"btn\">Choose key file…<input type=\"file\" data-file=\"import-keys\" hidden></label></div></details>",
        esc(form(app, "keys-text"))
    ));
}

fn logged_out(app: &App, h: &mut String) {
    let device = host::storage_get(crate::accounts_flow::K_DEVICE).filter(|s| !s.is_empty()).is_some();
    let pk_here = host::storage_get(crate::accounts_flow::K_PK_HINT).as_deref() == Some("1");
    if let Some((name, _)) = &app.locked {
        h.push_str(&format!("<section class=\"card narrow\"><h2>Unlock “{}”</h2><p class=\"small muted\">This backup file is protected by its passphrase.</p>", esc(name)));
        h.push_str("<div class=\"row\"><input id=\"unlock-pass\" type=\"password\" placeholder=\"passphrase\" autocomplete=\"current-password\" data-in=\"form\" data-arg=\"unlock-pass\" data-enter=\"unlock-account\" aria-label=\"Passphrase\"><button class=\"btn primary\" data-a=\"unlock-account\">Unlock</button></div></section>");
    }
    h.push_str("<section class=\"card welcome\"><h1>Your free account</h1><p>Make one with a single tap — your face, fingerprint or Windows Hello is the login. No password, no app to install.</p>");
    h.push_str("<div class=\"row\"><button class=\"btn primary big\" data-a=\"passkey-create\">Create account</button>");
    h.push_str(&format!("<button class=\"btn big{}\" data-a=\"passkey-signin\">Sign in</button></div>", if pk_here { " primary-soft" } else { "" }));
    if device {
        h.push_str("<p><button class=\"link\" data-a=\"device-signin\">Continue with the account saved in this browser</button></p>");
    }
    h.push_str("<p class=\"small muted\">Your login works on your other devices too when they share the same Apple, Google or Microsoft account.</p></section>");
    h.push_str("<details class=\"adv\"><summary>Other ways to sign in</summary><div class=\"cols2\">");
    let remembered = host::storage_get(K_ACCOUNT).filter(|s| !s.is_empty()).and_then(|s| json::parse(&s).ok());
    h.push_str("<section class=\"card\"><h3>Backup file</h3>");
    if let Some(v) = remembered {
        h.push_str(&format!("<p class=\"small muted\">This browser remembers the backup “{}”.</p>", esc(&v.get("name").str_or("Account"))));
        h.push_str("<div class=\"row\"><input id=\"unlock-pass2\" type=\"password\" placeholder=\"passphrase\" autocomplete=\"current-password\" data-in=\"form\" data-arg=\"unlock-pass\" data-enter=\"unlock-remembered\" aria-label=\"Passphrase\"><button class=\"btn\" data-a=\"unlock-remembered\">Unlock</button></div><p class=\"small\"><button class=\"link danger\" data-a=\"forget-device\">Forget it on this device</button></p>");
    }
    h.push_str("<label class=\"drop\"><span class=\"big\">Drop your backup file</span><span class=\"muted small\">anywhere on this page — or click to choose it. Solana key files work too.</span><input type=\"file\" data-file=\"account-file\" hidden></label>");
    keys_import(h, app);
    h.push_str("</section><section class=\"card\"><h3>Without a passkey</h3><p class=\"small\">Keep the account in this browser only. Quick, but clearing the browser's data loses it — download a backup once it holds money.</p><button class=\"btn\" data-a=\"browser-account\">Use this browser</button>");
    h.push_str("<h3 style=\"margin-top:1rem\">Password-protected file</h3><div class=\"formgrid\">");
    h.push_str(&format!("<label>Account name<input id=\"acct-name\" value=\"{}\" placeholder=\"e.g. parts-catalog\" data-in=\"form\" data-arg=\"acct-name\" maxlength=\"40\"></label>", esc(form(app, "acct-name"))));
    h.push_str("<label>Passphrase (10+ characters)<input id=\"pass1\" type=\"password\" autocomplete=\"new-password\" data-in=\"form\" data-arg=\"pass1\"></label>");
    h.push_str("<label>Repeat passphrase<input id=\"pass2\" type=\"password\" autocomplete=\"new-password\" data-in=\"form\" data-arg=\"pass2\" data-enter=\"create-account\"></label>");
    h.push_str("</div><div class=\"row\"><button class=\"btn\" data-a=\"create-account\">Create with a file</button></div><p class=\"small muted\">Downloads an encrypted file (the IQ SDK's own <code>passwordEncrypt</code> scheme). The file plus passphrase is the login.</p></section></div></details>");
}

fn logged_in(app: &App, h: &mut String) {
    let a = app.account.as_ref().unwrap();
    let Some(main) = a.main() else {
        advanced(app, h);
        return;
    };
    let m = main.address();
    let bal = balance_text(app, &m);
    let others: u64 = a.addresses().iter().filter(|w| **w != m).filter_map(|w| app.balances.get(w).and_then(|b| b.ready().copied())).sum();
    h.push_str("<h1>Your account</h1>");
    h.push_str(&rpc_hint(app));
    h.push_str(&format!("<section class=\"card balcard\"><div class=\"muted small\">Balance</div><div class=\"huge\">{}</div>", bal));
    if others > 0 {
        h.push_str(&format!("<div class=\"small muted\">+ {} held by your databases for saving</div>", ui::sol(others)));
    }
    h.push_str(&format!(
        "<div class=\"row\"><button class=\"btn {} big\" data-a=\"panel\" data-arg=\"add\">Add funds</button><button class=\"btn {} big\" data-a=\"panel\" data-arg=\"send\">Send</button><button class=\"link\" data-a=\"refresh-balances\">refresh</button></div>",
        if app.panel == "add" { "primary" } else { "" },
        if app.panel == "send" { "primary" } else { "" }
    ));
    if app.panel == "add" {
        h.push_str(&format!(
            "<div class=\"panel\"><p>Send SOL to this address from an exchange (Coinbase, Kraken…) or another wallet. It shows up here within seconds.</p><div class=\"addrbox\"><span class=\"mono\">{a}</span><button class=\"btn\" data-a=\"copy\" data-arg=\"{a}\">Copy address</button></div>{}{}</div>",
            crate::qr::svg(&format!("solana:{}", m)).unwrap_or_default(),
            if app.settings.cluster == "devnet" { format!("<div class=\"row\"><button class=\"btn\" data-a=\"airdrop\" data-arg=\"{}\">Get 1 free test SOL (devnet)</button></div>", esc(&m)) } else { String::new() },
            a = esc(&m)
        ));
    }
    if app.panel == "send" {
        h.push_str("<div class=\"panel\">");
        match &app.send_review {
            Some(r) => {
                h.push_str(&format!(
                    "<p class=\"big\">Send {} to {}?</p><p class=\"small muted\">Network fee 0.000005 SOL. Payments can't be undone.</p><div class=\"row\"><button class=\"btn primary\" data-a=\"send-confirm\">Send now</button><button class=\"btn\" data-a=\"send-cancel\">Back</button></div>",
                    ui::sol(r.lamports),
                    esc(&r.label)
                ));
            }
            None => {
                h.push_str(&format!(
                    "<div class=\"formgrid\"><label>To (address or name.sol)<input id=\"send-to\" value=\"{}\" placeholder=\"alice.sol or a Solana address\" data-in=\"form\" data-arg=\"send-to\" spellcheck=\"false\" autocomplete=\"off\"></label><label>Amount in SOL<input id=\"send-amt\" value=\"{}\" placeholder=\"0.1 or max\" inputmode=\"decimal\" data-in=\"form\" data-arg=\"send-amt\" data-enter=\"send-review\"></label></div><div class=\"row\"><button class=\"btn primary\" data-a=\"send-review\">Review</button>{}</div>",
                    esc(form(app, "send-to")),
                    esc(form(app, "send-amt")),
                    app.busy_note.as_ref().map(|n| format!("<span class=\"muted small\">{}</span>", esc(n))).unwrap_or_default()
                ));
            }
        }
        h.push_str("</div>");
    }
    h.push_str("</section>");
    // protecting the account
    match &a.origin {
        crate::account::Origin::Browser => {
            h.push_str("<section class=\"card warnbox\"><b>This account lives only in this browser.</b> If the browser's data is cleared, it's gone — including its money. Download a backup (protected by a passphrase you choose) and keep it somewhere safe.");
            backup_form(app, h);
            h.push_str("</section>");
        }
        crate::account::Origin::Passkey { .. } => {
            h.push_str("<details class=\"adv\"><summary>Download a backup</summary><p class=\"small\">Your passkey is your login. A backup file is a second way in — useful if you ever lose access to your passkey.</p>");
            backup_form(app, h);
            h.push_str("</details>");
        }
        crate::account::Origin::File => {}
    }
    h.push_str("<p><a href=\"#/mine\">Your tables and files →</a></p>");
    h.push_str("<details class=\"adv\"><summary>Advanced: wallets, keys and backup file</summary>");
    advanced(app, h);
    h.push_str("</details>");
    h.push_str("<p><button class=\"btn\" data-a=\"logout\">Sign out</button></p>");
}

fn backup_form(app: &App, h: &mut String) {
    let a = app.account.as_ref().unwrap();
    if a.seal.is_some() {
        h.push_str("<div class=\"row\"><button class=\"btn primary\" data-a=\"save-account\">Download backup</button></div>");
        return;
    }
    h.push_str("<div class=\"formgrid\"><label>Choose a passphrase (10+ characters)<input id=\"bk-pass1\" type=\"password\" autocomplete=\"new-password\" data-in=\"form\" data-arg=\"pass1\"></label><label>Repeat it<input id=\"bk-pass2\" type=\"password\" autocomplete=\"new-password\" data-in=\"form\" data-arg=\"pass2\" data-enter=\"set-passphrase\"></label></div><div class=\"row\"><button class=\"btn primary\" data-a=\"set-passphrase\">Download backup</button></div>");
}

fn advanced(app: &App, h: &mut String) {
    let a = app.account.as_ref().unwrap();
    let total = total_balance(app).map(ui::sol).unwrap_or_default();
    h.push_str(&format!(
        "<p class=\"muted\">{} · {} wallet(s) · {} total · {}</p>",
        esc(&a.name),
        a.wallets.len(),
        esc(&total),
        match &a.origin {
            crate::account::Origin::Passkey { .. } => "signed in with a passkey".to_string(),
            crate::account::Origin::Browser => "saved in this browser".to_string(),
            crate::account::Origin::File => if a.seal.is_some() { "<span class=\"good\">encrypted account file</span>".to_string() } else { "<span class=\"warn\">no passphrase yet</span>".to_string() },
        }
    ));
    if a.origin == crate::account::Origin::File && (a.dirty || app.unsaved_keys) {
        h.push_str(&format!(
            "<div class=\"card warnbox\"><b>Save your account file.</b> {} <button class=\"btn primary\" data-a=\"save-account\" {}>Save account file</button></div>",
            if app.unsaved_keys {
                "Imported keys live only in the file — until you save it, they exist only in this tab."
            } else {
                "New wallets can always be recovered from the file you have, but labels and notes are only kept once you save."
            },
            if a.seal.is_none() { "disabled" } else { "" }
        ));
    }
    // wallets
    h.push_str("<section class=\"card\"><div class=\"tablehead\"><h3>Wallets</h3><span class=\"grow\"></span><button class=\"link\" data-a=\"refresh-balances\">refresh balances</button></div>");
    h.push_str("<div class=\"scroll\"><table class=\"grid\"><thead><tr><th>Label</th><th>Address</th><th class=\"num\">Balance</th><th>Used by</th><th></th></tr></thead><tbody>");
    for w in &a.wallets {
        let ad = w.address();
        let used: Vec<String> = app
            .drafts
            .iter()
            .filter(|d| d.wallet.as_deref() == Some(ad.as_str()))
            .map(|d| format!("<a href=\"#/ws/{}\">{}</a>", esc(&d.key), esc(&d.name)))
            .chain(
                app.dbroots
                    .ready()
                    .map(|rs| rs.iter().filter(|r| r.creator == ad && !app.drafts.iter().any(|d| d.name == r.name())).map(|r| format!("<a href=\"#/db/{}\">{}</a>", esc(&r.pda), esc(&r.name()))).collect::<Vec<_>>())
                    .unwrap_or_default(),
            )
            .collect();
        let kind = match w.kind {
            Kind::Derived(i) => format!("<span class=\"muted small\" title=\"Derived from the account's master secret\">#{}</span>", i),
            Kind::Imported => "<span class=\"pill\" title=\"Imported key, stored in the account\">imported</span>".into(),
        };
        h.push_str(&format!(
            "<tr><td><input id=\"wl-{a}\" class=\"inline\" value=\"{}\" data-in=\"wallet-label\" data-arg=\"{a}\" aria-label=\"Wallet label\"> {}</td><td><span class=\"addr\" title=\"{a}\">{}</span> <button class=\"link\" data-a=\"copy\" data-arg=\"{a}\">copy</button></td><td class=\"num\">{}</td><td class=\"small\">{}</td><td>",
            esc(&w.label),
            kind,
            esc(&solana::short(&ad)),
            balance_text(app, &ad),
            if used.is_empty() { "<span class=\"muted\">—</span>".to_string() } else { used.join(", ") },
            a = esc(&ad)
        ));
        let others: String = a
            .wallets
            .iter()
            .filter(|o| o.address() != ad)
            .map(|o| {
                let oa = o.address();
                format!("<option value=\"{}\" {}>{} ({})</option>", esc(&oa), if form(app, &format!("to:{}", ad)) == oa { "selected" } else { "" }, esc(&o.label), esc(&solana::short(&oa)))
            })
            .collect();
        h.push_str(&format!(
            "<details><summary>Send · key</summary><div class=\"formgrid small\"><label>To one of your wallets<select data-in=\"form\" data-arg=\"to:{a}\"><option value=\"\">—</option>{}</select></label><label>…or any address<input id=\"to2-{a}\" value=\"{}\" data-in=\"form\" data-arg=\"to2:{a}\" spellcheck=\"false\"></label><label>Amount (SOL, or \"all\")<input id=\"amt-{a}\" value=\"{}\" inputmode=\"decimal\" data-in=\"form\" data-arg=\"amt:{a}\" data-enter=\"transfer\"></label></div><div class=\"row\"><button class=\"btn primary\" data-a=\"transfer\" data-arg=\"{a}\">Send</button>{}</div>",
            others,
            esc(form(app, &format!("to2:{}", ad))),
            esc(form(app, &format!("amt:{}", ad))),
            if app.settings.cluster == "devnet" { format!("<button class=\"btn\" data-a=\"airdrop\" data-arg=\"{}\">Airdrop 1 devnet SOL</button>", esc(&ad)) } else { String::new() },
            a = esc(&ad)
        ));
        if app.reveal_key.as_deref() == Some(ad.as_str()) {
            h.push_str(&format!(
                "<p class=\"warn small\">Anyone with this key controls this wallet. It works in Phantom, Solflare or the Solana CLI.</p><pre class=\"secret\">{}</pre><button class=\"btn\" data-a=\"reveal-key\" data-arg=\"{}\">Hide</button>",
                esc(&w.kp.export_b58()),
                esc(&ad)
            ));
        } else {
            h.push_str(&format!("<button class=\"link\" data-a=\"reveal-key\" data-arg=\"{}\">Show secret key</button>", esc(&ad)));
        }
        if w.kind == Kind::Imported {
            h.push_str(&format!(" · <button class=\"link danger\" data-a=\"wallet-remove\" data-arg=\"{}\">remove from account</button>", esc(&ad)));
        }
        h.push_str("</details></td></tr>");
    }
    h.push_str("</tbody></table></div>");
    h.push_str(&format!(
        "<div class=\"row\"><input id=\"wlabel\" placeholder=\"label for a new wallet, e.g. db: fasteners\" value=\"{}\" data-in=\"form\" data-arg=\"wlabel\" data-enter=\"new-wallet\"><button class=\"btn primary\" data-a=\"new-wallet\">New wallet</button></div><p class=\"small muted\">New wallets are derived from your account's secret, so they can't be lost: signing in again finds them.</p>",
        esc(form(app, "wlabel"))
    ));
    keys_import(h, app);
    h.push_str("</section>");
    // file
    h.push_str("<section class=\"card\"><h3>Backup file</h3>");
    if a.seal.is_some() {
        h.push_str("<p class=\"small\">Encrypted with your passphrase — the IQ SDK's <code>passwordEncrypt</code> scheme (PBKDF2-SHA256 × 250,000 → AES-256-GCM).</p>");
        h.push_str("<div class=\"row\"><button class=\"btn primary\" data-a=\"save-account\">Download backup file</button></div>");
        h.push_str("<details><summary>Change passphrase</summary>");
    } else {
        h.push_str("<p class=\"small\">Set a passphrase to download an encrypted backup.</p><div>");
    }
    h.push_str("<div class=\"formgrid\"><label>New passphrase (10+ characters)<input id=\"pass1\" type=\"password\" autocomplete=\"new-password\" data-in=\"form\" data-arg=\"pass1\"></label><label>Repeat<input id=\"pass2\" type=\"password\" autocomplete=\"new-password\" data-in=\"form\" data-arg=\"pass2\" data-enter=\"set-passphrase\"></label></div><div class=\"row\"><button class=\"btn primary\" data-a=\"set-passphrase\">Set passphrase &amp; download</button></div>");
    h.push_str(if a.seal.is_some() { "</details>" } else { "</div>" });
    if a.origin == crate::account::Origin::File {
        h.push_str(&format!(
            "<label class=\"check\"><input type=\"checkbox\" data-in=\"remember\" {} {}> Keep the encrypted file in this browser, so next time only the passphrase is needed</label>",
            if app.remember { "checked" } else { "" },
            if a.seal.is_none() { "disabled" } else { "" }
        ));
    }
    h.push_str("<details><summary>Export without encryption</summary><p class=\"small warn\">An unencrypted file gives anyone who sees it control of every wallet in it.</p><button class=\"btn\" data-a=\"save-account\" data-arg=\"plain\">Download unencrypted file</button></details>");
    h.push_str("</section>");
}

// ---------------------------------------------------------------- my tables

pub fn mine(app: &App, h: &mut String) {
    h.push_str("<section class=\"hero\"><h1>My tables</h1>");
    let addrs: Vec<String> = app.account.as_ref().map(|a| a.addresses()).unwrap_or_default();
    match &app.account {
        Some(_) => h.push_str(&format!(
            "<p>Everything you've made — {} across your account. <a href=\"#/account\">Account →</a></p></section>",
            esc(&total_balance(app).map(ui::sol).unwrap_or_default())
        )),
        None => h.push_str("<p>Sign in to see the databases, tables and files you've made. Drafts in this browser are listed below either way.</p><a class=\"btn primary\" href=\"#/account\">Create account or sign in</a></section>"),
    }
    h.push_str(&rpc_hint(app));
    // on chain
    if !addrs.is_empty() {
        h.push_str("<h2>Databases you own</h2>");
        match &app.dbroots {
            Load::Ready(roots) => {
                let own: Vec<&net::DbRootInfo> = roots.iter().filter(|r| addrs.contains(&r.creator)).collect();
                // created from this browser but not in the (cached) list yet
                let fresh: Vec<&crate::state::Draft> = app
                    .drafts
                    .iter()
                    .filter(|d| d.root_sig.as_deref().map(|s| s != "existing").unwrap_or(false))
                    .filter(|d| d.wallet.as_ref().map(|w| addrs.contains(w)).unwrap_or(false))
                    .filter(|d| !own.iter().any(|r| r.name() == d.name))
                    .collect();
                if own.is_empty() && fresh.is_empty() {
                    h.push_str("<p class=\"muted\">None yet. Make one in the <a href=\"#/ws\">Editor</a> and save it.</p>");
                } else {
                    h.push_str("<div class=\"scroll\"><table class=\"grid\"><thead><tr><th>Database</th><th>Tables</th><th>Owner wallet</th><th class=\"num\">Wallet balance</th><th></th></tr></thead><tbody>");
                    for r in &own {
                        let tables: Vec<String> = r.tables.iter().take(8).map(|t| format!("<a href=\"#/t/{}/{}\">{}</a>", esc(&r.pda), esc(&t.pda), esc(&net::label_of(t)))).collect();
                        let more = if r.tables.len() > 8 { format!(" +{}", r.tables.len() - 8) } else { String::new() };
                        let draft = app.drafts.iter().find(|d| d.name == r.name());
                        h.push_str(&format!(
                            "<tr><td><a href=\"#/db/{}\"><b>{}</b></a></td><td class=\"small\">{}{}</td><td>{}</td><td class=\"num\">{}</td><td>{}</td></tr>",
                            esc(&r.pda),
                            esc(&r.name()),
                            if tables.is_empty() { "<span class=\"muted\">no tables</span>".to_string() } else { tables.join(", ") },
                            more,
                            who(app, &r.creator),
                            balance_text(app, &r.creator),
                            match draft {
                                Some(d) => format!("<a class=\"small\" href=\"#/ws/{}\">draft{}</a>", esc(&d.key), if d.ghosts() > 0 { format!(" · {} unsaved", d.ghosts()) } else { String::new() }),
                                None => String::new(),
                            }
                        ));
                    }
                    for d in &fresh {
                        h.push_str(&format!(
                            "<tr><td><a href=\"#/ws/{}\"><b>{}</b></a> <span class=\"pill\" title=\"Just created — the list refreshes within 30 minutes\">new</span></td><td class=\"small\">{}</td><td>{}</td><td class=\"num\">{}</td><td></td></tr>",
                            esc(&d.key),
                            esc(&d.name),
                            esc(&d.tables.iter().filter(|t| t.created.is_some()).map(|t| t.name.as_str()).collect::<Vec<_>>().join(", ")),
                            who(app, d.wallet.as_deref().unwrap_or("")),
                            balance_text(app, d.wallet.as_deref().unwrap_or(""))
                        ));
                    }
                    h.push_str("</tbody></table></div>");
                }
                h.push_str(&format!(
                    "<p class=\"small muted\">From {}. Each owner wallet is also the database's public donation address.</p>",
                    if app.use_rpc() { "Solana, read live" } else { "IQ's database list (refreshed every ~30 minutes)" }
                ));
            }
            Load::Err(e) => h.push_str(&format!("<div class=\"card bad\">Couldn't load the database list: {}</div>", esc(e))),
            _ => h.push_str("<div class=\"loading\">Loading databases…</div>"),
        }
    }
    // drafts
    h.push_str("<h2>Drafts in this browser</h2>");
    if app.drafts.is_empty() {
        h.push_str("<p class=\"muted\">No drafts. <a href=\"#/ws\">Start one →</a></p>");
    } else {
        h.push_str("<div class=\"scroll\"><table class=\"grid\"><thead><tr><th>Database</th><th>Status</th><th class=\"num\">Tables</th><th class=\"num\">Unsaved</th><th>Wallet</th></tr></thead><tbody>");
        for d in &app.drafts {
            let role = match (&d.wallet, app.name_checks.get(&d.key)) {
                (Some(w), Some(Load::Ready(Some(c)))) if c != w => "<span class=\"pill un\">contributor</span>",
                _ if d.root_sig.is_some() => "<span class=\"pill off\">on chain</span>",
                _ => "<span class=\"pill ghost\">not saved yet</span>",
            };
            h.push_str(&format!(
                "<tr><td><a href=\"#/ws/{}\">{}</a></td><td>{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td>{}</td></tr>",
                esc(&d.key),
                esc(&d.name),
                role,
                d.tables.len(),
                d.ghosts(),
                d.wallet.as_ref().map(|w| who(app, w)).unwrap_or_else(|| "<span class=\"muted\">not chosen</span>".into())
            ));
        }
        h.push_str("</tbody></table></div>");
    }
    // files
    if !addrs.is_empty() {
        h.push_str("<h2>Files</h2>");
        if app.use_rpc() {
            h.push_str("<p class=\"muted small\">File listings come from IQ's gateway, which serves mainnet. Switch Settings → Read tables from → IQ gateway to see them.</p>");
        } else {
            let mut rows = String::new();
            let mut loading = 0;
            let mut errs = vec![];
            for w in &addrs {
                match app.files.get(w) {
                    Some(Load::Ready(list)) => {
                        for f in list {
                            let name = if f.filename.is_empty() { solana::short(&f.sig) } else { f.filename.clone() };
                            rows.push_str(&format!(
                                "<tr><td><button class=\"link file\" data-a=\"open-tx\" data-arg=\"{}\" data-val=\"{}\">📎 {}</button></td><td class=\"small\">{}{}</td><td>{}</td><td class=\"small\">{}</td><td><button class=\"link\" data-a=\"copy\" data-arg=\"iq://tx/{}#{}\">copy link</button></td></tr>",
                                esc(&f.sig),
                                esc(&name),
                                esc(&name),
                                esc(&f.filetype),
                                if f.chunks > 1 { format!(" · {} chunks", f.chunks) } else { String::new() },
                                who(app, w),
                                f.time.map(ui::time).unwrap_or_default(),
                                esc(&f.sig),
                                esc(&crate::app::pct_encode(&name))
                            ));
                        }
                    }
                    Some(Load::Loading) => loading += 1,
                    Some(Load::Err(e)) => errs.push(e.clone()),
                    _ => {}
                }
            }
            if rows.is_empty() {
                h.push_str(if loading > 0 { "<div class=\"loading\">Loading files…</div>" } else { "<p class=\"muted\">No files yet. Put one in a cell with 📎 File in the Editor.</p>" });
            } else {
                h.push_str("<div class=\"scroll\"><table class=\"grid\"><thead><tr><th>File</th><th>Type</th><th>Wallet</th><th>Time</th><th></th></tr></thead><tbody>");
                h.push_str(&rows);
                h.push_str("</tbody></table></div>");
            }
            if !errs.is_empty() {
                h.push_str(&format!("<p class=\"small warn\">Some wallets' files couldn't be listed: {}</p>", esc(&errs[0])));
            }
        }
    }
}

// ------------------------------------------------------------- draft wallet

pub fn draft_wallet(app: &App, key: &str, h: &mut String) {
    let Some(d) = app.draft_idx(key).map(|i| &app.drafts[i]) else { return };
    let wallet = d.wallet.clone();
    let can_change = d.root_sig.is_none();
    h.push_str("<section class=\"card\"><h3>Database wallet</h3>");
    h.push_str(&rpc_hint(app));
    let pick = |h: &mut String, label: &str| {
        let Some(a) = app.account.as_ref() else { return };
        let opts: String = a
            .wallets
            .iter()
            .map(|w| {
                let ad = w.address();
                format!(
                    "<option value=\"{}\" {}>{} · {} · {}</option>",
                    esc(&ad),
                    if wallet.as_deref() == Some(ad.as_str()) { "selected" } else { "" },
                    esc(&w.label),
                    esc(&solana::short(&ad)),
                    app.balances.get(&ad).and_then(|b| b.ready().copied()).map(ui::sol).unwrap_or_default()
                )
            })
            .collect();
        h.push_str(&format!(
            "<label class=\"formgrid small\">{}<select data-in=\"draft-wallet\" data-arg=\"{}\"><option value=\"\">—</option>{}</select></label>",
            label,
            esc(key),
            opts
        ));
    };
    match (&app.account, &wallet, app.draft_keypair(key)) {
        (None, w, _) => {
            h.push_str("<p>Every database has a wallet that creates it on chain, signs its <em>official</em> rows and pays for them. Its address is also a public donation address.</p>");
            if let Some(w) = w {
                h.push_str(&format!("<p class=\"small\">This draft uses {}.</p>", addr(w)));
            }
            h.push_str("<a class=\"btn primary\" href=\"#/account\">Log in to continue</a>");
        }
        (Some(_), None, _) => {
            h.push_str("<p>Every database has a wallet that creates it on chain, signs its <em>official</em> rows and pays for them. Its address is also a public donation address, so a dedicated wallet per database keeps its funds and history separate.</p>");
            h.push_str(&format!("<div class=\"row\"><button class=\"btn primary\" data-a=\"draft-new-wallet\" data-arg=\"{}\">Create a dedicated wallet for “{}”</button></div>", esc(key), esc(&d.name)));
            pick(h, "…or use one of your wallets");
        }
        (Some(_), Some(w), None) => {
            h.push_str(&format!("<p class=\"warn\">This draft's wallet {} isn't in the account you're logged in with. Log in with the account that holds it{}.</p>", addr(w), if can_change { ", or pick another wallet" } else { "" }));
            if can_change {
                pick(h, "Use one of your wallets");
            }
        }
        (Some(_), Some(w), Some(_)) => {
            let label = wallet_label(app, w).unwrap_or_default();
            h.push_str(&format!(
                "<p><b>{}</b> signs everything for <b>{}</b> and is its <em>official</em> identity. Anyone can fund it by sending SOL to its address.</p>",
                esc(&label),
                esc(&d.name)
            ));
            h.push_str(&format!(
                "<div class=\"addrbox\"><span class=\"mono\">{a}</span><button class=\"link\" data-a=\"copy\" data-arg=\"{a}\">copy</button><a href=\"solana:{a}?label={}\">donation link</a><a href=\"https://browser.iqlabs.dev/{a}\" target=\"_blank\" rel=\"noopener\">IQ browser</a><a href=\"{}\" target=\"_blank\" rel=\"noopener\">Solscan</a></div>",
                esc(&crate::app::pct_encode(&format!("{} on IQ", d.name))),
                esc(&ui::solscan_account(w, &app.settings.cluster)),
                a = esc(w)
            ));
            h.push_str(&format!("<p class=\"big\">{} <button class=\"link\" data-a=\"balance\" data-arg=\"{}\">refresh</button></p>", balance_text(app, w), esc(w)));
            h.push_str(&crate::qr::svg(&format!("solana:{}", w)).unwrap_or_default());
            // top up from another account wallet
            if let Some(a) = app.account.as_ref() {
                let others: Vec<_> = a.wallets.iter().filter(|o| &o.address() != w).collect();
                if !others.is_empty() {
                    let sel = form(app, &format!("ffrom:{}", key));
                    let opts: String = others
                        .iter()
                        .map(|o| {
                            let oa = o.address();
                            format!(
                                "<option value=\"{}\" {}>{} · {}</option>",
                                esc(&oa),
                                if sel == oa { "selected" } else { "" },
                                esc(&o.label),
                                app.balances.get(&oa).and_then(|b| b.ready().copied()).map(ui::sol).unwrap_or_else(|| solana::short(&oa))
                            )
                        })
                        .collect();
                    h.push_str(&format!(
                        "<div class=\"row\"><select data-in=\"form\" data-arg=\"ffrom:{k}\" aria-label=\"Fund from\"><option value=\"\">fund from…</option>{}</select><input id=\"famt-{k}\" placeholder=\"0.1\" inputmode=\"decimal\" value=\"{}\" data-in=\"form\" data-arg=\"famt:{k}\" aria-label=\"Amount in SOL\" style=\"max-width:110px\"><button class=\"btn\" data-a=\"fund-from\" data-arg=\"{k}\">Move SOL</button></div>",
                        opts,
                        esc(form(app, &format!("famt:{}", key))),
                        k = esc(key)
                    ));
                }
            }
            if app.settings.cluster == "devnet" {
                h.push_str(&format!("<div class=\"row\"><button class=\"btn\" data-a=\"airdrop\" data-arg=\"{}\">Airdrop 1 devnet SOL</button></div>", esc(w)));
            }
            if can_change {
                h.push_str("<details><summary>Use a different wallet</summary>");
                pick(h, "Wallet");
                h.push_str(&format!("<button class=\"link\" data-a=\"draft-new-wallet\" data-arg=\"{}\">or create a new dedicated one</button></details>", esc(key)));
            }
        }
    }
    h.push_str("</section>");
}

/// Is this route one of the account's own pages?
pub fn is_account_route(r: &Route) -> bool {
    matches!(r, Route::Mine | Route::Account)
}

pub fn root_pda(name: &str) -> String {
    b58(&iq::db_root_pda(name.as_bytes()))
}
