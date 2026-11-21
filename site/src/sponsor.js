// POST /api/sponsor: turns the sponsor dialog into a Dodo Payments checkout
// session and answers with its URL. The product is a pay-what-you-want one, so
// the amount picked in the dialog is what the sponsor pays.
//
// Secrets (wrangler secret put):
//   DODO_PAYMENTS_API_KEY   API key from Dashboard → Developer → API
//   DODO_SPONSOR_PRODUCT_ID a one-time product with "pay what you want" on
// Vars:
//   DODO_ENV                "live_mode" (default) or "test_mode"

const DODO_BASE = { live_mode: "https://live.dodopayments.com", test_mode: "https://test.dodopayments.com" };
const GITHUB_LOGIN = /^[a-z\d](?:[a-z\d]|-(?=[a-z\d])){0,38}$/i;
const MIN_USD = 1;
const MAX_USD = 10000;

const json = (status, body) =>
  new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json; charset=utf-8" } });

const fail = (status, code, message) => json(status, { error: { code, message } });

/** Checks the dialog's fields. Returns { ok, value } or { ok: false, response }. */
const parse = (body) => {
  if (typeof body !== "object" || body === null) {
    return { ok: false, response: fail(400, "bad_body", "Send a JSON object with name, github and amount.") };
  }
  const name = typeof body.name === "string" ? body.name.trim() : "";
  if (name.length < 1 || name.length > 80) {
    return { ok: false, response: fail(400, "bad_name", "Enter a name between 1 and 80 characters.") };
  }
  const github = typeof body.github === "string" ? body.github.trim().replace(/^@/, "") : "";
  if (github !== "" && !GITHUB_LOGIN.test(github)) {
    return { ok: false, response: fail(400, "bad_github", "That isn't a valid GitHub username. Leave it empty or fix it.") };
  }
  const amount = body.amount;
  if (typeof amount !== "number" || !Number.isInteger(amount) || amount < MIN_USD || amount > MAX_USD) {
    return { ok: false, response: fail(400, "bad_amount", `Pick a whole dollar amount from $${MIN_USD} to $${MAX_USD}.`) };
  }
  return { ok: true, value: { name, github, amount } };
};

export const sponsor = async (request, env) => {
  if (request.method !== "POST") return fail(405, "method", "Use POST.");

  const key = env.DODO_PAYMENTS_API_KEY;
  const product = env.DODO_SPONSOR_PRODUCT_ID;
  const base = DODO_BASE[env.DODO_ENV ?? "live_mode"];
  if (!key || !product || !base) {
    console.error("sponsor: missing config", { key: Boolean(key), product: Boolean(product), env: env.DODO_ENV });
    return fail(503, "not_configured", "Sponsoring isn't set up yet. Nothing was charged. Try again later.");
  }

  let body;
  try {
    body = await request.json();
  } catch {
    return fail(400, "bad_body", "Send a JSON object with name, github and amount.");
  }
  const parsed = parse(body);
  if (!parsed.ok) return parsed.response;
  const { name, github, amount } = parsed.value;

  const origin = new URL(request.url).origin;
  let res;
  try {
    res = await fetch(`${base}/checkouts`, {
      method: "POST",
      headers: { authorization: `Bearer ${key}`, "content-type": "application/json" },
      body: JSON.stringify({
        product_cart: [{ product_id: product, quantity: 1, amount: amount * 100 }],
        metadata: { kind: "sponsor", name, github },
        return_url: `${origin}/?sponsor=done`,
      }),
    });
  } catch (err) {
    console.error("sponsor: Dodo unreachable", err);
    return fail(502, "upstream", "The payment provider didn't answer. Nothing was charged. Try again in a minute.");
  }

  const data = await res.json().catch(() => null);
  if (!res.ok || typeof data?.checkout_url !== "string") {
    console.error("sponsor: Dodo rejected checkout", res.status, data);
    return fail(502, "upstream", "Checkout couldn't be started. Nothing was charged. Try again in a minute.");
  }
  return json(200, { url: data.checkout_url });
};
