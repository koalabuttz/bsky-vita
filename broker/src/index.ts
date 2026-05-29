// bsky-vita OAuth callback broker.
//
// Stateless relay that exists ONLY because the PS Vita has no usable browser
// and so cannot receive the OAuth redirect from atproto's authorization
// server directly. The user consents on their phone; the phone's browser is
// redirected here; this Worker stores `(state -> {code, iss})` in KV under
// a 5-minute TTL; the Vita polls and pops the value once.
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

// Security headers applied to every HTML response.
const HTML_SECURITY_HEADERS = {
  "X-Content-Type-Options": "nosniff",
  "X-Frame-Options": "DENY",
  "Referrer-Policy": "no-referrer",
  // Prevent search engines from indexing the success / info page (no useful
  // content anyway, but defensive).
  "X-Robots-Tag": "noindex, nofollow",
} as const;

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);

    // Only GET is meaningful for this Worker.
    if (request.method !== "GET") {
      return new Response("Method not allowed", { status: 405 });
    }

    switch (url.pathname) {
      case "/callback":
        return handleCallback(url, env);
      case "/pop":
        return handlePop(url, env);
      case "/":
        return handleRoot();
      default:
        return new Response("Not found", { status: 404 });
    }
  },
} satisfies ExportedHandler<Env>;

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

// Vita-side polling endpoint. The Vita polls this with the same `state` it
// generated locally; on hit it gets the opaque payload and the KV entry is
// deleted (single-use).
async function handlePop(url: URL, env: Env): Promise<Response> {
  const state = url.searchParams.get("state");
  if (!state || state.length > 256) {
    return new Response(null, { status: 400 });
  }

  const value = await env.STATE_KV.get(state);
  if (value === null) {
    // Either nothing has landed yet (Vita polls again in a few seconds) or
    // it already popped (replay protection).
    return new Response(null, { status: 404 });
  }

  // Delete-after-read: enforce single-use. We `await` to keep this on the
  // request critical path — the Vita's next poll should reliably return 404
  // rather than serving the same code twice during KV's eventual-consistency
  // window. Cost is one extra KV op; acceptable.
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

const SUCCESS_HTML = `<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
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

const INFO_HTML = `<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
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
  <p>This Worker is a tiny stateless relay used by
    <a href="https://github.com/koalabuttz/bsky-vita">bsky-vita</a>
    (a homebrew Bluesky client for the PlayStation Vita) to receive OAuth
    authorization codes from the user's phone after consent.</p>
  <p>It exposes exactly two endpoints, <code>/callback</code> (write) and
    <code>/pop</code> (read-once, delete). Entries auto-expire after 5 minutes.</p>
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
