# IQ Tables

A database portal for [IQ Labs](https://iqlabs.dev) on-chain tables — browse every table on IQ, draft new databases as **ghost data**, and inscribe them to Solana when they're ready. Built to be hosted on IQ's own on-chain web (IQ Pages).

- **Explore** every IQ database and table through IQ's gateway, with sorting, filtering, CSV/JSON export, and an **official / unofficial** filter (official = written by the database's own wallet).
- **My tables**: everything your account has made in one place — databases your wallets own (with their tables and balances), drafts in this browser, and files your wallets inscribed.
- **Accounts without a wallet extension**: one encrypted account file holds all your wallets. Drop it anywhere on the page, type the passphrase, and you're logged in to every wallet in it. Make a dedicated wallet per database or project, move SOL between them, import existing keys (Solana CLI `id.json`, base58 secret keys).
- **Workspace**: build tables and rows locally (type them in, paste CSV/JSON, or upload a file). Nothing costs anything until you inscribe.
- **Links and files in cells**: `https://…`, `name.sol` (opens in IQ's browser), `iq://table/<table>/<record>`, `iq://db/<database>` and `iq://tx/<signature>` are clickable. Attach a small file to a row and it's inscribed with IQ's own file instruction, then linked from the cell.
- **Packing + compression**: hundreds of records per inscription, so a 0.001 SOL write carries a whole page of data.
- **Draft changes to live tables**: open an inscribed table in the workspace, edit or delete records, and inscribe only the changes.

Everything is **dependency-free Rust** compiled to WebAssembly: SHA-256/512, Keccak-256, Ed25519, PBKDF2, AES-256-GCM, Base58, Solana transaction encoding (legacy and v1), the IQ program's instructions, JSON, a context-mixing compressor, a QR encoder and the UI. The only JavaScript is `web/host.js`, a ~230-line bridge to the DOM, `fetch`, `localStorage` and files, because browsers can't run WebAssembly without it.

## Status

Prototype. The Rust core is checked byte-for-byte against the official SDK (`@iqlabs-official/solana-sdk` 0.2.0), and the whole app passes an end-to-end test against a mock chain that validates every instruction with the program's IDL. **It has not yet written to mainnet.** See [Before using it on mainnet](#before-using-it-on-mainnet).

## Deploy to IQ Pages

`site/` holds the built app: one self-contained `index.html` (about 1 MB, WebAssembly embedded) and `iqpages.json`. With IQ's git CLI (`npm install -g @iqlabs-official/iq-git-cli`):

```bash
cd site
iqgit init
iqgit create iq-tables --public
iqgit add .
iqgit commit -m "IQ Tables v0.1.0"
iqgit push
iqgit pages deploy
```

`iqgit pages deploy` prints the live link (`browser.iqlabs.dev/<commit-table-address>`). To give it a stable name, point a `.sol` domain at it (the CLI prints the steps). A dedicated domain also gives the portal its own browser origin, which matters because every IQ Pages site otherwise shares `browser.iqlabs.dev` — including its browser storage (where "remember this account on this device" keeps the encrypted account file).

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
cargo test --release  # 18 unit tests, incl. byte-for-byte SDK comparisons
```

No crates are used. If the wasm target can't be installed but `rust-src` can, `BUILD_STD=1 ./build.sh` builds the standard library from source.

## How it works

### Accounts and wallets
- An **account file** (`<name>.iqaccount.json`) holds a 32-byte master secret, wallet labels, and any imported keys. It's encrypted with the IQ SDK's own `passwordEncrypt` scheme — PBKDF2-SHA256 × 250,000 → AES-256-GCM — so the SDK's `passwordDecrypt` opens it too (the test suite checks this both ways). Nothing is stored on a server; keys exist in the page only while you're logged in.
- **Log in** by dropping the file anywhere on the page (or choosing it) and typing the passphrase. Optionally the page keeps the *encrypted* file in this browser so next time only the passphrase is needed.
- **New wallets** are derived: `seed(i) = SHA-256("iq-tables/account/v1/wallet" ‖ master ‖ u32le(i))`. A wallet made after your last save can't be lost — logging in rescans the next indices for on-chain activity and recovers them. **The domain string is frozen**; changing it would change every derived address.
- **Imported keys** (Solana CLI keypair arrays, lists of them, or text with one base58 secret key per line, optionally `label: key`) are stored in the file, so save it after importing — the page won't let you log out with unsaved imports.
- **Unencrypted** account files can be exported and dropped too (clearly marked), for people who want to manage keys themselves.
- Each **database** has a wallet: usually a dedicated one ("Create a dedicated wallet for …"). It creates the DbRoot (so it is the creator and the *official* signer), optionally locks table creation to itself, creates tables (each **locked** = only it may write, or **open** = anyone, shown as unofficial), and signs every pack. Its address is a public donation address with a QR code; "Move SOL" tops it up from any other wallet in the account.
- Contributors use the same flow with their own account; their rows show as unofficial.

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

### Costs (from IQ Labs' own cost model)
| Item | Cost |
|---|---|
| Each pack or file (direct write) | 0.001 SOL program fee + 0.000005 SOL network fee |
| First write from a new wallet | ~0.062 SOL one-time rent (IQ user accounts) |
| Create a database | ~0.002–0.003 SOL rent |
| Create a table | rent + IQ's table-creation fee (not published; the simulation shows the exact amount before sending) |

## Source map
| File | What |
|---|---|
| `src/crypto/` | SHA-256/512, Keccak-256, Base58, Ed25519 (TweetNaCl port), PBKDF2 + AES-256-GCM |
| `src/solana.rs` | PDAs, message compilation, legacy + v1 wire formats, transaction parsing |
| `src/iq.rs` | IQ program: seeds, PDAs, instruction encoders and decoders, account decoders |
| `src/codec.rs` | Compressor + text-safe encoding |
| `src/pack.rs` | Pack layout, planner, merge (latest wins, tombstones) |
| `src/account.rs` | Account file: derivation, encryption, key import formats |
| `src/accounts_flow.rs` | Log in/out, create, save, wallets, transfers, airdrops |
| `src/app.rs` | State, routing, events, explorer and workspace flows |
| `src/chain.rs` | Reading databases and rows straight from Solana |
| `src/attach.rs` | Inscribing files into cells; the inscription viewer |
| `src/inscribe.rs` | Simulate → send → confirm pipeline |
| `src/views.rs`, `src/views_account.rs` | HTML rendering |
| `src/qr.rs` | QR encoder for donation addresses |
| `web/host.js` | The browser bridge (DOM, fetch, storage, files, drag and drop) |
| `tools/` | Reference fixtures from the official SDK and the end-to-end test (dev only) |

## Tests
- `cargo test --release` (18 tests): hashes, Base58, Ed25519 signatures and 150 random PDAs against the SDK and noble; every instruction (`initialize_db_root`, `manage_table_creators`, `create_table` open/locked, `user_initialize`, `db_code_in`, `user_inventory_code_in`, `realloc_account`) byte-for-byte against the SDK's builder; full v1 transactions byte-identical to the SDK's `buildV1Transaction`; PBKDF2 and AES-GCM against Node; opening the SDK's `passwordEncrypt` output; parsing our own transactions back; account file round-trips and key-import formats; JSON escaping identical to `JSON.stringify`; codec and pack round-trips; merge rules.
- `cd tools && npm install && CHROME_PATH=/path/to/chrome npm run e2e` (71 checks): the built page in headless Chromium against a mock chain that verifies every signature (both wire formats), decodes every instruction with the program's IDL and compares its accounts and data with the SDK's builder, plus a mock IQ gateway. Covers: explore, search, HTML escaping; creating an account (file opened with the SDK's `passwordDecrypt`, derivation checked independently); importing a CLI key file and pasted keys; refusing logout with unsaved keys; logging in by drag-and-drop; wrong passphrase; recovering a wallet made after the last save; "remember on this device"; dedicated database wallets and moving SOL; CSV import and packing; attaching a file (wallet setup + `user_inventory_code_in`); inscription with DbRoot realloc; locked vs open tables; web, record and file links; the file viewer and download; record links; My tables; editing a live record; unofficial contributions; growing pre-upgrade accounts before a v1 write; rejecting writes to a locked table at simulation; reading everything back straight from Solana with batched RPC; devnet airdrops; mobile layout.
- `npm run qr`: decodes the generated QR with jsQR.

## Before using it on mainnet
1. **An RPC that accepts browser requests.** Solana's public mainnet endpoint answers browsers with `403 Access forbidden`, so balances, transfers and inscriptions need your own RPC URL in Settings (a free Helius, QuickNode or Triton key works). Reading tables doesn't need one — that goes through IQ's gateway.
2. **Writer locks, table-creation fee, schema checks**: the mock follows the SDK and IDL; a first real run (one small database, one table, one pack, ~0.1 SOL) confirms them. The simulation step shows any program error and the exact cost before anything is sent.

## License
MIT — see LICENSE.
