"""Loopback-only XRPC/CDN fixtures. No real account or external service is used."""
import io
import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit

from PIL import Image

DID = "did:plc:aaaaaaaaaaaaaaaaaaaaaaaa"
CID = "bafyreihdwdcefgh4dqkjv67uzcmw7ojee6xedzdetojuzjevtenxquvyku"
FEED_A = f"at://{DID}/app.bsky.feed.generator/alpha"
FEED_B = f"at://{DID}/app.bsky.feed.generator/beta"
DATE = "2026-01-01T00:00:00.000Z"


class Fixtures:
    def __init__(self, directory):
        self.directory = directory
        self.requests = []
        self.unexpected = []
        self.lock = threading.Lock()
        self.kind = "plain"
        self.version = "Original timeline"
        self.timeline_failures = 0
        self.timeline_status = 503
        self.page_failures = 0
        self.image_failures = 0
        self.full_bad = False
        self.hold = None
        self.entered = threading.Event()
        self.feed_versions = {}
        self.image_counts = {}
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), self.handler())
        self.endpoint = f"http://127.0.0.1:{self.server.server_port}"
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def close(self):
        if self.hold:
            self.hold.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=3)
        (self.directory / "requests.json").write_text(json.dumps(self.requests, indent=2))

    def handler(self):
        fixtures = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_GET(self):
                self.respond()

            def do_POST(self):
                self.rfile.read(int(self.headers.get("Content-Length", 0)))
                self.respond()

            def respond(self):
                url = urlsplit(self.path)
                query = {k: v[0] for k, v in parse_qs(url.query).items()}
                status, body, content_type = fixtures.response(self.command, url.path, query)
                with fixtures.lock:
                    fixtures.requests.append({"method": self.command, "path": url.path,
                        "query": query, "status": status, "time": time.monotonic()})
                data = json.dumps(body).encode() if isinstance(body, dict) else body
                self.send_response(status)
                self.send_header("Content-Type", content_type)
                self.send_header("Content-Length", str(len(data)))
                self.send_header("Connection", "close")
                self.end_headers()
                try:
                    self.wfile.write(data)
                except (BrokenPipeError, ConnectionResetError):
                    pass

        return Handler

    def count(self, path, **query):
        with self.lock:
            return sum(r["path"] == path and all(r["query"].get(k) == v for k, v in query.items())
                for r in self.requests)

    def profile(self, index=0):
        return {"did": DID, "handle": "tester.test", "displayName": "Fixture Tester",
            "avatar": f"{self.endpoint}/avatar_thumbnail/{index}@jpeg"}

    def post(self, text, index=0, embed=None):
        post = {"uri": f"at://{DID}/app.bsky.feed.post/p{index}", "cid": CID,
            "author": self.profile(index if self.kind == "cache" else 0),
            "record": {"$type": "app.bsky.feed.post", "text": text, "createdAt": DATE},
            "indexedAt": DATE, "likeCount": 1, "repostCount": 0, "replyCount": 1,
            "viewer": {}}
        if embed:
            post["embed"] = embed
        return post

    def image_embed(self):
        return {"$type": "app.bsky.embed.images#view", "images": [
            {"thumb": f"{self.endpoint}/image/{name}@webp",
             "fullsize": f"{self.endpoint}/full/{name}@webp", "alt": "",
             "aspectRatio": {"width": 320, "height": 180}}
            for name in ["red", "green"]]}

    def timeline(self, query):
        if self.kind == "cache":
            return {"feed": [{"post": self.post(f"Cache row {i:02}", i)} for i in range(80)]}
        if self.kind == "pagination":
            if query.get("cursor"):
                return {"feed": [{"post": self.post("Pagination recovered", 20)}]}
            return {"cursor": "page-two", "feed": [
                {"post": self.post(f"Page one row {i}", i)} for i in range(9)]}
        embed = self.image_embed() if self.kind == "images" else None
        if self.kind == "video":
            embed = {"$type": "app.bsky.embed.video#view", "cid": CID,
                "playlist": f"{self.endpoint}/unused.m3u8", "aspectRatio": {"width": 320, "height": 180}}
        return {"feed": [{"post": self.post(self.version, embed=embed)}]}

    def response(self, method, path, query):
        ok = lambda body: (200, body, "application/json")
        error = lambda status: (status, {"error": "FixtureFailure", "message": "Injected test failure"}, "application/json")
        if path.startswith(("/image/", "/full/", "/avatar_thumbnail/")):
            with self.lock:
                self.image_counts[path] = self.image_counts.get(path, 0) + 1
            if path.startswith("/image/") and self.image_failures:
                self.image_failures -= 1
                return error(503)
            if path.startswith("/full/") and self.full_bad:
                return 200, b"not a JPEG", "image/jpeg"
            color = (235, 35, 35) if "red" in path else (30, 210, 50)
            if "avatar" in path:
                color = (35, 90, 200)
            output = io.BytesIO()
            Image.new("RGB", (64, 64), color).save(output, format="JPEG", quality=95)
            return 200, output.getvalue(), "image/jpeg"
        nsid = path.removeprefix("/xrpc/")
        if nsid == "com.atproto.server.getSession":
            return ok({"did": DID, "handle": "tester.test"})
        if nsid in ("app.bsky.feed.getTimeline", "app.bsky.feed.getFeed"):
            snapshot = self.timeline(query)
            gate, self.hold = self.hold, None
            if gate:
                self.entered.set()
                if not gate.wait(15):
                    return error(504)
            if query.get("cursor") and self.page_failures:
                self.page_failures -= 1
                return error(503)
            if not query.get("cursor") and self.timeline_failures:
                self.timeline_failures -= 1
                return error(self.timeline_status)
            if nsid.endswith("getFeed"):
                feed = query["feed"]
                count = self.feed_versions.get(feed, 0) + 1
                self.feed_versions[feed] = count
                snapshot = {"feed": [{"post": self.post(f"{'Alpha' if feed == FEED_A else 'Beta'} generation {count}")}]}
            return ok(snapshot)
        if nsid == "app.bsky.actor.getPreferences":
            return ok({"preferences": [{"$type": "app.bsky.actor.defs#savedFeedsPrefV2", "items": [
                {"id": "following", "type": "timeline", "value": "following", "pinned": True},
                {"id": "alpha", "type": "feed", "value": FEED_A, "pinned": True},
                {"id": "beta", "type": "feed", "value": FEED_B, "pinned": True}]}]})
        if nsid == "app.bsky.feed.getFeedGenerators":
            return ok({"feeds": [{"uri": uri, "cid": CID, "did": DID, "creator": self.profile(),
                "displayName": name, "indexedAt": DATE} for uri, name in [(FEED_A, "Alpha"), (FEED_B, "Beta")]]})
        if nsid == "app.bsky.feed.getPostThread":
            return ok({"thread": {"$type": "app.bsky.feed.defs#threadViewPost",
                "post": self.post("Thread fixture"), "replies": []}})
        if nsid == "app.bsky.actor.getProfile":
            return ok({**self.profile(), "description": "Local emulator fixture profile",
                "followersCount": 12, "followsCount": 3, "postsCount": 1})
        if nsid == "app.bsky.feed.getAuthorFeed":
            return ok({"feed": [{"post": self.post("Profile fixture post")}]})
        if nsid == "app.bsky.notification.listNotifications":
            return ok({"notifications": []})
        if nsid == "app.bsky.notification.updateSeen" and method == "POST":
            return ok({})
        if nsid == "chat.bsky.convo.listConvos":
            return ok({"convos": []})
        if nsid == "app.bsky.actor.searchActors":
            return ok({"actors": [self.profile()]})
        if nsid == "app.bsky.feed.searchPosts":
            return ok({"posts": [self.post("Search fixture post")]})
        with self.lock:
            self.unexpected.append({"method": method, "path": path, "query": query})
        return error(500)
