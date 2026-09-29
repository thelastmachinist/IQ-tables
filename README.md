# IQ Tables

A database portal for [IQ Labs](https://iqlabs.dev) on-chain tables — browse every table on IQ, draft new databases as **ghost data**, and inscribe them to Solana when they're ready. Built to be hosted on IQ's own on-chain web (IQ Pages).

- **Explore** every IQ database and table through IQ's gateway, with sorting, filtering, CSV/JSON export, and an **official / unofficial** filter (official = written by the database's own wallet).
- **My tables**: everything your account has made in one place — databases your wallets own (with their tables and balances), drafts in this browser, and files your wallets inscribed.
- **One-tap accounts**: "Create account" makes a wallet protected by a passkey (Face ID, Touch ID, Windows Hello or a phone). No wallet extension, no seed phrase, no name or email. Signing in on another device with the same passkey brings the same wallet back.
- **Money in plain words**: *Add funds* shows the address and a QR code to send SOL to from an exchange; *Send* takes an address or a `name.sol`. Each database gets its own wallet automatically and is topped up from your balance when you save — people never have to manage wallets unless they open *Advanced*.
- **Editor**: a spreadsheet (click a cell and type, Tab/Enter/arrows, copy and paste from Excel or Google Sheets, undo/redo, sort, filter, column menus) with phpMyAdmin-style tabs: *Browse*, *Structure*, *SQL*, *Import & export*, *Save*. Changes are held as ghost data until you press **Save to blockchain**, which shows the cost first.
- **SQL console**: `SELECT` (WHERE, GROUP BY, ORDER BY, LIMIT, aggregates), `INSERT`, `UPDATE`, `DELETE`, `CREATE TABLE`, `ALTER TABLE`, `DROP TABLE`, `SHOW TABLES`, `SHOW CHANGES`, `DESCRIBE`, `COMMIT`, `ROLLBACK` — run against the saved rows plus your unsaved changes; `COMMIT` saves.
- **Power users** can still use an encrypted account file holding many wallets (drop it anywhere on the page to sign in), import existing keys (Solana CLI `id.json`, base58 secret keys) and move SOL between wallets.
- **Links and files in cells**: `https://…`, `name.sol` (opens in IQ's browser), `iq://table/<table>/<record>`, `iq://db/<database>` and `iq://tx/<signature>` are clickable. Attach a small file to a row and it's inscribed with IQ's own file instruction, then linked from the cell.
- **Packing + compression**: hundreds of records per inscription, so a 0.001 SOL write carries a whole page of data.
- **Edit live tables**: open a saved table in the Editor, change or delete records, and only the changes are written.

Everything is **dependency-free Rust** compiled to WebAssembly: SHA-256/512, Keccak-256, Ed25519, PBKDF2, AES-256-GCM, Base58, Solana transaction encoding (legacy and v1), the IQ program's instructions, JSON, a context-mixing compressor, a QR encoder and the UI. The only JavaScript is `web/host.js`, a ~360-line bridge to the DOM, `fetch`, `localStorage`, passkeys (WebAuthn) and files, because browsers can't run WebAssembly without it.

## Status

Prototype. The Rust core is checked byte-for-byte against the official SDK (`@iqlabs-official/solana-sdk` 0.2.0), the whole app passes an end-to-end test against a mock chain that validates every instruction with the program's IDL, and its transactions have been dry-run against IQ's deployed program on devnet (see [Checked against the real program](#checked-against-the-real-program)). **It has not yet written to mainnet.** See [Before using it on mainnet](#before-using-it-on-mainnet).

## Deploy to IQ Pages

`site/` holds the built app: one self-contained `index.html` (about 1.5 MB, WebAssembly embedded) and `iqpages.json`. With IQ's git CLI (`npm install -g @iqlabs-official/iq-git-cli`):

```bash
cd site
iqgit init
iqgit create iq-tables --public
iqgit add .
iqgit commit -m "IQ Tables v0.1.0"
iqgit push
iqgit pages deploy
```

`iqgit pages deploy` prints the live link (`browser.iqlabs.dev/<commit-table-address>`). To give it a stable name, point a `.sol` domain at it (the CLI prints the steps). A dedicated domain also gives the portal its own browser origin, which matters because every IQ Pages site otherwise shares `browser.iqlabs.dev` — its browser storage, and the domain passkeys are tied to. Give the portal its own domain before real users create passkey accounts: a passkey made on one domain can't be used from another.

## Run it locally (PyCharm or any terminal)

`run.py` (Python standard library only) serves the app at `http://localhost:8000` and opens your browser. If you've changed the Rust or web sources and Rust is installed, it rebuilds first. In PyCharm: right-click `run.py` → **Run 'run'**. Stop it with the red square.

```bash
python run.py              # rebuild if needed, then serve
python run.py --no-build   # serve the committed build as is
```

## Build

```bash
rustup target add wasm32-unknown-unknown
./build.sh            # writes site/index.html, site/iqpages.json, dist/multi/
cargo test --release  # 22 unit tests, incl. byte-for-byte SDK comparisons
```

No crates are used. If the wasm target can't be installed but `rust-src` can, `BUILD_STD=1 ./build.sh` builds the standard library from source.

## How it works

### Accounts and wallets
- **Passkey accounts** (the default). The page asks the device for a passkey with the WebAuthn **PRF** extension, which returns a secret only that passkey can produce. The account's master secret is derived from it (`SHA-256("iq-tables/passkey-master/v1" ‖ prf)`), so there is nothing to store or back up: the same passkey on any device — synced by iCloud Keychain, Google Password Manager or a password manager — gives the same wallets. The list of wallet names is kept in the browser, encrypted with a second key from the same passkey; on a new device the page rescans derived wallets for on-chain activity instead. *Download a backup* writes the usual encrypted account file for anyone who wants one.
- If the device has no passkey support, **Use this browser** keeps the account in this browser's storage only (and says so), with a nudge to download a backup once it holds money.
- **Add funds** shows the main wallet's address and QR code; **Send** accepts an address or a `.sol` name (resolved through IQ's gateway `/sns/<name>`), and shows a review step before anything moves. Buying SOL with a card needs a licensed on-ramp, which the portal doesn't embed.
- **Databases get their own wallet automatically** ("db: <name>"), derived from the account. Saving checks its balance and moves what's needed (estimate + 10%) from the main balance first; file attachments do the same. Balances held by database wallets are shown on the account page.
- An **account file** (`<name>.iqaccount.json`) holds a 32-byte master secret, wallet labels, and any imported keys. It's encrypted with the IQ SDK's own `passwordEncrypt` scheme — PBKDF2-SHA256 × 250,000 → AES-256-GCM — so the SDK's `passwordDecrypt` opens it too (the test suite checks this both ways). Nothing is stored on a server; keys exist in the page only while you're logged in.
- **Log in** by dropping the file anywhere on the page (or choosing it) and typing the passphrase. Optionally the page keeps the *encrypted* file in this browser so next time only the passphrase is needed.
- **New wallets** are derived: `seed(i) = SHA-256("iq-tables/account/v1/wallet" ‖ master ‖ u32le(i))`. A wallet made after your last save can't be lost — logging in rescans the next indices for on-chain activity and recovers them. **The domain string is frozen**; changing it would change every derived address.
- **Imported keys** (Solana CLI keypair arrays, lists of them, or text with one base58 secret key per line, optionally `label: key`) are stored in the file, so save it after importing — the page won't let you log out with unsaved imports.
- **Unencrypted** account files can be exported and dropped too (clearly marked), for people who want to manage keys themselves.
- Each **database** has a wallet: usually a dedicated one ("Create a dedicated wallet for …"). It creates the DbRoot (so it is the creator and the *official* signer), optionally locks table creation to itself, creates tables (each **locked** = only it may write, or **open** = anyone, shown as unofficial), and signs every pack. Its address is a public donation address with a QR code; "Move SOL" tops it up from any other wallet in the account.
- Contributors use the same flow with their own account; their rows show as unofficial.

### Editor
- The sheet shows the saved rows (read from the chain, filtered to the database's own wallet and yours) with your unsaved changes on top: new rows are green, edited cells amber, deleted rows struck through. The Save tab and the bar above the sheet count them and show the cost.
- Excel keys: type to replace, Enter/F2 to edit in place, Tab/Enter to move, arrows and Shift+arrows, Ctrl+C / Ctrl+V (tab-separated, so pasting from Excel or Sheets adds rows and columns as needed), Delete, Ctrl+Z / Ctrl+Y (a paste undoes in one step). Column menus sort, rename, set the ID column and delete columns; once a table has saved data, only adding columns is offered, since renaming would disconnect saved rows.
- *Structure* holds table options (open to everyone / locked to the owner, compression) and columns; *Import & export* takes CSV/JSON and downloads the sheet; *Save* shows exactly what will be written and runs it with progress.
- **SQL** (`src/sql.rs`, `src/sql_exec.rs`) is a small SQL dialect over the same model: reads see saved + unsaved rows; writes become unsaved changes like any edit, so `ROLLBACK`, Undo and the Save tab all work on them. Text comparisons are case-insensitive. `DROP TABLE` only removes tables that were never saved — on-chain data is permanent, and `DELETE` writes tombstones that hide rows rather than erasing them.

### Packs
A packed table has two on-chain columns, `id` and `p`. Each on-chain row is a *pack* of many records:

```
{"id": "<pack id>", "p": "IQT1z<compressed text>"}
```

- Records are laid out column by column (all values of column 1, then column 2, …) so similar values sit together.
- The layout is compressed by `src/codec.rs`: a bitwise context-mixing compressor (order 0–5 contexts, a match model, a logistic mixer and two SSE stages). On a 120-record parts catalog it beats `gzip -9` by 33% and Brotli-11 by 18%.
- The bytes are written with a 92-character alphabet (printable ASCII minus `"` and `\`), 13 bits per 2 characters, so the text never needs escaping inside the JSON-in-JSON that IQ stores.
- A pack fills one transaction: the metadata JSON must be ≤ 3,400 bytes with v1 transactions (700 with legacy ones). The planner packs as many records as fit.
- Every pack carries its own column list, so a table can gain columns later and old packs still decode.
- Records are keyed by the table's id column. Newer packs replace older records with the same id; deletions are tombstone records. Pack ids are content hashes, so an accidental double inscription changes nothing.
- `IQT1j<json>` is the uncompressed variant (readable and searchable by the gateway, about 5× more writes).

In the end-to-end test, 600 randomized parts records packed into 4 inscriptions (about 150 per pack).

### Links and files
- Any cell whose value is a link renders as one: web links open in a new tab; `name.sol` opens `browser.iqlabs.dev/name.sol`; other addresses open in IQ's browser; `iq://table/<table>` / `iq://table/<table>/<record id>` and `iq://db/<database>` open in the explorer (with "database › table › record" as the label); `iq://tx/<signature>` and bare signatures open the inscription viewer.
- Every table, database and record has a **copy link** button, so linking a row to a record in another table is copy → paste.
- **Attaching a file** (📎 on a ghost row) inscribes it with `user_inventory_code_in`, exactly like the SDK's `codeIn` direct path: same metadata, text files stored as text, binary as base64. The cell gets `iq://tx/<signature>#<filename>`. Single-transaction files only for now: up to ~2.4 KB binary / ~3.2 KB text (~500 bytes until v1 transactions are live). Cost: 0.001 SOL, plus the wallet's one-time IQ setup if it's new.
- **Opening a file** asks IQ's gateway first (`/data/<signature>`) and falls back to reading the transaction from Solana. Images preview inline, text shows as text, packs decode to their records, anything can be downloaded. "My tables" lists every file your wallets inscribed (IQ gateway `/user/<wallet>/assets`), including ones made with other IQ tools.

### Inscribing
Each step (create DbRoot, create table, one-time account setup, each pack) is **simulated first** through the RPC: program errors and the exact cost appear before any SOL moves. The step is then sent and polled until confirmed. Progress is saved after every step, so closing the tab and pressing Resume continues where it left off.

Like the SDK, Auto mode uses v1 transactions only when the cluster's v1 feature gate (`txv1aq4…GLL`) is active, and before the first v1-sized write it grows a wallet's IQ accounts if they were made by the pre-upgrade program (`realloc_account` to 4,213 / 4,215 bytes).

### Reading
By default tables are read through IQ's gateway (fast, cached, with search and files). Settings → *Read tables from → Solana directly* reads live instead: the database list via `getProgramAccounts`, table metadata via `getAccountInfo`, and rows by walking the table's transaction history (`getSignaturesForAddress` + batched `getTransaction`) and decoding each inline `db_code_in`. Devnet always reads this way, since the gateway serves mainnet.

### Costs (measured against IQ's program on devnet)
| Item | Cost |
|---|---|
| Each pack or file (direct write) | 0.001 SOL program fee + 0.000005 SOL network fee |
| First write from a new wallet | ~0.05 SOL one-time rent (IQ user accounts: 4,213 + 4,215 bytes + user state) |
| Create a database | ~0.0115 SOL rent (2,133-byte account) |
| Create a table | ~0.015 SOL rent + 0.00093 SOL IQ table-creation fee |

Rent stays locked in the accounts; only the fees are spent. The simulation before each step shows the exact amount on the cluster you're using.

## Source map
| File | What |
|---|---|
| `src/crypto/` | SHA-256/512, Keccak-256, Base58, Ed25519 (TweetNaCl port), PBKDF2 + AES-256-GCM |
| `src/solana.rs` | PDAs, message compilation, legacy + v1 wire formats, transaction parsing |
| `src/iq.rs` | IQ program: seeds, PDAs, instruction encoders and decoders, account decoders |
| `src/codec.rs` | Compressor + text-safe encoding |
| `src/pack.rs` | Pack layout, planner, merge (latest wins, tombstones) |
| `src/account.rs` | Accounts: derivation, passkey and file origins, encryption, key import formats |
| `src/accounts_flow.rs` | Passkeys, sign in/out, Add funds / Send, wallets, transfers, rescans, airdrops |
| `src/sheet.rs` | Spreadsheet model: saved rows + unsaved changes, cell edits, paste, columns |
| `src/editor.rs` | Editor state: selection, keys, undo/redo, save with automatic top-up |
| `src/sql.rs`, `src/sql_exec.rs` | SQL tokenizer, parser and evaluator; running statements against a database |
| `src/app.rs` | State, routing, events, explorer and workspace flows |
| `src/chain.rs` | Reading databases and rows straight from Solana |
| `src/attach.rs` | Inscribing files into cells; the inscription viewer |
| `src/inscribe.rs` | Simulate → send → confirm pipeline |
| `src/views.rs`, `src/views_account.rs`, `src/views_ws.rs` | HTML rendering (explorer, account, editor) |
| `src/qr.rs` | QR encoder for donation addresses |
| `web/host.js` | The browser bridge (DOM, fetch, storage, passkeys, keys and paste, files, drag and drop) |
| `tools/` | Reference fixtures from the official SDK and the end-to-end test (dev only) |

## Tests
- `cargo test --release` (22 tests): hashes, Base58, Ed25519 signatures and 150 random PDAs against the SDK and noble; every instruction (`initialize_db_root`, `manage_table_creators`, `create_table` open/locked, `user_initialize`, `db_code_in`, `user_inventory_code_in`, `realloc_account`) byte-for-byte against the SDK's builder; full v1 transactions byte-identical to the SDK's `buildV1Transaction`; PBKDF2 and AES-GCM against Node; opening the SDK's `passwordEncrypt` output; parsing our own transactions back; account file round-trips and key-import formats; JSON escaping identical to `JSON.stringify`; codec and pack round-trips; merge rules; the spreadsheet model (edits, paste, saved-column rules); the SQL dialect against a draft database.
- `cd tools && npm install && CHROME_PATH=/path/to/chrome npm run e2e` (89 checks): the built page in headless Chromium against a mock chain that verifies every signature (both wire formats), decodes every instruction with the program's IDL and compares its accounts and data with the SDK's builder, plus a mock IQ gateway and a virtual passkey authenticator. Covers: explore, search, HTML escaping; one-tap passkey accounts (nothing stored in the clear, same wallet on sign-in and on a fresh device), Add funds, sending to a `.sol` name; the spreadsheet (typing, Tab/Enter, paste from Excel, undo/redo, CSV import of 600 rows); SQL (DROP/CREATE/INSERT, WHERE/ORDER BY, GROUP BY, DELETE, SHOW CHANGES, COMMIT); saving with automatic database wallets and top-up; creating a file account (file opened with the SDK's `passwordDecrypt`, derivation checked independently); importing a CLI key file and pasted keys; refusing logout with unsaved keys; logging in by drag-and-drop; wrong passphrase; recovering a wallet made after the last save; "remember on this device"; dedicated database wallets and moving SOL; CSV import and packing; attaching a file (wallet setup + `user_inventory_code_in`); inscription with DbRoot realloc; locked vs open tables; web, record and file links; the file viewer and download; record links; My tables; editing a live record; unofficial contributions; growing pre-upgrade accounts before a v1 write; rejecting writes to a locked table at simulation; reading everything back straight from Solana with batched RPC; devnet airdrops; mobile layout.
- `npm run qr`: decodes the generated QR with jsQR.

## Checked against the real program
IQ's program is deployed on devnet, and devnet has v1 transactions switched on. The portal's own Rust code built these transactions, which were run through `simulateTransaction` on devnet (signature checks off, so nothing was signed or sent):

- **One v1 transaction doing the whole flow** — `initialize_db_root`, `manage_table_creators`, `create_table` (locked), `user_initialize`, `db_code_in` with a 1,486-byte pack, `user_inventory_code_in` with a text file: **succeeded**. Every instruction encoding matches the deployed program.
- **Writer locks are enforced on chain**: an outsider's `db_code_in` to a locked table fails with the program's `NotAuthorized` (6000); the same write to an open table succeeds.
- `user_initialize` creates full-size (post-upgrade) accounts on devnet, so no resize is needed for new wallets there.
- The fees and sizes in the cost table above come from these runs.
- The deployed app itself was loaded in a real browser on devnet: it read balances and the database list (`getProgramAccounts`) straight from Solana.

Still to do: a funded end-to-end run (the public devnet faucet was dry at the time), and mainnet.

## Before using it on mainnet
1. **Its own domain**, so passkeys and browser storage belong to the portal alone (see [Deploy](#deploy-to-iq-pages)).
2. **An RPC that accepts browser requests.** Solana's public mainnet endpoint answers browsers with `403 Access forbidden`, so balances, transfers and inscriptions need your own RPC URL in Settings (a free Helius, QuickNode or Triton key works). Reading tables doesn't need one — that goes through IQ's gateway.
3. **v1 on mainnet**: Auto mode checks the v1 feature gate and uses legacy transactions (700-byte packs, ~5× more writes) until it's active.
4. A first small real run (one database, one table, one pack, ~0.1 SOL) — the simulation step shows any program error and the exact cost before anything is sent.

## License
MIT — see LICENSE.
