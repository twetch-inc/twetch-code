"use strict";
const $ = (id) => document.getElementById(id);
const KEY = "twetch-wallet-session";

if (location.hash.length > 1) {
  sessionStorage.setItem(KEY, location.hash.slice(1));
  history.replaceState(null, "", "/wallet");
}

async function call(path, body) {
  const res = await fetch(`/wallet/api/${path}`, {
    method: body === undefined ? "GET" : "POST",
    headers: { "x-twetch-session": sessionStorage.getItem(KEY) ?? "", "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
    cache: "no-store",
  });
  if (res.status === 401) throw new Error("This page is no longer signed in. Open Wallet from TwetchCode again.");
  if (path === "qr") return res.text();
  const json = await res.json();
  if (!res.ok) throw new Error(json.error?.message ?? "That did not work.");
  return json;
}

let toastTimer;
function toast(text) {
  $("toast").textContent = text;
  $("toast").classList.add("on");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => $("toast").classList.remove("on"), 3500);
}

function usd(v) {
  if (v === null || v === undefined) return "";
  if (v !== 0 && Math.abs(v) < 0.01) return `$${v.toFixed(6).replace(/0+$/, "")}`;
  return v.toLocaleString("en-US", { style: "currency", currency: "USD" });
}
const bsv = (sats) => `${(sats / 1e8).toFixed(8)} BSV`;
const sats = (n) => `${Number(n).toLocaleString("en-US")} sats`;

const STATES = {
  open: "Running", owed: "To pay", held: "Needs approval", signed: "Sending", pending: "Sent",
  paid: "Paid", free: "Free", gone: "Not charged", failed: "Failed", canceled: "Not sent",
};

function el(tag, className, text) {
  const e = document.createElement(tag);
  if (className) e.className = className;
  if (text !== undefined) e.textContent = text;
  return e;
}

function what(r) {
  if (r.kind === "topup") return "Top-up";
  if (r.kind === "withdraw") return `Withdraw to ${r.to.slice(0, 8)}…`;
  return r.model || "Request";
}

function row(r) {
  const tr = el("tr");
  const when = new Date(r.ts);
  tr.append(el("td", "", when.toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" })));
  tr.append(el("td", "", what(r)));
  const cost = el("td", "num", r.state === "open" || r.state === "gone" ? "–" : sats(r.sats));
  if (r.usd !== null) cost.title = usd(r.usd);
  tr.append(cost);
  const st = el("td", `state-${r.state}`, STATES[r.state] ?? r.state);
  if (r.note) st.title = r.note;
  tr.append(st);
  const tx = el("td", "mono");
  if (r.txid) {
    const a = el("a", "", `${r.txid.slice(0, 10)}…`);
    a.href = `https://whatsonchain.com/tx/${r.txid}`;
    a.target = "_blank";
    a.rel = "noreferrer";
    a.title = r.txid;
    tx.append(a);
  }
  tr.append(tx);
  return tr;
}

function waitingRow(r) {
  const li = el("li");
  const text = el("div");
  const price = r.usd === null ? "" : ` (${usd(r.usd)})`;
  text.append(el("strong", "", `${what(r)}: ${sats(r.sats)}${price}`));
  text.append(el("p", "hint", r.note));
  const actions = el("div", "actions");
  const add = (label, quiet, path, done) => {
    const b = el("button", quiet ? "quiet" : "", label);
    b.type = "button";
    b.addEventListener("click", () => act(b, async () => { await call(path, { id: r.id }); toast(done); }));
    actions.append(b);
  };
  if (r.state === "held") {
    add("Approve payment", false, "approve", "Approved");
  } else {
    if (r.txid) add("Send again", false, "retry", "Sending again");
    add("Release coins", true, "cancel", "Coins released");
  }
  li.append(text, actions);
  return li;
}

let last = "";
let qrFor = "";
let limitsTouched = false;

async function load() {
  const s = await call("state");
  const text = JSON.stringify(s);
  if (text === last) return;
  last = text;
  const total = s.ready_sats + s.coming_sats;
  $("usd").textContent = s.bsv_usd === null ? bsv(total) : usd((total * s.bsv_usd) / 1e8);
  $("bsv").textContent = s.bsv_usd === null ? "The BSV price is not known right now" : bsv(total);
  $("coming").hidden = s.coming_sats === 0;
  $("coming").textContent = `${sats(s.coming_sats)} change on its way`;
  $("problem").hidden = !s.problem;
  $("problem").textContent = s.problem;
  $("address").textContent = s.address;
  $("deposit").hidden = !s.address || s.status !== "ready";
  if (s.address && qrFor !== s.address) {
    qrFor = s.address;
    call("qr").then((svg) => {
      $("qr").src = `data:image/svg+xml,${encodeURIComponent(svg)}`;
      $("qr").hidden = false;
    }).catch(() => {});
  }
  if (!limitsTouched) {
    $("per-payment").value = s.limits.per_payment_usd.toFixed(2);
    $("per-day").value = s.limits.per_day_usd.toFixed(2);
  }
  $("today").textContent = `${usd(s.spent_today_usd)} spent today.`;
  const g = s.gateway;
  $("gw-balance").textContent = g ? sats(g.balance_sats) : "–";
  $("gw-credit").textContent = g ? sats(g.credit_sats) : "–";
  $("gw-spent").textContent = g ? sats(g.spent_sats) : "–";
  $("owed").textContent = sats(s.owed_sats);
  $("gw-note").textContent = s.gateway_error
    ? `Twetch's inference gateway could not be reached: ${s.gateway_error}`
    : "Each request is paid with one transaction for exactly what it cost.";
  $("waiting").hidden = s.waiting.length === 0;
  $("waiting-list").replaceChildren(...s.waiting.map(waitingRow));
  $("activity").replaceChildren(...s.recent.map(row));
  $("empty").hidden = s.recent.length > 0;
  $("restore").hidden = !s.can_restore;
}

async function act(button, f) {
  button.disabled = true;
  try {
    await f();
    last = "";
    await load();
  } catch (e) {
    toast(e.message);
  } finally {
    button.disabled = false;
  }
}

function confirmed(title, text, ok) {
  $("confirm-title").textContent = title;
  $("confirm-text").textContent = text;
  $("confirm-ok").textContent = ok;
  const d = $("confirm");
  d.returnValue = "cancel";
  d.showModal();
  return new Promise((done) => d.addEventListener("close", () => done(d.returnValue === "ok"), { once: true }));
}

$("copy").addEventListener("click", async () => {
  await navigator.clipboard.writeText($("address").textContent);
  toast("Address copied");
});
$("refresh").addEventListener("click", (e) => act(e.target, async () => { await call("refresh", {}); toast("Checked"); }));
for (const id of ["per-payment", "per-day"]) $(id).addEventListener("input", () => { limitsTouched = true; });
$("limits").addEventListener("submit", (e) => {
  e.preventDefault();
  const read = (id) => Number($(id).value.replace(/[$,\s]/g, ""));
  act(e.submitter, async () => {
    const per_payment_usd = read("per-payment");
    const per_day_usd = read("per-day");
    if (!Number.isFinite(per_payment_usd) || !Number.isFinite(per_day_usd)) throw new Error("Enter dollar amounts.");
    await call("limits", { per_payment_usd, per_day_usd });
    limitsTouched = false;
    toast("Limits saved");
  });
});
$("withdraw").addEventListener("submit", async (e) => {
  e.preventDefault();
  const button = e.submitter;
  const to = $("to").value.trim();
  const raw = $("amount").value.trim();
  const amount = raw === "" ? null : Math.round(Number(raw) * 1e8);
  if (amount !== null && !(amount > 0)) {
    toast("Enter an amount in BSV, or leave it empty to send everything.");
    return;
  }
  const size = amount === null ? "everything" : bsv(amount);
  if (!await confirmed("Withdraw", `Send ${size} to ${to}? This cannot be undone.`, "Withdraw")) return;
  act(button, async () => {
    const r = await call("withdraw", amount === null ? { to } : { to, sats: amount });
    $("withdraw").reset();
    toast(`Sent: ${r.txid.slice(0, 12)}…`);
  });
});
function hidePhrase() {
  $("phrase").textContent = "";
  $("phrase-box").hidden = true;
  $("reveal").textContent = "Show recovery phrase";
}
$("reveal").addEventListener("click", async (e) => {
  const button = e.target;
  if (!$("phrase-box").hidden) {
    hidePhrase();
    return;
  }
  const warning = "Make sure nobody can see your screen. Never type these words into a website or chat.";
  if (!await confirmed("Show recovery phrase", warning, "Show it")) return;
  act(button, async () => {
    $("phrase").textContent = (await call("reveal", {})).phrase;
    $("phrase-box").hidden = false;
    button.textContent = "Hide recovery phrase";
  });
});
$("rotate").addEventListener("click", async (e) => {
  const button = e.target;
  if (!await confirmed("Replace gateway key", "The current key stops working right away. Your balance stays.", "Replace key")) return;
  act(button, async () => { await call("rotate", {}); toast("Key replaced"); });
});
$("restore").addEventListener("submit", async (e) => {
  e.preventDefault();
  const button = e.submitter;
  if (!await confirmed("Restore wallet", "The wallet here is replaced by the one these words belong to.", "Restore")) return;
  act(button, async () => {
    await call("restore", { phrase: $("restore-phrase").value });
    $("restore").reset();
    qrFor = "";
    toast("Restored");
  });
});

let failed = false;
async function tick() {
  if (document.hidden) return;
  try {
    await load();
    failed = false;
  } catch (e) {
    if (!failed) last = "";
    failed = true;
    $("problem").hidden = false;
    $("problem").textContent = e instanceof TypeError ? "TwetchCode's wallet is not running." : e.message;
  }
}
document.addEventListener("visibilitychange", () => {
  if (document.hidden) hidePhrase();
  else tick();
});
setInterval(tick, 2500);
tick().then(() => call("refresh", {})).catch(() => {});
