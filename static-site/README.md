# bsky-vita hosted assets

Static files to deploy under `https://www.davidlewis.xyz/bsky-vita/` (or your
chosen host root) so bsky-vita's OAuth flow can resolve client metadata and
receive redirects.

## Files

- `bsky-vita/client_metadata.json` — atproto OAuth client metadata. Served
  at `https://www.davidlewis.xyz/bsky-vita/client_metadata.json`. The atproto
  authorization server fetches this URL to learn about the client (redirect
  URIs, scopes, grant types, etc.).
- `bsky-vita/callback/index.html` — broker-pickup redirect page. Served at
  `https://www.davidlewis.xyz/bsky-vita/callback/`. Receives the OAuth redirect
  from bsky.social and forwards the parameters to the Cloudflare Worker
  broker for the Vita to pick up.
- `bsky-vita/callback-qr/index.html` — v1.x QR-pickup stub. Served at
  `https://www.davidlewis.xyz/bsky-vita/callback-qr/`. Currently displays a
  "ships in v1.x" notice; ships as a true broker-less QR-render page in
  v1.x once the Vita-side camera+QR scanner lands.

## Deploying

These are pure static files; any static host will do. The expected URL
structure is the directory pattern (`/bsky-vita/callback/index.html` served
as `/bsky-vita/callback`). Most modern static hosts (Cloudflare Pages,
GitHub Pages, Vercel, Netlify, Nginx with default config) do this
automatically. If your host does not normalize the path, either:

1. Configure URL rewrites so `/bsky-vita/callback` serves
   `/bsky-vita/callback/index.html`, or
2. Rename the files to `callback.html` / `callback-qr.html` and update
   `redirect_uris` in `client_metadata.json` to match the new URLs.

Either approach is fine — the OAuth spec just requires byte-identical
matching between the `redirect_uri` parameter the client sends and one of
the URIs declared in the metadata document.

### Rsync example

If your davidlewis.xyz site root is `~/davidlewis.xyz/public/`:

```sh
rsync -av --delete bsky-vita/ ~/davidlewis.xyz/public/bsky-vita/
```

Then deploy your site as usual.

## After deploying

Verify the metadata is served correctly:

```sh
curl https://www.davidlewis.xyz/bsky-vita/client_metadata.json
```

Should return valid JSON with the four `redirect_uris`, `dpop_bound_access_tokens: true`,
and the scope string.

```sh
curl -i https://www.davidlewis.xyz/bsky-vita/callback/
```

Should return the HTML redirect page (status 200, with the JS that forwards
to the broker).

If you change the broker URL (e.g. to a custom domain), edit the
`BROKER_URL` constant in `callback/index.html` and also update
`BROKER_URL` in `crates/bsky-oauth/src/lib.rs` before rebuilding the VPK.
