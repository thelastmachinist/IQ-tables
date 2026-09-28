// Decode the QR matrix written by `cargo test qr_matrix_for_scanner_check`
// with an independent scanner (jsQR) and compare the text.
const jsQR = require("jsqr");
const fs = require("fs");
const path = require("path");
const [text, ...rows] = fs.readFileSync(path.join(__dirname, "..", "target", "qr.txt"), "utf8").trim().split("\n");
const n = rows.length, scale = 8, border = 4, W = (n + 2 * border) * scale;
const px = new Uint8ClampedArray(W * W * 4).fill(255);
for (let y = 0; y < n; y++) for (let x = 0; x < n; x++) if (rows[y][x] === "1")
  for (let dy = 0; dy < scale; dy++) for (let dx = 0; dx < scale; dx++) {
    const i = (((y + border) * scale + dy) * W + (x + border) * scale + dx) * 4;
    px[i] = px[i + 1] = px[i + 2] = 0;
  }
const r = jsQR(px, W, W);
console.log(r && r.data === text ? "QR OK: " + r.data : "QR MISMATCH: " + (r && r.data));
process.exit(r && r.data === text ? 0 : 1);
