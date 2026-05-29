# bsky-vita OAuth callback broker

A stateless Cloudflare Worker that relays the OAuth `code` from the user's
phone browser to the Vita. The Vita can't receive an OAuth redirect itself
(no usable on-device browser; no native URI scheme), so this Worker exists
to bridge that gap.

It does as little as possible. Two endpoints; one KV namespace; zero logs.

## What it sees, what it can do

The Worker only ever holds a 5-minute, single-read mapping of
`state -> {code, iss}`. Where:

- `state` is a 128-bit random nonce generated **on the Vita** for each login.
- `code` is the OAuth authorization code returned by the authorization server.
- `iss` is the authorization server's identifier (always `https://bsky.social`
  for Bluesky users).

The `code` is **cryptographically useless without the on-Vita PKCE verifier
AND the on-Vita DPoP private key.** Both PKCE ([RFC 7636]) and DPoP
([RFC 9449]) are mandatory under [atproto OAuth]; the broker design works
*because* the spec assumes the redirect channel can leak and compensates
on the wire.

So even a hostile operator running this Worker — or a successful attacker
compromising it — cannot:

- Impersonate any user on Bluesky.
- Read DMs, post, follow, like, or do anything else on a user's behalf.
- Recover anyone's account password (we never see passwords; OAuth doesn't
  share them).
- Learn anyone's handle or DID (`code` and `state` are opaque random-looking
  strings).

[RFC 7636]: https://datatracker.ietf.org/doc/html/rfc7636
[RFC 9449]: https://datatracker.ietf.org/doc/html/rfc9449
[atproto OAuth]: https://atproto.com/specs/oauth

## Privacy posture

- `observability.enabled = false` in `wrangler.jsonc` disables the
  per-request logging Cloudflare would otherwise sample.
- `src/index.ts` contains zero `console.log` / `console.error` /
  `console.warn` calls on request data. The only side effects are
  `STATE_KV.put / get / delete`.
- No request bodies are accepted (every endpoint is `GET`).
- KV entries auto-expire after 300 s and are deleted on the first `/pop` read.

## Deploying your own broker

You don't have to trust the default deployment. The whole Worker is ~150
lines of TypeScript and free to run on Cloudflare's free tier.

### Prerequisites

- Node 22+
- A free Cloudflare account
- `wrangler` (installed via `npm install` below)

### Steps

```sh
cd broker
npm install
npx wrangler login          # opens browser; one-time

# Create the KV namespace and copy the printed ID into wrangler.jsonc:
npx wrangler kv namespace create STATE_KV
# (paste the returned `id` into the `kv_namespaces` block)

# Deploy:
npx wrangler deploy
```

Wrangler prints the deployed URL (e.g.
`https://bsky-vita-broker.<your-subdomain>.workers.dev`). That's the value
to set as `BROKER_URL` in `crates/bsky-oauth/src/lib.rs` if you're building
your own bsky-vita VPK.

### Verifying it works

```sh
# Simulate a redirect arriving from bsky.social:
curl 'https://YOUR-URL/callback?state=test123&code=fake_code&iss=https://bsky.social'

# Should return the "Login received" HTML page.

# Then pop it:
curl 'https://YOUR-URL/pop?state=test123'
# {"code":"fake_code","iss":"https://bsky.social"}

# Second pop returns 404 (single-use):
curl -i 'https://YOUR-URL/pop?state=test123'
# HTTP/2 404
```

## Endpoints

### `GET /callback?state={state}&code={code}&iss={iss}`

Called by the user's phone browser after the www.davidlewis.xyz/bsky-vita/
callback page redirects to it. Writes `(state -> {code, iss})` to KV with
a 300 s TTL. Returns the "Login received" HTML page.

- `400` if any of `state`, `code`, `iss` is missing or unreasonably long.
- `200` with HTML on success.

### `GET /pop?state={state}`

Called by the Vita, polling for the authorization code. Returns the JSON
`{code, iss}` once, then deletes the KV entry (single-read).

- `400` if `state` is missing or unreasonably long.
- `404` if no entry exists for this `state` (either nothing has landed yet,
  or it already popped).
- `200` with `Content-Type: application/json` and body `{"code":"...","iss":"..."}` on success.

### `GET /`

Friendly info page explaining what the Worker is and linking to source.

## Why a Worker and not something simpler?

A pure static page that the Vita scans via its camera would be cleaner from
a "no trust at all" standpoint, and we plan to add that as an opt-in pickup
mechanism (`Transport::Qr`) in v1.x. v1 ships with the broker path because:

- It works without sceCamera integration (which is real engineering — ~150
  LoC FFI + frame conversion + a QR decoder).
- For the default user, "scan a QR with your phone, wait two seconds, you're
  in" is a noticeably smoother UX than "scan a QR with your phone, then pick
  up your Vita and scan a different QR with its rear camera at the right
  angle."
- The privacy guarantee in the broker path is still very strong (PKCE + DPoP
  catch any leak), and users who *do* want zero-trust can self-host this
  Worker on their own Cloudflare account in under five minutes.
