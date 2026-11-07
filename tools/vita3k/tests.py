"""Black-box tests: assertions use rendered pixels/text and observed HTTP traffic."""
import subprocess
import threading
import time
import unittest

from PIL import ImageChops, ImageStat

from fixtures import CID, FEED_A, FEED_B
from run import App

TIMELINE = "/xrpc/app.bsky.feed.getTimeline"


class EmulatorTests(unittest.TestCase):
    options = None

    def app(self, *, authenticated=True, kind="plain", configure=None, frontend="sdl"):
        app = App(self.options, self._testMethodName, authenticated, kind, frontend)
        self.addCleanup(app.close)
        if configure:
            configure(app)
        app.start()
        return app

    def test_login_validation_and_password_visibility(self):
        app = self.app(authenticated=False, frontend="qt")
        app.wait_text("Sign in with Bluesky")
        app.tap(480, 395)
        app.wait_text("Handle is required")
        app.tap(480, 452)
        app.wait_text("Sign in with an app password", "Show password")
        app.tap(400, 300)
        app.type("test-only-pass")
        app.key("Return")
        app.wait_text("Show password")
        self.assertNotIn("test-only-pass", app.text(box=(190, 270, 770, 325)))
        app.tap(670, 342)
        app.wait_text("Hide password", "test-only-pass")
        app.image("password-visible")
        # Reopening the real IME must preserve the existing field value.
        app.tap(400, 300)
        app.key("Return")
        app.wait_text("test-only-pass", "Hide password")
        app.tap(670, 342)
        app.wait_text("Show password")
        self.assertNotIn("test-only-pass", app.text(box=(190, 270, 770, 325)))
        app.image("password-hidden")
        self.assertEqual(app.fixtures.requests, [])

    def test_temporary_feed_failure_retries_then_renders(self):
        def configure(app):
            app.fixtures.timeline_failures = 2
        app = self.app(configure=configure)
        app.wait_text("Original timeline", "Refresh", timeout=35)
        requests = [r for r in app.fixtures.requests if r["path"] == TIMELINE]
        self.assertEqual([r["status"] for r in requests], [503, 503, 200])
        self.assertGreaterEqual(requests[1]["time"] - requests[0]["time"], 0.85)
        self.assertGreaterEqual(requests[2]["time"] - requests[1]["time"], 1.85)

    def test_permanent_feed_failure_waits_for_manual_retry(self):
        def configure(app):
            app.fixtures.timeline_failures = 1
            app.fixtures.timeline_status = 404
        app = self.app(configure=configure)
        app.wait_text("Could not load timeline", "Retry now")
        time.sleep(3)
        self.assertEqual(app.fixtures.count(TIMELINE), 1)
        app.tap(830, 344)
        app.wait_text("Original timeline")
        self.assertEqual(app.fixtures.count(TIMELINE), 2)

    def test_refresh_preserves_posts_until_success(self):
        app = self.app()
        app.wait_text("Original timeline", "Refresh")
        app.fixtures.timeline_failures = 1
        app.fixtures.timeline_status = 404
        app.tap(895, 20)
        app.wait_text("Original timeline", "Retry now")
        app.image("failed-refresh-retains-post")
        app.fixtures.version = "Fresh replacement"
        app.tap(830, 458)
        app.wait_text("Fresh replacement")
        self.assertNotIn("Original timeline", app.text())
        self.assertEqual(app.fixtures.count(TIMELINE), 3)

    def test_feed_response_reaches_covered_timeline(self):
        app = self.app()
        app.wait_text("Original timeline")
        app.fixtures.version = "Covered response arrived"
        gate = threading.Event()
        app.fixtures.hold = gate
        self.addCleanup(gate.set)
        app.tap(895, 20)
        self.assertTrue(app.fixtures.entered.wait(3), "Refresh was not dispatched")
        # Open an already-loaded row while the refresh HTTP response is held.
        app.tap(350, 145)
        gate.set()
        app.wait_text("Thread fixture")
        app.key("c")
        app.wait_text("Covered response arrived")
        self.assertEqual(app.fixtures.count(TIMELINE), 2)

    def test_pagination_retries_the_same_cursor(self):
        def configure(app):
            app.fixtures.page_failures = 1
        app = self.app(kind="pagination", configure=configure)
        app.wait_text("Page one row")
        for _ in range(12):
            app.key("Down", 0.04)
        app.wait_text("Pagination recovered", timeout=35)
        pages = [r for r in app.fixtures.requests if r["path"] == TIMELINE and r["query"].get("cursor")]
        self.assertEqual([r["query"]["cursor"] for r in pages], ["page-two", "page-two"])
        self.assertEqual([r["status"] for r in pages], [503, 200])

    def test_stale_replies_are_ignored_after_switching_away_and_back(self):
        app = self.app()
        app.wait_text("Original timeline", "Alpha", "Beta")
        gate = threading.Event()
        app.fixtures.hold = gate
        self.addCleanup(gate.set)
        app.tap(155, 65)
        self.assertTrue(app.fixtures.entered.wait(3), "First custom feed request was not sent")
        app.tap(235, 65)
        app.tap(155, 65)
        gate.set()
        app.wait_text("Alpha generation 2")
        self.assertNotIn("Alpha generation 1", app.text())
        path = "/xrpc/app.bsky.feed.getFeed"
        self.assertEqual(app.fixtures.count(path, feed=FEED_A), 2)
        self.assertEqual(app.fixtures.count(path, feed=FEED_B), 1)

    def test_feed_retries_are_bounded_and_can_be_restarted_manually(self):
        def configure(app):
            app.fixtures.timeline_failures = 99
        app = self.app(configure=configure)
        app.wait(lambda: app.fixtures.count(TIMELINE) == 4, "Retry limit was not reached", timeout=25)
        app.wait_text("Could not load timeline", "Retry now")
        time.sleep(2)
        self.assertEqual(app.fixtures.count(TIMELINE), 4)
        app.fixtures.timeline_failures = 0
        app.tap(830, 344)
        app.wait_text("Original timeline")
        self.assertEqual(app.fixtures.count(TIMELINE), 5)

    def test_two_image_grid_normalizes_urls_and_retries(self):
        def configure(app):
            app.fixtures.image_failures = 2
        app = self.app(kind="images", configure=configure)
        app.wait_text("Original timeline")
        app.wait(lambda: app.colored("red") > 10000 and app.colored("green") > 10000,
            "Two-image grid did not render both JPEG fixtures", timeout=35)
        self.assertGreaterEqual(sum(app.fixtures.image_counts.get(f"/image/{c}@jpeg", 0) for c in ("red", "green")), 4)
        self.assertFalse(any("@webp" in path for path in app.fixtures.image_counts))
        # Repeated frames must not download an already-cached avatar again.
        self.assertEqual(app.fixtures.image_counts["/avatar_thumbnail/0@jpeg"], 1)
        app.image("two-image-grid")

    def test_invalid_fullsize_image_stops_and_manual_retry_recovers(self):
        def configure(app):
            app.fixtures.full_bad = True
        app = self.app(kind="images", configure=configure)
        app.wait(lambda: app.colored("red") > 10000, "Thumbnail did not render")
        app.tap(240, 250)
        app.wait_text("Retry image", box=(350, 480, 610, 540))
        path = "/full/red@jpeg"
        time.sleep(3)
        self.assertEqual(app.fixtures.image_counts.get(path), 1)
        app.fixtures.full_bad = False
        app.tap(480, 511)
        app.wait(lambda: app.fixtures.image_counts.get(path) == 2 and app.colored("red") > 100000,
            "Manual fullsize retry did not recover")
        app.image("viewer-recovered")
        app.key("c")
        app.wait_text("Original timeline")

    def test_top_level_navigation_and_compose_return(self):
        app = self.app()
        app.wait_text("Original timeline")
        app.key("z")
        app.wait_text("New post")
        app.key("c")
        app.wait_text("Original timeline")
        app.tap(480, 515)
        app.wait_text("No notifications")
        app.tap(672, 515)
        app.wait_text("No conversations")
        app.tap(864, 515)
        app.wait_text("Fixture Tester", "Profile fixture post")
        app.tap(288, 515)
        app.wait_text("Tap to search", "People", "Posts")
        app.tap(96, 515)
        app.wait_text("Original timeline", "Refresh")
        self.assertFalse(app.fixtures.unexpected, app.fixtures.unexpected)

    def test_image_cache_eviction_stress(self):
        app = self.app(kind="cache")
        app.wait_text("Cache row 00")
        deadline = time.monotonic() + 120
        while len(app.fixtures.image_counts) < 70 and time.monotonic() < deadline:
            # Hold across multiple guest frames; JPEG uploads can stall a frame.
            app.key("Down", 0.3)
            app.alive()
        self.assertGreaterEqual(len(app.fixtures.image_counts), 70,
            "Stress test did not exceed the 64-texture cache capacity")
        app.tap(895, 20)
        app.wait_text("Cache row 00")
        app.wait(lambda: app.fixtures.image_counts.get("/avatar_thumbnail/0@jpeg", 0) >= 2,
            "Evicted first avatar was not downloaded again after refresh")
        app.image("cache-eviction-survived")

    def test_color_emoji_are_rendered(self):
        def configure(app):
            app.fixtures.version = "Emoji fixture: 😀 ❤️ 👩🏽‍💻 🇺🇸"
        app = self.app(configure=configure)
        app.wait_text("Emoji fixture")
        app.wait(lambda: app.colored("any", box=(205, 115, 650, 160)) > 80,
            "Color emoji did not upload/render in the post body")
        app.image("emoji-rendering")

    def test_video_frames_pause_resume_and_seek(self):
        def configure(app):
            directory = app.data / "video"
            directory.mkdir()
            subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "lavfi", "-i",
                "testsrc2=size=320x180:rate=24", "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000",
                "-t", "30", "-c:v", "libx264", "-profile:v", "baseline", "-pix_fmt", "yuv420p",
                "-preset", "ultrafast", "-c:a", "aac", "-movflags", "+faststart", str(directory / f"{CID}.mp4")],
                check=True, timeout=45)
        app = self.app(kind="video", configure=configure)
        app.wait_text("Original timeline")
        app.tap(120, 185)
        app.wait(lambda: app.colored("any", box=(200, 100, 760, 400)) > 10000,
            "Decoded color video was not rendered", timeout=35)
        region = (200, 110, 760, 390)
        first = app.image().crop(region)
        time.sleep(0.8)
        second = app.image().crop(region)
        self.assertGreater(sum(ImageStat.Stat(ImageChops.difference(first, second)).mean), 2,
            "Video frame did not advance")
        app.key("z")
        time.sleep(0.5)
        paused = app.image().crop(region)
        time.sleep(0.6)
        still = app.image().crop(region)
        self.assertLess(sum(ImageStat.Stat(ImageChops.difference(paused, still)).mean), 1,
            "Paused video kept advancing")
        app.key("z")
        app.key("Right")
        time.sleep(0.8)
        resumed = app.image("video-resumed-after-seek").crop(region)
        self.assertGreater(sum(ImageStat.Stat(ImageChops.difference(still, resumed)).mean), 2)
        app.key("c")
        app.wait_text("Original timeline")
