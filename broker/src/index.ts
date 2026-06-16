// bsky-vita OAuth Worker — hosts the client_metadata.json, receives the
// OAuth callback directly from the authorization server, and serves the
// /pop endpoint the Vita polls for the resulting code.
//
// Lives at https://broker.davidlewis.xyz/.
//
// The earlier two-host design (static `client_metadata.json` + callback
// page on davidlewis.xyz, broker on a separate workers.dev URL) folded
// into one Worker once we added the custom domain — eliminates one HTTP
// hop in the OAuth flow and one deployment surface.
//
// Security model (why this is safe to host):
//
// - The OAuth `code` is **useless without** the on-Vita PKCE verifier AND the
//   on-Vita DPoP private key. Even if every byte through this Worker were
//   exfiltrated, an attacker could not impersonate the user. PKCE is RFC 7636;
//   DPoP is RFC 9449; both are mandatory under atproto OAuth.
// - We never see tokens, JWTs, DIDs, handles, or anything user-identifiable.
//   The `state` is a random per-login nonce; the `code` is opaque to us; the
//   `iss` is just `https://bsky.social` (always, for Bluesky users).
// - The KV entry self-deletes on the first /pop hit; otherwise it expires
//   after 300 s. There is no read API beyond /pop, no enumeration, no admin.
//
// Privacy posture:
// - `observability.enabled = false` in wrangler.jsonc disables Workers Logs.
// - This source contains zero `console.log` / `console.error` / `console.warn`
//   calls on request data. The only side effects are KV put/get/delete.
// - The Worker source is open and auditable in this repo; users who don't
//   want to trust the default deployment can self-host (see README.md).

interface Env {
  STATE_KV: KVNamespace;
}

// 5 min: long enough for the user to bring their attention back to the Vita
// after consenting on their phone; short enough that stale entries don't
// linger. Cloudflare KV minimum TTL is 60 s; 300 is a comfortable middle.
const ENTRY_TTL_SECONDS = 300;

// Canonical Worker origin — referenced from CLIENT_METADATA + as the
// redirect_uri the Vita advertises. Bound to broker.davidlewis.xyz via the
// `routes` block in wrangler.jsonc; the *.workers.dev URL still resolves
// (wrangler keeps it on by default) but isn't used for OAuth.
const ORIGIN = "https://broker.davidlewis.xyz";

// Security headers applied to every HTML response.
const HTML_SECURITY_HEADERS = {
  "X-Content-Type-Options": "nosniff",
  "X-Frame-Options": "DENY",
  "Referrer-Policy": "no-referrer",
  // Prevent search engines from indexing the success / info / metadata
  // surfaces (no useful crawl content anyway, but defensive).
  "X-Robots-Tag": "noindex, nofollow",
} as const;

// The OAuth client metadata document. Served as JSON at /client_metadata.json
// and used by the atproto authorization server to learn redirect URIs,
// scopes, etc. `client_id` MUST equal the URL this document is served at —
// the AS fetches client_id and treats it as the canonical identifier.
const CLIENT_METADATA = {
  client_id: `${ORIGIN}/client_metadata.json`,
  client_name: "bsky-vita",
  client_uri: `${ORIGIN}/`,
  redirect_uris: [
    `${ORIGIN}/callback`,
    // v1.x QR-pickup path (currently a stub page; real QR-render in v1.x).
    // Declared from v1 so adding camera-scan pickup later triggers no
    // metadata churn / re-consent.
    `${ORIGIN}/callback-qr`,
  ],
  scope: "atproto transition:generic transition:chat.bsky",
  grant_types: ["authorization_code", "refresh_token"],
  response_types: ["code"],
  application_type: "native",
  token_endpoint_auth_method: "none",
  dpop_bound_access_tokens: true,
} as const;

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);

    // Only GET is meaningful for this Worker.
    if (request.method !== "GET") {
      return new Response("Method not allowed", { status: 405 });
    }

    switch (url.pathname) {
      case "/client_metadata.json":
        return handleClientMetadata();
      case "/callback":
        return handleCallback(url, env);
      case "/callback-qr":
        return handleCallbackQr();
      case "/pop":
        return handlePop(url, env);
      case "/":
        return handleRoot();
      default:
        return new Response("Not found", { status: 404 });
    }
  },
} satisfies ExportedHandler<Env>;

// Returns the OAuth client metadata document. Cached for an hour on the
// edge — `client_id` is a stable URL the AS fetches once per token issuance,
// and the document itself changes only on deploy.
function handleClientMetadata(): Response {
  return new Response(JSON.stringify(CLIENT_METADATA, null, 2) + "\n", {
    status: 200,
    headers: {
      "Content-Type": "application/json; charset=utf-8",
      "Cache-Control": "public, max-age=3600",
      // No security headers tying it to a single origin — this resource
      // is meant to be fetched by atproto AS servers, not browsers.
    },
  });
}

// Phone-browser entry point. Called once per login, by the user's phone
// browser, immediately after they consent at the authorization server.
async function handleCallback(url: URL, env: Env): Promise<Response> {
  const state = url.searchParams.get("state");
  const code = url.searchParams.get("code");
  const iss = url.searchParams.get("iss");

  // The atproto OAuth spec requires `state` and `code` in the redirect; `iss`
  // is required when the AS advertises `authorization_response_iss_parameter_supported`.
  // bsky.social does. We're strict here so a malformed redirect fails fast.
  if (!state || !code || !iss) {
    return new Response("Missing required parameters (state, code, iss).", {
      status: 400,
      headers: { "Content-Type": "text/plain; charset=utf-8", ...HTML_SECURITY_HEADERS },
    });
  }

  // Reasonable upper bounds — anything beyond these is malformed / abuse.
  // Real atproto values are well under these limits. Reject early before KV.
  if (state.length > 256 || code.length > 1024 || iss.length > 256) {
    return new Response("Parameter too long.", {
      status: 400,
      headers: { "Content-Type": "text/plain; charset=utf-8", ...HTML_SECURITY_HEADERS },
    });
  }

  // Opaque payload — the Worker does not inspect it. The Vita parses it.
  const payload = JSON.stringify({ code, iss });
  await env.STATE_KV.put(state, payload, { expirationTtl: ENTRY_TTL_SECONDS });

  return new Response(SUCCESS_HTML, {
    status: 200,
    headers: { "Content-Type": "text/html; charset=utf-8", ...HTML_SECURITY_HEADERS },
  });
}

// v1.x stub. When camera-scan pickup ships, this endpoint will render the
// (code, state, iss) as a QR code for the Vita's rear camera to scan
// directly — completely bypassing the broker KV path. Today it just tells
// the user the path isn't live yet.
function handleCallbackQr(): Response {
  return new Response(CALLBACK_QR_STUB_HTML, {
    status: 200,
    headers: { "Content-Type": "text/html; charset=utf-8", ...HTML_SECURITY_HEADERS },
  });
}

// Vita-side polling endpoint. The Vita polls this with the same `state` it
// generated locally; on hit it gets the opaque payload and the KV entry is
// deleted. This is BEST-EFFORT single-use: under the Vita's sequential polling
// (one in-flight `/pop` at a time) the get-then-delete reliably hands the
// payload out once and 404s thereafter. It is NOT atomically single-use — two
// concurrent `/pop` requests for the same `state` can both `get` the value
// before either `delete` runs, so both would receive it. Cloudflare KV has no
// atomic get-and-delete primitive; a hard single-use guarantee would require
// serializing pops through a Durable Object (see the delete note below).
async function handlePop(url: URL, env: Env): Promise<Response> {
  const state = url.searchParams.get("state");
  if (!state || state.length > 256) {
    return new Response(null, { status: 400 });
  }

  const value = await env.STATE_KV.get(state);
  if (value === null) {
    // Either nothing has landed yet (Vita polls again in a few seconds) or it
    // already popped. Note this is best-effort, not strict replay protection:
    // because get-then-delete is not atomic, a racing concurrent pop could
    // still observe the value between this read and the delete below.
    return new Response(null, { status: 404 });
  }

  // Delete-after-read: best-effort single-use. We `await` to keep this on the
  // request critical path — under the Vita's sequential polling the next poll
  // should reliably return 404 rather than serving the same code twice during
  // KV's eventual-consistency window. Cost is one extra KV op; acceptable.
  //
  // Caveat: this read-then-delete is NOT atomic. Two concurrent `/pop`s for the
  // same `state` can both read `value` before either delete lands, so both
  // could be served the payload. KV cannot do atomic get-and-delete; if a hard
  // single-use / replay guarantee is ever required, route pops through a
  // Durable Object (per-`state` single-threaded execution) instead of KV.
  await env.STATE_KV.delete(state);

  return new Response(value, {
    status: 200,
    headers: {
      "Content-Type": "application/json",
      "Cache-Control": "no-store",
    },
  });
}

// Friendly root page for anyone who curls / opens the bare host. Confirms
// what this is and links to source for auditability.
function handleRoot(): Response {
  return new Response(INFO_HTML, {
    status: 200,
    headers: { "Content-Type": "text/html; charset=utf-8", ...HTML_SECURITY_HEADERS },
  });
}

const SUCCESS_HTML = `<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>Login received - bsky-vita</title>
  <style>
    body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      background: #15171a; color: #e4e6eb; margin: 0; padding: 0;
      min-height: 100vh; display: grid; place-items: center; text-align: center; }
    main { max-width: 28rem; padding: 2rem; }
    h1 { font-size: 1.5rem; margin: 0 0 0.5rem; color: #1d9bf0; }
    p { font-size: 1rem; line-height: 1.5; margin: 0.75rem 0; color: #c2c6ca; }
    .ok { font-size: 3rem; margin: 0; line-height: 1; }
  </style>
</head>
<body>
  <main>
    <p class="ok">&check;</p>
    <h1>Login received</h1>
    <p>You can close this tab and return to your PS Vita.</p>
  </main>
</body>
</html>`;

const CALLBACK_QR_STUB_HTML = `<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>bsky-vita - QR pickup not yet available</title>
  <style>
    body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      background: #15171a; color: #e4e6eb; margin: 0; padding: 0;
      min-height: 100vh; display: grid; place-items: center; text-align: center; }
    main { max-width: 32rem; padding: 2rem; }
    h1 { font-size: 1.5rem; margin: 0 0 0.5rem; color: #f5a623; }
    p { font-size: 1rem; line-height: 1.5; margin: 0.75rem 0; color: #c2c6ca; }
  </style>
</head>
<body>
  <main>
    <h1>QR pickup ships in v1.x</h1>
    <p>You arrived here because your bsky-vita app requested the camera-scan
      OAuth pickup mode, which isn't shipping until v1.x.</p>
    <p>For now, please return to your Vita and choose the default
      &ldquo;broker pickup&rdquo; sign-in path.</p>
  </main>
</body>
</html>`;

const INFO_HTML = `<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>bsky-vita OAuth broker</title>
  <style>
    body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      background: #15171a; color: #e4e6eb; margin: 0; padding: 2rem;
      max-width: 40rem; margin-inline: auto; line-height: 1.6; }
    h1 { color: #1d9bf0; }
    code { background: #22252a; padding: 0.1em 0.3em; border-radius: 4px;
      font-size: 0.95em; }
    a { color: #1d9bf0; }
    p { color: #c2c6ca; }
  </style>
</head>
<body>
  <h1>bsky-vita OAuth broker</h1>
  <p>This Worker is the OAuth surface used by
    <a href="https://github.com/koalabuttz/bsky-vita">bsky-vita</a>,
    a homebrew Bluesky client for the PlayStation Vita. It serves the
    client metadata, receives the consent-flow redirect from the
    authorization server, and exposes a single-read pickup endpoint the
    Vita polls.</p>
  <p>Endpoints:</p>
  <ul style="color:#c2c6ca">
    <li><code>/client_metadata.json</code> &mdash; OAuth client metadata
      (atproto AS fetches this).</li>
    <li><code>/callback</code> &mdash; receives <code>(code, state, iss)</code>
      from the AS, writes <code>(state &rarr; {code, iss})</code> to KV with a
      5-minute TTL, returns "Login received" to the user.</li>
    <li><code>/pop?state=&hellip;</code> &mdash; the Vita reads this once and
      the entry is deleted.</li>
    <li><code>/callback-qr</code> &mdash; v1.x QR-pickup stub.</li>
  </ul>
  <p>It logs nothing &mdash; <code>observability.enabled</code> is false in
    <code>wrangler.jsonc</code> and the source contains zero
    <code>console.log</code> calls on request data. The authorization codes
    that pass through are cryptographically useless without the on-device
    PKCE verifier and DPoP private key.</p>
  <p>Source: <a href="https://github.com/koalabuttz/bsky-vita/tree/main/broker">github.com/koalabuttz/bsky-vita/broker</a>.
    Privacy-conscious users can self-host &mdash; see the README in that
    directory.</p>
</body>
</html>`;
