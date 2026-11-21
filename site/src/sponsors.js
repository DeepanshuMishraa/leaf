// The sponsor wall. Dodo calls POST /api/dodo-webhook when a payment clears;
// each sponsor payment is kept in the SPONSORS KV namespace, keyed by payment
// id so a retried webhook can't count twice. GET /api/sponsors lists them.
//
// Secret (wrangler secret put):
//   DODO_WEBHOOK_SECRET  signing secret from Dashboard → Developer → Webhooks
// Binding:
//   SPONSORS             a KV namespace (see wrangler.jsonc)

const GITHUB_LOGIN = /^[a-z\d](?:[a-z\d]|-(?=[a-z\d])){0,38}$/i;
const TOLERANCE_S = 5 * 60;

const json = (status, body, headers = {}) =>
  new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json; charset=utf-8", ...headers } });

const fromBase64 = (s) => Uint8Array.from(atob(s), (c) => c.charCodeAt(0));
const toBase64 = (buf) => btoa(String.fromCharCode(...new Uint8Array(buf)));

/** Standard Webhooks check: HMAC-SHA256 over "id.timestamp.body", any v1 signature may match. */
const verify = async (headers, body, secret) => {
  const id = headers.get("webhook-id");
  const ts = headers.get("webhook-timestamp");
  const sigs = headers.get("webhook-signature");
  if (!id || !ts || !sigs) return "missing webhook-id, webhook-timestamp or webhook-signature";
  const age = Math.abs(Date.now() / 1000 - Number(ts));
  if (!Number.isFinite(age) || age > TOLERANCE_S) return `timestamp ${ts} is outside the ${TOLERANCE_S}s window`;
  const key = await crypto.subtle.importKey(
    "raw", fromBase64(secret.replace(/^whsec_/, "")), { name: "HMAC", hash: "SHA-256" }, false, ["sign"],
  );
  const expected = toBase64(await crypto.subtle.sign("HMAC", key, new TextEncoder().encode(`${id}.${ts}.${body}`)));
  const ok = sigs.split(" ").some((part) => {
    const [version, sig] = part.split(",");
    return version === "v1" && sig?.length === expected.length && crypto.subtle.timingSafeEqual(
      new TextEncoder().encode(sig), new TextEncoder().encode(expected),
    );
  });
  return ok ? null : "no signature matched";
};

export const webhook = async (request, env) => {
  if (request.method !== "POST") return json(405, { error: "Use POST." });
  if (!env.DODO_WEBHOOK_SECRET || !env.SPONSORS) {
    console.error("webhook: missing config", { secret: Boolean(env.DODO_WEBHOOK_SECRET), kv: Boolean(env.SPONSORS) });
    // A 5xx makes Dodo retry, so payments that land before setup still count.
    return json(503, { error: "Sponsor storage isn't configured." });
  }
  const body = await request.text();
  const problem = await verify(request.headers, body, env.DODO_WEBHOOK_SECRET);
  if (problem) {
    console.warn("webhook: rejected", problem);
    return json(401, { error: "Invalid signature." });
  }

  let event;
  try {
    event = JSON.parse(body);
  } catch {
    return json(400, { error: "Body isn't JSON." });
  }
  const data = event?.data;
  const meta = data?.metadata;
  // Only sponsor payments go on the wall; acknowledge everything else.
  if (event?.type !== "payment.succeeded" || meta?.kind !== "sponsor") return json(200, { ok: true, skipped: true });

  const name = typeof meta.name === "string" ? meta.name.trim().slice(0, 80) : "";
  const github = typeof meta.github === "string" && GITHUB_LOGIN.test(meta.github) ? meta.github : "";
  if (typeof data.payment_id !== "string" || !name) {
    console.error("webhook: sponsor payment without payment_id or name", data?.payment_id);
    return json(200, { ok: true, skipped: true });
  }
  const cents = Number.isInteger(data.total_amount) ? data.total_amount : 0;
  await env.SPONSORS.put(`payment:${data.payment_id}`, "", { metadata: { name, github, cents, at: event.timestamp ?? null } });
  return json(200, { ok: true });
};

/** One entry per sponsor (by GitHub login, else by name), biggest total first. */
export const list = async (env) => {
  if (!env.SPONSORS) return json(200, [], { "cache-control": "public, max-age=60" });
  const byKey = new Map();
  let cursor;
  do {
    const page = await env.SPONSORS.list({ prefix: "payment:", cursor });
    for (const { metadata: m } of page.keys) {
      if (!m || typeof m.name !== "string") continue;
      const key = m.github ? `gh:${m.github.toLowerCase()}` : `name:${m.name.toLowerCase()}`;
      const seen = byKey.get(key);
      if (seen) {
        seen.cents += m.cents ?? 0;
        seen.name = m.name;
      } else {
        byKey.set(key, { name: m.name, github: m.github || "", cents: m.cents ?? 0 });
      }
    }
    cursor = page.list_complete ? undefined : page.cursor;
  } while (cursor);
  const sponsors = [...byKey.values()]
    .sort((a, b) => b.cents - a.cents)
    .map(({ name, github }) => (github ? { name, github } : { name }));
  return json(200, sponsors, { "cache-control": "public, max-age=60" });
};
