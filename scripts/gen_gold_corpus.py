#!/usr/bin/env python3
"""Generate the frozen synthetic gold corpus for the M3 search-quality comparison.

    python3 -I scripts/gen_gold_corpus.py            # write the corpus JSON
    python3 -I scripts/gen_gold_corpus.py --audit    # also print the per-query audit

Everything here is invented: projects, services, numbers, people-free prose. The
only randomness (drawer order, ids, timestamps, author field) comes from
random.Random(20261014), so the output is byte-for-byte reproducible.

The text of every drawer is hand-written below so the relevance grades can be
reasoned about. Queries name their graded drawers by *key*; keys are resolved to
the final ids at the end. `audit()` re-checks the grades mechanically (word
overlap, identifier grep, paraphrase/cross-lingual leak checks).

Grading policy (frozen before any system ran on the corpus):
- Grade 2 = the drawer fully answers the query (exact/near duplicates of the
  answer are 2 too; so are transcript chunks that state the whole answer).
  Grade 1 = partial. For identifier queries grade 1 goes ONLY to chunks that
  literally contain the token; semantically related chunks stay ungraded.
- q39 is a deliberate decoy: its query says "Mac", and the cross-compile note
  (key h02) also contains the literal word "Mac" without answering it.
- lexical_ok for Thai queries means "the unsegmented text shares character
  n-grams (>= 4) with a grade-2 drawer", not that a whitespace tokenizer would
  match; for English it means shared content words.
- created_at is random except where the drawer carries its own date (diary
  entries, transcript file names), where it follows that date.
"""
import json
import random
import re
import sys
from pathlib import Path

SEED = 20261014
OUT = Path(__file__).resolve().parent.parent / "crates/alice-daemon/tests/data/gold_corpus.json"

# --------------------------------------------------------------------------
# Drawers.  (key, kind, wing, room, source_file, text)
# --------------------------------------------------------------------------
D = []


def add(key, kind, wing, room, text, source_file=None):
    assert all(d[0] != key for d in D), key
    D.append((key, kind, wing, room, source_file, text.strip()))


# ---- 40 English knowledge notes ------------------------------------------
add("e01", "knowledge_en", "pg_ai", "decisions", """
Decision: the snapshot store stays on SQLite in WAL mode instead of moving to Postgres. The proxy is single-node, the working set fits in a 40 MB page cache, and an extra database server would double our operational surface for no measurable read gain. Revisit only if we ever need more than one writer host.
""")
add("e02", "knowledge_en", "harbor", "decisions", """
Decision: compress harbor archives with zstd at level 6, not xz. On the 12 GB benchmark tree zstd -6 finished in 4 min 10 s against 13 min for xz -6, and the output was only 8% larger. Backup windows matter more to us than the last few percent of ratio.
""")
add("e03", "knowledge_en", "lumen", "decisions", """
Decision: server state in the dashboard is handled by TanStack Query, and Redux is not used for fetched data. Caching, refetch-on-focus and request deduplication come for free, which removed about 600 lines of hand-written reducers and thunks. Redux Toolkit stays on the table only if we ever need cross-page client state.
""")
add("e04", "knowledge_en", "pg_ai", "decisions", """
Decision: each API token is limited to 120 requests per minute, enforced as a token bucket with a burst of 30. Requests over the limit get HTTP 429 with a Retry-After header. The numbers live in config/rate_limits.yaml so they can change without a deploy.
""")
add("e05", "knowledge_en", "harbor", "decisions", """
Decision: harbor encrypts archives with age rather than GPG. Recipients are single-line public keys that fit in an environment variable or a config file, there is no keyring to corrupt, and the format is small enough to audit in an afternoon. Archives written with GPG stay readable through the legacy flag until the 2027 cleanup.
""")
add("e06", "knowledge_en", "lumen", "decisions", """
Decision: Moment.js is gone from the dashboard; Luxon does all date arithmetic and formatting. Moment is in maintenance mode, its mutable objects caused two bugs last quarter, and Luxon tree-shakes well enough to trim 48 KB from the gzipped bundle.
""")
add("e07", "knowledge_en", "pg_ai", "decisions", """
Decision: keep the single monorepo. We looked at splitting pg_ai into three repos (proxy, tools, protos) and rejected it: cross-cutting changes would need three coordinated pull requests, and CI already finishes in nine minutes.
""")
add("e08", "knowledge_en", "personal_dev", "decisions", """
Design reviews happen on Tuesdays at 14:00. The author circulates a written one-page doc at least 24 hours earlier; if nobody has read it by Monday evening the review slides a week rather than being shortened to fit.
""")
add("e09", "knowledge_en", "harbor", "decisions", """
Retention policy for harbor snapshots: keep the last 7 daily, 4 weekly and 12 monthly snapshots. Everything else is pruned after a successful backup, never before, so a failed run cannot shrink the history.
""")
add("e10", "knowledge_en", "lumen", "decisions", """
Decision: lumen builds with Vite instead of webpack. The cold production build dropped from 94 s to 11 s and hot reload from about 3 s to under 300 ms. We lost one loader-based SVG plugin and replaced it with vite-plugin-svgr.
""")
add("e11", "knowledge_en", "pg_ai", "decisions", """
Decision: pg_ai writes JSON log lines through the standard library slog package, not zap. One fewer dependency, and the handler interface is enough for our redaction wrapper. Field names are lower_snake_case and every line carries request_id.
""")
add("e12", "knowledge_en", "harbor", "decisions", """
harbor exit codes: 0 success, 2 usage error, 3 partial backup (some files unreadable but the archive was written), 4 another backup holds the lock. Scripts that wrap harbor should treat 3 as a warning and 4 as retry-later.
""")
add("e13", "knowledge_en", "lumen", "decisions", """
Decision: feature flags are read from a plain JSON file shipped with each release; we do not subscribe to a hosted flag service. The dashboard has fewer than twenty flags, flipping one is a one-line pull request, and a third party's outage should never be able to change our UI.
""")
add("e14", "knowledge_en", "pg_ai", "decisions", """
The asset manifest is cached in memory for 15 minutes in pg_ai. Shorter TTLs hammered the CDN origin during the evening peak; a POST to /internal/manifest/refresh forces an early reload when a hotfix needs to go out sooner.
""")

add("b01", "knowledge_en", "pg_ai", "bugfix", """
Bug: the websocket hub leaked goroutines. Every client spawned a writer goroutine that blocked on its send channel forever once the client disconnected, so the goroutine count climbed by about 400 per hour. Fix: the hub now cancels a per-client context on disconnect and the writer exits on ctx.Done(). Verified with runtime.NumGoroutine() staying flat over 24 hours.
""")
add("b02", "knowledge_en", "harbor", "bugfix", """
Bug: harbor silently skipped files whose names contain a newline. The file list was passed between stages as newline-separated text, so such a name was split into two bogus entries that both failed to stat. Fix: use NUL-delimited listing end to end.
""")
add("b03", "knowledge_en", "lumen", "bugfix", """
Bug: the revenue chart flickered while the window was being resized. The ResizeObserver callback re-rendered the SVG on every pixel of movement and triggered a loop warning. Fix: debounce the handler by 120 ms and render only the final size.
""")
add("b04", "knowledge_en", "pg_ai", "bugfix", """
Bug: requests from pg_ai to the upstream game API hung for minutes during an outage. The Go http.Client had Timeout left at its zero value, which means no timeout at all. Fix: one shared client with an 8 second overall timeout and a 2 second dial timeout.
""")
add("b05", "knowledge_en", "harbor", "bugfix", """
Bug: harbor restore failed on macOS with "operation not permitted" for some files. The archive preserved the com.apple.quarantine extended attribute and the restore path tried to set it back, which the OS will not allow for unsigned binaries. Fix: skip the quarantine xattr on restore and log it at debug level.
""")
add("b06", "knowledge_en", "lumen", "bugfix", """
Bug: dates in the dashboard appeared one day early for users west of UTC, for example in UTC-8. The API sends date-only strings like 2026-03-04, and new Date() parses those as UTC midnight, which is the previous evening locally. Fix: split date-only values into local year, month and day components before formatting.
""")
add("b07", "knowledge_en", "pg_ai", "bugfix", """
Bug: pg_ai's memory grew by roughly 90 MB per day. An unbounded map kept every subscriber ever seen, including long-gone clients. Fix: replace it with an LRU capped at 5000 entries; resident memory is now flat at around 210 MB.
""")
add("b08", "knowledge_en", "harbor", "bugfix", """
Bug: flaky harbor tests that passed alone but failed under a full cargo test run. Several tests wrote into the same fixed directory under /tmp. Fix: every test creates its own tempfile::TempDir and never shares paths.
""")
add("b09", "knowledge_en", "lumen", "bugfix", """
Bug: the settings page froze in an endless render loop. A useEffect listed an object literal in its dependency array, so a new object was created on every render and re-triggered the effect. Fix: memoise the object with useMemo or depend on its primitive fields instead.
""")
add("b10", "knowledge_en", "pg_ai", "bugfix", """
Bug: the snapshot writer deadlocked during compaction. Two compactions acquired the table lock and the manifest lock in opposite order. Fix: a single documented lock order, table before manifest, plus a test that runs two compactions concurrently under the race detector.
""")
add("b11", "knowledge_en", "harbor", "bugfix", """
Bug: harbor's progress bar ran past 100% when a file shrank during the backup, because total bytes were computed at scan time. Fix: clamp per-file progress to the size actually read and recompute the total when the run ends.
""")
add("b12", "knowledge_en", "lumen", "bugfix", """
Bug: modal dialogs appeared behind the sticky header. The header's transform created a stacking context that outranked the modal's z-index. Fix: render modals into a portal at document.body with z-index 1000 and drop the unnecessary transform from the header.
""")
add("b13", "knowledge_en", "personal_dev", "bugfix", """
Fixed the dev laptop's Wi-Fi dropping every twenty minutes or so by turning off adapter power saving: `sudo iw dev wlan0 set power_save off`, made permanent with a NetworkManager conf.d file.
""")

add("h01", "knowledge_en", "pg_ai", "howto", """
How to regenerate the protobuf stubs in pg_ai: install buf 1.x, then run `make proto` from the repo root. It rewrites gen/ and CI fails if you forget to commit the result. Never hand-edit anything under gen/.
""")
add("h02", "knowledge_en", "harbor", "howto", """
How to cross-compile harbor for Linux ARM from a Mac: install zig and cargo-zigbuild, add the target with `rustup target add aarch64-unknown-linux-musl`, then run `cargo zigbuild --release --target aarch64-unknown-linux-musl`. The musl binary is fully static and runs on any distro.
""")
add("h03", "knowledge_en", "lumen", "howto", """
How to debug a failing Playwright test: run `npx playwright test --headed --workers=1 --debug` so a real browser window opens and steps execute one at a time. Use `page.pause()` to stop at a specific line, and open the trace afterwards with `npx playwright show-trace`.
""")
add("h04", "knowledge_en", "pg_ai", "howto", """
How to rotate API tokens in pg_ai: run `tokenctl rotate --grace 24h`. The old token keeps working for the grace period so clients can switch without downtime; after that it returns 401. Run `tokenctl list` to confirm the old one shows as expiring.
""")
add("h05", "knowledge_en", "harbor", "howto", """
How to verify a harbor backup: `harbor verify --deep <archive>` re-reads every chunk, checks its BLAKE3 hash against the manifest and test-decrypts a sample. Without --deep it only checks the manifest signature, which takes about a second.
""")
add("h06", "knowledge_en", "lumen", "howto", """
How to add a locale to the dashboard: copy src/locales/en.json to the new language code, run `npm run i18n:extract` to sync keys, translate the values, and register the code in supportedLocales. CI fails if any key is missing.
""")
add("h07", "knowledge_en", "pg_ai", "howto", """
How to profile CPU in pg_ai: start the server with profiling enabled, then run `go tool pprof -http=:6060 "http://localhost:<port>/debug/pprof/profile?seconds=30"`. The flame graph is under View, Flame Graph.
""")
add("h08", "knowledge_en", "personal_dev", "howto", """
How to review a branch without disturbing current work: `git worktree add ../review-topic origin/topic`, open it in a second editor window, and delete it afterwards with `git worktree remove ../review-topic`.
""")
add("h09", "knowledge_en", "harbor", "howto", """
Release process for harbor: update CHANGELOG.md, bump the version in Cargo.toml, tag vX.Y.Z and push the tag. CI builds binaries for four targets, writes SHA256SUMS and signs it with the release key. Making the draft GitHub release public is the final manual step.
""")
add("h10", "knowledge_en", "lumen", "howto", """
How to publish Storybook: `npm run build-storybook` writes storybook-static/, which the docs workflow uploads to the static bucket on every merge to main. The URL is stable, so designers can bookmark it.
""")
add("h11", "knowledge_en", "pg_ai", "howto", """
How to load test pg_ai with k6: `k6 run loadtest/spin.js` ramps from 0 to 200 virtual users over 2 minutes and holds for 5. Pass `-e BASE_URL=https://staging.example.test` so you do not hit a local instance by accident. Watch p95 latency; the budget is 250 ms.
""")
add("h12", "knowledge_en", "personal_dev", "howto", """
Nightly Postgres backup to S3-compatible storage: cron runs `pg_dump -Fc` at 01:30, pipes it through gzip, and uploads with rclone to the backups bucket. A lifecycle rule on the bucket deletes objects older than 30 days.
""")
add("h13", "knowledge_en", "harbor", "howto", """
How to debug flaky CI in harbor: re-run the job with RUST_LOG=trace and RUST_BACKTRACE=1 set as job variables, then download the uploaded test-logs artifact. Each test writes its own file named after the test.
""")

# ---- 25 Thai knowledge notes ---------------------------------------------
add("t01", "knowledge_th", "pg_ai", "decisions", """
ตัดสินใจย้ายที่เก็บเซสชันของ pg_ai ออกจากหน่วยความจำในโปรเซสไปไว้ที่ Redis เพราะเราต้องรันหลายอินสแตนซ์หลังโหลดบาลานเซอร์ และผู้ใช้ไม่ควรหลุดล็อกอินเวลาที่เครื่องหนึ่งรีสตาร์ต ตั้งอายุเซสชันไว้ 30 นาทีและต่ออายุทุกครั้งที่มีการใช้งาน
""")
add("t02", "knowledge_th", "harbor", "howto", """
วิธีตั้งเวลาสำรองข้อมูลอัตโนมัติ: เพิ่มบรรทัด cron ให้รันทุกวันตอนตีสองด้วยคำสั่ง harbor backup และให้ส่งอีเมลแจ้งเตือนเมื่อโปรแกรมจบด้วยรหัสที่ไม่ใช่ศูนย์ อย่าลืมกำหนดตัวแปร PATH ไว้ในไฟล์ crontab เพราะ cron ไม่ได้โหลดค่าจากเชลล์ของเรา
""")
add("t03", "knowledge_th", "lumen", "bugfix", """
บั๊กปุ่มบันทึกในหน้าตั้งค่า: ผู้ใช้กดซ้ำสองครั้งติดกันทำให้ระบบสร้างรายการซ้ำ เพราะคำขอแรกยังไม่ทันตอบกลับ แก้โดยปิดปุ่มและแสดงสถานะกำลังบันทึกระหว่างรอ พร้อมให้ฝั่งเซิร์ฟเวอร์รับ idempotency key เพื่อกันซ้ำอีกชั้นหนึ่ง
""")
add("t04", "knowledge_th", "personal_dev", "decisions", """
ข้อตกลงเรื่องภาษา: เอกสารและบันทึกภายในจะเขียนเป็นภาษาไทยก็ได้ตามสะดวก แต่ชื่อฟังก์ชัน ชื่อตัวแปร และข้อความคอมมิตให้เขียนเป็นภาษาอังกฤษเสมอ เพื่อให้ค้นหาง่ายและเพื่อนร่วมทีมที่ไม่ได้อ่านไทยก็เข้าใจได้
""")
add("t05", "knowledge_th", "lumen", "decisions", """
สรุปประชุมวันพุธ: ทีมตกลงปิดรายงานสรุปรายสัปดาห์ชั่วคราว เพราะคิวรีรวมข้อมูลใช้เวลาเกินสิบวินาทีและทำให้ฐานข้อมูลช้าในช่วงเช้า ระหว่างนี้เหลือเฉพาะรายงานรายเดือน จะกลับมาเปิดอีกครั้งเมื่อมีตารางสรุปผลล่วงหน้าแล้ว
""")
add("t06", "knowledge_th", "lumen", "bugfix", """
วิธีแก้ฟอนต์ไทยเพี้ยนในรายงาน PDF ที่สร้างฝั่งเซิร์ฟเวอร์: สระลอยและวรรณยุกต์ซ้อนทับกัน เพราะไลบรารีไม่ได้จัดรูปอักขระก่อนวาด แก้โดยฝังฟอนต์ Sarabun ลงในไฟล์และเปิดโหมด text shaping ก่อนวาดข้อความ แล้วเปิดไฟล์ดูด้วยตาทุกครั้งที่อัปเดตเวอร์ชันของไลบรารี
""")
add("t07", "knowledge_th", "personal_dev", "howto", """
ลำดับตั้งค่าเครื่องใหม่สำหรับเขียนโค้ด: ติดตั้ง Homebrew ก่อน แล้วใช้ mise ติดตั้ง Go, Node และ Rust ตามเวอร์ชันในไฟล์ .tool-versions จากนั้นคัดลอก dotfiles จากรีโปส่วนตัว และตั้งค่า git ให้เซ็นคอมมิตด้วย SSH key
""")
add("t08", "knowledge_th", "lumen", "bugfix", """
บั๊กปีแสดงเป็น พ.ศ.: เมื่อใช้ Intl.DateTimeFormat กับ locale th-TH เบราว์เซอร์จะเลือกปฏิทินพุทธเป็นค่าเริ่มต้น ทำให้ปี 2026 ขึ้นเป็น 2569 ในกราฟที่ต้องการ ค.ศ. แก้โดยระบุ calendar: 'gregory' ให้ชัดเจน ส่วนหน้าที่ต้องการ พ.ศ. จริง ๆ ให้ระบุ calendar: 'buddhist' เอง
""")
add("t09", "knowledge_th", "pg_ai", "decisions", """
ตัดสินใจใช้เซิร์ฟเวอร์ขนาดกลางสองเครื่องแทนเครื่องใหญ่เครื่องเดียว แม้ราคารวมจะสูงกว่าเล็กน้อย เพราะถ้าเครื่องหนึ่งล่มอีกเครื่องยังรับงานต่อได้ และเราอัปเดตระบบทีละเครื่องได้โดยไม่ต้องหยุดบริการ
""")
add("t10", "knowledge_th", "lumen", "howto", """
วิธีลดขนาดรูปภาพบนหน้าเว็บ: แปลงเป็น WebP คุณภาพ 80 ด้วย cwebp หรือ sharp ซึ่งเล็กลงราว 60% เมื่อเทียบกับ JPEG เดิม และให้ใส่ width กับ height ในแท็ก img เสมอ เพื่อไม่ให้เลย์เอาต์กระโดดตอนรูปโหลดเสร็จ
""")
add("t11", "knowledge_th", "pg_ai", "bugfix", """
บั๊กค้นชื่อภาษาไทยไม่เจอ: ข้อมูลบางรายการเก็บสระกับวรรณยุกต์เรียงลำดับต่างจากที่ผู้ใช้พิมพ์ ทำให้สตริงดูเหมือนกันแต่เทียบกันไม่เท่า แก้โดย normalize เป็น NFC และจัดลำดับอักขระกำกับให้ตรงกันก่อนทำดัชนีและก่อนค้นหา
""")
add("t12", "knowledge_th", "pg_ai", "decisions", """
ตัดสินใจจำกัดขนาดไฟล์อัปโหลดไว้ที่ 20 เมกะไบต์ต่อไฟล์ เพราะไฟล์ที่ใหญ่กว่านี้ทำให้เวิร์กเกอร์ค้าง และผู้ใช้ส่วนใหญ่ส่งแค่ภาพหน้าจอ หากต้องการส่งไฟล์ใหญ่กว่านั้นให้ใช้ลิงก์อัปโหลดตรงไปยังที่เก็บอ็อบเจ็กต์
""")
add("t13", "knowledge_th", "personal_dev", "howto", """
วิธีย้อนคอมมิตที่พุชขึ้นรีโมตไปแล้วโดยไม่เขียนประวัติทับ: ใช้ git revert <sha> เพื่อสร้างคอมมิตใหม่ที่ล้างผลของคอมมิตเดิม ห้ามใช้ git push --force บนสาขา main เพราะคนที่ดึงโค้ดไปแล้วจะพัง
""")
add("t14", "knowledge_th", "pg_ai", "bugfix", """
สรุปเหตุระบบล่มคืนวันเสาร์: ดิสก์เต็มเพราะไฟล์ล็อกโตไม่หยุดและไม่มีการหมุนไฟล์ ทำให้ฐานข้อมูลเขียนไม่ได้ แก้โดยตั้ง logrotate เก็บล็อกย้อนหลัง 14 วัน บีบอัดไฟล์เก่า และเพิ่มการแจ้งเตือนเมื่อพื้นที่ว่างเหลือต่ำกว่า 15%
""")
add("t15", "knowledge_th", "personal_dev", "howto", """
วิธีจัดการ SSH key: สร้างคีย์แยกต่อเครื่อง ไม่คัดลอกคีย์ข้ามเครื่อง ตั้งรหัสผ่านป้องกันคีย์ แล้วให้ ssh-agent จำรหัสผ่านผ่าน Keychain ของ macOS เมื่อเครื่องหายก็เพิกถอนเฉพาะคีย์ของเครื่องนั้น
""")
add("t16", "knowledge_th", "harbor", "decisions", """
ตัดสินใจเขียน harbor ด้วย Rust แทน Python เพราะอยากแจกเป็นไฟล์ไบนารีไฟล์เดียวที่ผู้ใช้ไม่ต้องติดตั้งตัวแปลภาษา และต้องการความเร็วในการอ่านไฟล์จำนวนมากพร้อมกัน
""")
add("t17", "knowledge_th", "lumen", "bugfix", """
บั๊กแบบฟอร์มลงทะเบียนไม่รับเบอร์โทรที่ขึ้นต้นด้วย +66 เพราะ regex เดิมรับเฉพาะเลขสิบหลักที่ขึ้นต้นด้วย 0 แก้เป็นแปลงเบอร์ให้อยู่ในรูปแบบ E.164 ก่อนแล้วค่อยตรวจสอบ
""")
add("t18", "knowledge_th", "personal_dev", "decisions", """
ประชุมสแตนด์อัพเริ่มเก้าโมงครึ่ง ใช้เวลาไม่เกินสิบห้านาที ทุกคนตอบสามข้อ: เมื่อวานทำอะไร วันนี้จะทำอะไร ติดอะไรอยู่ ถ้าเรื่องไหนต้องคุยลึกให้แยกไปคุยกันต่อหลังจบประชุม
""")
add("t19", "knowledge_th", "lumen", "howto", """
วิธีทดสอบหน้าเว็บบนเน็ตช้า: เปิด DevTools แท็บ Network เลือก throttling เป็น Slow 3G และติ๊กปิดแคช แล้วรีโหลดหน้า ดูว่าเนื้อหาสำคัญขึ้นภายในสามวินาทีหรือไม่
""")
add("t20", "knowledge_th", "lumen", "decisions", """
ปีนี้ยังไม่ทำแอปมือถือแบบเนทีฟ ใช้ PWA ที่ติดตั้งลงหน้าจอหลักได้ไปก่อน เพราะทีมมีคนเดียวที่เขียน Swift ได้ และฟีเจอร์ที่ผู้ใช้ต้องการส่วนใหญ่คือดูรายงานกับรับการแจ้งเตือน
""")
add("t21", "knowledge_th", "pg_ai", "howto", """
วิธีเปลี่ยนรหัสผ่านฐานข้อมูลโดยไม่ให้บริการหยุด: สร้างผู้ใช้ตัวที่สองที่มีสิทธิ์เท่ากันด้วยรหัสผ่านใหม่ อัปเดตค่าในระบบเก็บความลับ รีสตาร์ตบริการทีละเครื่อง แล้วค่อยลบผู้ใช้ตัวเก่าทิ้ง
""")
add("t22", "knowledge_th", "lumen", "bugfix", """
บั๊กอีเมลแจ้งเตือนตกไปอยู่ในกล่องสแปม สาเหตุคือโดเมนผู้ส่งยังไม่ได้ตั้งค่า SPF และ DKIM แก้โดยเพิ่มเรคคอร์ด TXT ตามที่ผู้ให้บริการอีเมลกำหนด แล้วรอ DNS อัปเดตประมาณหนึ่งชั่วโมงก่อนทดสอบอีกครั้ง
""")
add("t23", "knowledge_th", "personal_dev", "decisions", """
หลักการเขียนข้อความคอมมิต: บรรทัดแรกไม่เกิน 72 ตัวอักษร ขึ้นต้นด้วยกริยาในรูปคำสั่ง และบอกว่า "ทำไม" มากกว่า "ทำอะไร" ส่วนรายละเอียดยาว ๆ ให้ไว้ในเนื้อความหลังเว้นหนึ่งบรรทัด
""")
add("t24", "knowledge_th", "personal_dev", "howto", """
ตั้งชื่อสาขา git ตามรูปแบบ feat/, fix/ หรือ chore/ ตามด้วยคำอธิบายสั้น ๆ ตัวพิมพ์เล็กคั่นด้วยขีดกลาง เช่น fix/login-timeout สาขาที่รวมเข้า main แล้วให้ลบทิ้งทันทีเพื่อไม่ให้รกรีโมต
""")
add("t25", "knowledge_th", "personal_dev", "decisions", """
งบประมาณคลาวด์: ตั้งเพดานไว้ไม่เกิน 3,000 บาทต่อเดือน ตั้งการแจ้งเตือนเมื่อค่าใช้จ่ายแตะ 80% ของงบ และตรวจบิลทุกวันศุกร์ ทรัพยากรที่ไม่มีแท็กบอกเจ้าของจะถูกปิดหลังผ่านไปสองสัปดาห์
""")

# ---- 15 mixed Thai/English notes -----------------------------------------
add("m01", "knowledge_mixed", "lumen", "decisions", """
เปิด feature flag ชื่อ new_checkout ให้ผู้ใช้ 10% ก่อนในสัปดาห์แรก ถ้า error rate ไม่เกิน 1% ค่อยเพิ่มเป็น 50% และ 100% ตามลำดับ ถ้ามีปัญหาให้ปิดโดยแก้ค่า rollout ในไฟล์ flags.json แล้ว deploy ใหม่
""")
add("m02", "knowledge_mixed", "harbor", "howto", """
ตั้งค่า CI ให้รัน cargo clippy กับ cargo test แบบขนานกันคนละ job และใช้ sccache เก็บแคชคอมไพล์ไว้บน S3 ทำให้เวลา CI ลดจาก 14 นาทีเหลือ 6 นาที
""")
add("m03", "knowledge_mixed", "pg_ai", "howto", """
การ deploy บน Kubernetes ใช้ rolling update โดยตั้ง maxUnavailable เป็น 0 และ maxSurge เป็น 1 เพื่อให้ pod เก่ายังรับ traffic อยู่จนกว่า pod ใหม่จะผ่าน readiness probe
""")
add("m04", "knowledge_mixed", "pg_ai", "bugfix", """
หน้ารายการธุรกรรมช้าเพราะ query กรองด้วย user_id แล้วเรียงตาม created_at ต้อง scan ทั้งตาราง เมื่อเพิ่ม composite index (user_id, created_at DESC) เวลาลดจาก 1.8 วินาทีเหลือ 40 มิลลิวินาที เร็วขึ้นประมาณ 45 เท่า
""")
add("m05", "knowledge_mixed", "lumen", "bugfix", """
เจอ CORS error ตอนเรียก API จาก staging เพราะ allowed origins มีแต่ production แก้โดยเพิ่ม https://staging.example.test ในรายการ ALLOWED_ORIGINS และอย่าใช้ wildcard * ร่วมกับ credentials
""")
add("m06", "knowledge_mixed", "pg_ai", "decisions", """
ตั้ง JWT access token ให้หมดอายุใน 15 นาที ส่วน refresh token อยู่ได้ 14 วันและหมุนเปลี่ยนทุกครั้งที่ใช้ (rotation) ถ้าตรวจพบการใช้ refresh token เก่าซ้ำ ให้เพิกถอนทั้งชุดทันที
""")
add("m07", "knowledge_mixed", "harbor", "howto", """
Docker image ของ harbor-server ใหญ่ถึง 1.2 GB จึงใช้ multi-stage build แยกขั้น build ออกจากขั้น runtime และใช้ base image แบบ distroless ทำให้เหลือประมาณ 90 MB
""")
add("m08", "knowledge_mixed", "pg_ai", "howto", """
ตั้ง pre-commit hook ให้รัน gofmt และ golangci-lint เฉพาะไฟล์ที่เปลี่ยน ถ้า lint ไม่ผ่านจะไม่ให้คอมมิต ข้ามได้ด้วย --no-verify เฉพาะกรณีฉุกเฉินเท่านั้น
""")
add("m09", "knowledge_mixed", "pg_ai", "decisions", """
webhook ที่ส่งไปหาลูกค้าถ้าล้มเหลวจะ retry แบบ exponential backoff เริ่มที่ 2 วินาที คูณสองทุกครั้ง พร้อม jitter ±20% สูงสุด 5 ครั้ง หลังจากนั้นย้ายเข้า dead-letter queue
""")
add("m10", "knowledge_mixed", "pg_ai", "howto", """
การเปลี่ยน schema ฐานข้อมูลใช้ expand/contract pattern: ปล่อยเวอร์ชันที่เพิ่มคอลัมน์ใหม่ก่อน (expand) ย้ายข้อมูลและสลับโค้ดไปอ่านคอลัมน์ใหม่ แล้วค่อยลบคอลัมน์เก่าในการ release ถัดไป (contract) จะได้ rollback ได้ทุกขั้น
""")
add("m11", "knowledge_mixed", "lumen", "howto", """
Sentry ส่ง alert ถี่เกินไป จึงตั้ง tracesSampleRate เป็น 0.2 และเพิ่ม ignoreErrors สำหรับ error ที่มาจาก browser extension ซึ่งเราแก้ไม่ได้
""")
add("m12", "knowledge_mixed", "pg_ai", "decisions", """
การค้นหาภาษาไทยต้องตัดคำก่อน ใช้ ICU word break iterator สำหรับ locale th ทั้งตอนทำ index และตอนรับ query ด้วยตัวตัดคำเดียวกัน ไม่เช่นนั้นคำค้นจะไม่ตรงกับ token ที่เก็บไว้
""")
add("m13", "knowledge_mixed", "lumen", "bugfix", """
login ด้วย Google ล้มเหลวบน production เพราะ redirect URI ที่ลงทะเบียนไว้เป็น http ไม่ใช่ https แก้ใน console ของผู้ให้บริการ OAuth แล้วรอสักครู่ก่อนลองใหม่
""")
add("m14", "knowledge_mixed", "personal_dev", "decisions", """
ใช้ Terraform workspace แยก staging กับ prod และกำหนดให้ terraform apply บน prod รันได้จาก CI เท่านั้น หลังจากผ่านการรีวิว plan แล้ว
""")
add("m15", "knowledge_mixed", "lumen", "bugfix", """
ปัญหา N+1 ใน GraphQL: resolver ของ orders ยิง query หาลูกค้าทีละแถว แก้ด้วย dataloader ที่รวม id ใน tick เดียวกันให้เหลือ query เดียวต่อหนึ่ง request
""")

# ---- 20 identifier / path notes ------------------------------------------
add("i01", "identifier", "pg_ai", "reference", """
HostLinkReconnect is the state the pg_ai client enters after the host link drops. It retries after jittered delays of 1, 2, 4 and 8 seconds, then every 15 seconds, and bumps the reconnect counter on each attempt. Calls made while in this state are queued, up to 64, and failed with ErrHostDown beyond that.
""")
add("i02", "identifier", "pg_ai", "reference", """
writer_lease is the row in the meta table that says which process may write snapshots. A writer renews it every 5 seconds with a 20 second expiry; if a renewal fails the process must stop writing at once and exit.
""")
add("i03", "identifier", "pg_ai", "reference", """
internal/teacher/jev/client.go wraps the HTTP calls to the tutor service: it signs requests, retries idempotent calls twice and caches nothing. Its tests live in client_test.go and use an httptest server; never point them at a real endpoint.
""")
add("i04", "identifier", "pg_ai", "reference", """
ErrLeaseExpired is what the store returns when a write is attempted after the single-writer fence has lapsed. The HTTP layer maps it to 503 with Retry-After: 5, and the CLI maps it to exit status 75.
""")
add("i05", "identifier", "harbor", "reference", """
HARBOR_AGE_RECIPIENTS holds a comma-separated list of age public keys (age1...). harbor encrypts every archive to all of them; to add a teammate, append their key and run the next backup. Removing a key does not re-encrypt archives that already exist.
""")
add("i06", "identifier", "harbor", "reference", """
BackupManifestV3 is the manifest struct written at the end of every harbor archive: archive id, creation time, the chunk list with BLAKE3 hashes, and the exclusion rules that were in force. V3 added the exclusion list; V1 and V2 manifests are read-only.
""")
add("i07", "identifier", "pg_ai", "reference", """
max_inflight_frames (default 32) caps how many unacknowledged frames pg_ai sends to a client before it pauses writing. Raising it improves throughput on high-latency links but costs roughly 1 MB of extra memory per connection for every 16 frames.
""")
add("i08", "identifier", "pg_ai", "reference", """
X-Request-Shard is a header the load balancer adds with a value from 0 to 7; pg_ai uses it to pick the worker pool. Clients must not set it, and the proxy strips it from any request that arrives from an untrusted address.
""")
add("i09", "identifier", "harbor", "reference", """
chunk_boundary_seed fixes the starting state of harbor's rolling hash. Change it and every chunk boundary moves, so deduplication against older archives stops working; it is therefore recorded in the archive header and never edited by hand.
""")
add("i10", "identifier", "lumen", "reference", """
LUMEN_FLAG_FILE points at the JSON file with the feature flags. When it is unset the dashboard falls back to the flags.json bundled in the build; in tests set it to a temp file to exercise both branches of a flag.
""")
add("i11", "identifier", "harbor", "reference", """
plan_prune() takes the sorted snapshot list and the retention rules and returns the ids to delete. It is a pure function with no I/O, never selects the newest snapshot, and has a property test that shuffles its input.
""")
add("i12", "identifier", "pg_ai", "reference", """
ConnState::Draining is entered when the server receives SIGTERM: the listener closes, existing connections finish their current frame, and after 25 seconds the rest are dropped with close code 1001.
""")
add("i13", "identifier", "personal_dev", "reference", """
scripts/seed_demo.sh fills a fresh local database with 300 fake accounts and a month of synthetic activity so the dashboards have something to draw. It refuses to run when DATABASE_URL does not point at localhost.
""")
add("i14", "identifier", "pg_ai", "reference", """
migrations/0007_snapshot_epoch.sql adds the epoch column to the snapshots table and backfills it from created_at. It is safe to run twice, and the down script only drops the column, it does not restore the old index.
""")
add("i15", "identifier", "lumen", "reference", """
KpiCard takes label, value, delta and an optional sparkline array. The delta is coloured by sign only, never by whether the change is good, because for churn a negative delta is the happy case.
""")
add("i16", "identifier", "lumen", "reference", """
useSortedRows(rows, key, dir) returns a memoised sorted copy and never mutates its input. Sorting is stable and uses Intl.Collator, so Thai and Latin headings order sensibly together.
""")
add("i17", "identifier", "harbor", "reference", """
The --exclude-from flag reads glob patterns from a file, one per line, with # comments. Patterns are matched against the path relative to the backup root, and a trailing slash restricts a pattern to directories.
""")
add("i18", "identifier", "pg_ai", "reference", """
SnapshotCompactor runs at 03:00 local time and merges delta files older than one day into a single base file. It logs the bytes before and after, and skips the run when fewer than three deltas exist.
""")
add("i19", "identifier", "pg_ai", "reference", """
ALERT_WEBHOOK_URL is where the health checker posts when two consecutive probes fail. It must be an https URL; the checker refuses plain http at startup and prints the offending value with its query string removed.
""")
add("i20", "identifier", "lumen", "reference", """
DEFAULT_PAGE_SIZE is 50 in the dashboard tables and is read once at module load. The server clamps any requested page size to 200, so asking for more silently returns 200 rows.
""")

# ---- 5 diary entries -------------------------------------------------------
add("y1", "diary", "wing_claude", "diary", """
2026-01-20 | harbor kickoff. Sketched the archive layout and picked the chunking approach. Slow start, but the design feels sound and the first spike already reads files faster than the old script. Next: header format. ★★
""")
add("y2", "diary", "wing_claude", "diary", """
2026-02-11 | pg_ai refactor day. Flag parsing moved out of main, golden tests rewritten to compare decoded JSON instead of bytes. Left a TODO about metric names. Focused, few interruptions. ★★
""")
add("y3", "diary", "wing_claude", "diary", """
2026-02-25 | harbor: change detection now trusts the previous manifest rather than the wall clock. Restore safety discussed at length; benchmark table drafted for the README. Good day. ★★★
""")
add("y4", "diary", "wing_claude", "diary", """
2026-03-02 | วันนี้จัด CI ของ pg_ai ให้เร็วขึ้นและตั้งชื่อเมตริกใหม่ ยังเหลือตอบรีวิวอีกหนึ่งข้อ รู้สึกว่าจังหวะงานดีขึ้นเมื่อเขียนแผนตั้งแต่เช้า ★★
""")
add("y5", "diary", "wing_claude", "diary", """
2026-03-09 | lumen: spent an hour on a focus bug that turned out to be a typo in an aria attribute. Lesson: read the DOM before theorising. Export worker idea parked for tomorrow. ★★
""")

# ---- 10 duplicates (exact or near) ----------------------------------------
NEAR = {
    "b01": """
Bug: the websocket hub in pg_ai leaked goroutines. Each client got a writer goroutine that blocked on its send channel for good after the client went away, and the goroutine count rose by roughly 400 per hour. Fix: the hub cancels a per-client context on disconnect and the writer returns on ctx.Done(). runtime.NumGoroutine() stayed flat for 24 hours afterwards.
""",
    "h07": """
Profiling CPU in pg_ai: start the server with profiling enabled and run `go tool pprof -http=:6060 "http://localhost:<port>/debug/pprof/profile?seconds=30"`. Open View, Flame Graph to get the flame graph.
""",
    "h05": """
Verifying a harbor backup: `harbor verify --deep <archive>` reads each chunk again, compares its BLAKE3 hash with the manifest and test-decrypts a sample. Plain `harbor verify` only checks the manifest signature, which takes about a second.
""",
    "e11": """
Decision: pg_ai logs JSON lines using the standard library slog, not zap. One dependency fewer, and the handler interface is enough for the redaction wrapper. Field names are lower_snake_case and every line includes request_id.
""",
    "b12": """
Bug: modal dialogs rendered behind the sticky header because the header's transform created a stacking context that beat the modal's z-index. Fix: mount modals in a portal on document.body with z-index 1000 and remove the needless transform from the header.
""",
}
EXACT = ["e04", "e09", "e01", "b08", "h06"]
by_key = {d[0]: d for d in D}
for k in EXACT:
    s = by_key[k]
    add(k + "_dup", "duplicate", s[2], s[3], s[5])
for k, text in NEAR.items():
    s = by_key[k]
    add(k + "_dup", "duplicate", s[2], s[3], text)

# ---- 35 transcript chunks in 6 files across 3 projects ----------------------
T = []  # (key, wing, file, text)


def tr(key, wing, file, text):
    T.append((key, wing, file, text.strip()))


FA = "sessions/pg_ai/2026-02-11-a1.jsonl"
FB = "sessions/pg_ai/2026-03-02-b7.jsonl"
FC = "sessions/harbor/2026-01-20-c3.jsonl"
FD = "sessions/harbor/2026-02-25-d8.jsonl"
FE = "sessions/lumen/2026-02-03-e5.jsonl"
FF = "sessions/lumen/2026-03-09-f2.jsonl"

tr("a1", "wing_pg_ai", FA, """
User: I want to clean up cmd/server/main.go before we add anything else. It parses flags by hand and half of them are never used.
Assistant: I counted eleven flags, four of which are never read after parsing. Proposal: move parsing into internal/config, keep only flags that have an environment variable twin, and fail fast on unknown flags instead of ignoring them.
User: Do it, but keep --version.
""")
tr("a2", "wing_pg_ai", FA, """
User: Are we sure about slog? zap is faster in the benchmarks I saw.
Assistant: At a few hundred lines per second the difference is below measurement noise, and slog removes a dependency. The only gap is sampling, which we do not use.
User: Fine, stay on slog and add request_id to every line.
""")
tr("a3", "wing_pg_ai", FA, """
User: TestCompactTwice hangs about one run in five. No panic, no output, just a timeout after ten minutes.
Assistant: I dumped the goroutines with SIGQUIT. One compaction holds the table lock and waits for the manifest lock; the other holds the manifest lock and waits for the table lock. Classic lock-order inversion between compactTable and updateManifest.
User: So pick an order.
Assistant: Table first, then manifest, everywhere. I will add a comment at the top of store.go and a race-enabled test that runs two compactions in parallel.
""")
tr("a4", "wing_pg_ai", FA, """
User: Metrics and debug handlers are currently on the public port. That feels wrong.
Assistant: Agreed. Move them to a separate listener bound to 127.0.0.1 only. I suggest port 7421, which is unused in our compose files and easy to remember next to the public 7400.
User: 7421 is fine. Make it configurable but default to that.
Assistant: Done: the default for the admin listener is 127.0.0.1:7421, overridable with one flag.
""")
tr("a5", "wing_pg_ai", FA, """
User: The golden JSON tests are noisy; any field order change makes them fail.
Assistant: We can compare decoded structures instead of raw bytes and keep byte-level goldens only for the wire format. I will add a helper assertJSONEqual that ignores key order but still checks types.
User: Good, do that and update the three existing tests.
""")
tr("a6", "wing_pg_ai", FA, """
User: Commit message?
Assistant: "refactor: move flag parsing to internal/config and reject unknown flags". The body lists the four removed flags.
User: Ship it. I'll look at the metrics naming tomorrow.
""")
tr("b1", "wing_pg_ai", FB, """
User: CI บน pg_ai ช้าลงเรื่อย ๆ ตอนนี้ปาเข้าไปแปดนาทีแล้ว
Assistant: ดูจากล็อกแล้วขั้นตอนที่กินเวลาคือการดาวน์โหลด Go modules ซ้ำทุกครั้ง ผมจะเพิ่ม cache ของโฟลเดอร์ module โดยใช้ hash ของ go.sum เป็นคีย์
User: ได้ ลองดูว่าเหลือเท่าไหร่
""")
tr("b2", "wing_pg_ai", FB, """
User: What should we call the new counters? I keep mixing styles.
Assistant: Prometheus convention: snake_case, a unit suffix, and _total for counters. So pgai_frames_sent_total, pgai_reconnects_total, pgai_snapshot_bytes. Keep the host out of the name and use labels.
User: Go with those.
""")
tr("b3", "wing_pg_ai", FB, """
User: The reviewer says the tutor client should not retry POSTs.
Assistant: Right, only idempotent calls get retried. I will keep the retry on GET and on POSTs that carry an idempotency key, and add a table test covering both.
User: Reply to the review and push.
""")
tr("b4", "wing_pg_ai", FB, """
User: The SQLite file keeps growing even after we delete old snapshots.
Assistant: Deleted pages go onto the freelist and the file never shrinks by itself. Enable auto_vacuum=INCREMENTAL at creation and run PRAGMA incremental_vacuum(2000) nightly; existing files need one full VACUUM first.
User: Schedule it after the compactor finishes.
""")
tr("b5", "wing_pg_ai", FB, """
User: ช่วยร่างหัวข้อของเอกสารสำหรับคนใหม่ที่เพิ่งเข้าทีมหน่อย
Assistant: เสนอห้าหัวข้อ: ภาพรวมสถาปัตยกรรม วิธีรันในเครื่อง วิธีรันเทสต์ ขั้นตอนส่งโค้ด และใครรับผิดชอบส่วนไหน
User: เพิ่มหัวข้อคำศัพท์ที่ใช้ในทีมด้วย
""")
tr("b6", "wing_pg_ai", FB, """
User: Anything left before I close the laptop?
Assistant: Two loose ends: the CI cache change is not merged yet, and the review reply is waiting on your push. Everything else is committed.
User: Okay, tomorrow.
""")
tr("c1", "harbor", FC, """
User: Nightly full backup of my 12 GB project tree takes too long with xz. Can we do better?
Assistant: xz -6 took about 13 minutes here; zstd -6 finished in about 4 minutes and the archive was roughly 8% bigger. Since the backup has to finish inside the idle window, zstd is the better trade.
User: Use zstd then.
""")
tr("c2", "harbor", FC, """
User: What do we put at the start of an archive so tools can recognise it?
Assistant: A fixed header: the four magic bytes "HRB1" (0x48 0x52 0x42 0x31), then one byte for the format version, currently 3, then the manifest offset as a u64. Anything that does not start with HRB1 is rejected before any decryption is attempted.
User: Good, write that down in the code comments.
""")
tr("c3", "harbor", FC, """
User: Walk me through how files are split into chunks.
Assistant: Content-defined chunking: a rolling hash slides over the file and cuts when the low 20 bits are zero, which gives chunks of about 1 MiB on average. Identical chunks across files and runs are stored once, which is where most of the saving comes from.
User: And the minimum size?
Assistant: 256 KiB minimum, 4 MiB maximum, so odd inputs cannot produce tiny or huge chunks.
""")
tr("c4", "harbor", FC, """
User: The walker follows symlinks into another volume's home directory.
Assistant: By default it should record the symlink itself and not follow it. I will add --follow-symlinks as an opt-in and skip other mount points unless --cross-device is passed.
User: Yes.
""")
tr("c5", "harbor", FC, """
User: Should harbor print JSON for scripts?
Assistant: Add --json to list and verify, one object per line so tools like jq can stream it. Human output stays the default and goes to stderr when --json is set so pipes stay clean.
User: Okay, just list and verify for now.
""")
tr("c6", "harbor", FC, """
User: Wrap up. What is left for the first alpha?
Assistant: Restore of a single path, the prune command, and man pages. Encryption and verify are done. I would tag v0.1.0-alpha after restore lands.
User: Agreed.
""")
tr("d1", "harbor", FD, """
User: Clock skew broke the incremental logic on my NAS: files with timestamps in the future were never picked up.
Assistant: Comparing mtime to the last-run time is fragile. Use the previous manifest instead: a file is unchanged if size, mtime and inode all match what the manifest recorded, regardless of the wall clock.
User: Implement it that way.
""")
tr("d2", "harbor", FD, """
User: ข้อความ error ตอนไฟล์อ่านไม่ได้ยังไม่ชัดเจน ผู้ใช้ไม่รู้ว่าต้องทำอะไรต่อ
Assistant: เสนอให้บอกสามอย่าง: ไฟล์ไหน เกิดอะไรขึ้น และทางแก้ที่ลองได้ เช่น "อ่านไม่ได้: Permission denied ลองรันด้วยสิทธิ์ที่อ่านโฟลเดอร์นี้ได้ หรือเพิ่มใน --exclude"
User: เอาตามนั้น
""")
tr("d3", "harbor", FD, """
User: The restore command overwrote a file I had edited since the backup.
Assistant: Restore should refuse to overwrite by default. I will add --overwrite and, for the default case, write to <name>.restored next to the original and print the list at the end.
User: Good, safer.
""")
tr("d4", "harbor", FD, """
User: Benchmark numbers for the README?
Assistant: On the 12 GB tree the first backup takes 4m10s, the second with nothing changed 9s, and the third with 2% changed 31s. A full restore takes 3m40s on the same disk. I would quote the three backup figures and say restore is limited by the disk.
User: Put them in a table.
""")
tr("d5", "harbor", FD, """
User: Is the work directory cleaned up if harbor is killed?
Assistant: Not today. I will create work files under <archive>.partial and delete them on startup if the lock is not held, so a crash never leaves garbage behind.
User: And rename to the final name at the very end?
Assistant: Yes, an atomic rename, so a half-written archive never has the final name.
""")
tr("d6", "harbor", FD, """
User: What did we decide about Windows?
Assistant: Not supported for 0.x. Paths use Unix semantics and the file lock relies on flock. We can revisit when someone needs it.
User: Note it in the README.
""")
tr("e1", "lumen", FE, """
User: The webpack dev server takes ages to start and a production build is over a minute and a half. Is Vite worth migrating to?
Assistant: I tried it on a branch: the cold production build went from 94 seconds to 11, and edits show in the browser in under 300 ms instead of about 3 seconds. The only snag was an SVG loader that needed a replacement plugin.
User: Then migrate.
""")
tr("e2", "lumen", FE, """
User: After a deploy the smoke tests sometimes fail on the first run and pass on the second.
Assistant: The CDN needs time to invalidate old assets; in our measurements it takes about 90 seconds to propagate. The deploy script should sleep for two minutes before starting the smoke tests instead of retrying blindly.
User: Add the two-minute wait.
""")
tr("e3", "lumen", FE, """
User: The KPI cards look cramped on tablets.
Assistant: Below 900 px the grid switches to two columns, but the card padding stays at 24 px. I will use clamp() for the padding and let the value font scale with container width.
User: Try it and send me a screenshot.
""")
tr("e4", "lumen", FE, """
User: ตอนสลับ dark mode กับ light mode หน้าจอกระพริบขาวแวบหนึ่ง
Assistant: เกิดเพราะอ่านธีมจาก localStorage หลังจาก React โหลดเสร็จ ควรใส่สคริปต์เล็ก ๆ ใน head ที่ตั้ง attribute data-theme ก่อนวาดหน้าแรก
User: ลองทำเลย
""")
tr("e5", "lumen", FE, """
User: Table sorting is slow with 5,000 rows.
Assistant: Sorting itself is fast; the cost is rendering every row. Use virtualization so only the roughly 30 visible rows exist in the DOM, and memoise the row component.
User: Use TanStack Virtual since we already depend on TanStack Query.
""")
tr("e6", "lumen", FE, """
User: Wrap up the session.
Assistant: Done today: the Vite migration branch and the tablet padding fix. Open: the dark-mode flash and table virtualization. Nothing is deployed yet.
User: Fine.
""")
tr("f1", "lumen", FF, """
User: Can we show relative times like "3 minutes ago" in the activity feed?
Assistant: Use Intl.RelativeTimeFormat; it handles plural rules per locale, including Thai. Refresh the labels every 30 seconds with one shared timer rather than one per row.
User: Single timer, good.
""")
tr("f2", "lumen", FF, """
User: The export-to-CSV button freezes the tab for large reports.
Assistant: Build the CSV in a Web Worker and stream it into a Blob in chunks of 5,000 rows; the main thread only receives progress messages.
User: Do it, and add a cancel button.
""")
tr("f3", "lumen", FF, """
User: ตัวกรองวันที่ในหน้ารายงานเลือกช่วงข้ามเดือนไม่ได้
Assistant: ปัญหาอยู่ที่คอมโพเนนต์ปฏิทินที่ล็อกไว้เดือนเดียว ผมจะเปลี่ยนไปใช้ range picker ที่แสดงสองเดือนพร้อมกัน
User: โอเค
""")
tr("f4", "lumen", FF, """
User: The accessibility audit found that the modal does not trap focus.
Assistant: Add focus trapping and restore focus to the opener on close, and set aria-modal plus a labelled title. Escape should close it.
User: Add tests for keyboard navigation too.
""")
tr("f5", "lumen", FF, """
User: Anything blocking tomorrow's release?
Assistant: Only the export fix. Everything else is reviewed. I will post the checklist in the channel tonight.
User: Great.
""")

# ---- 20 code-file chunks ----------------------------------------------------
C = []


def code(key, wing, file, text):
    C.append((key, wing, file, text.strip("\n")))


code("c01", "pg_ai", "internal/store/writer_lease.go", r'''
// Single-writer fencing for the snapshot store.
const leaseTTL = 20 * time.Second

func (s *Store) acquireLease(ctx context.Context, holder string) error {
	now := s.clock.Now()
	res, err := s.db.ExecContext(ctx,
		`UPDATE meta SET value = ?, expires_at = ?
		 WHERE key = 'writer_lease' AND (expires_at < ? OR value = ?)`,
		holder, now.Add(leaseTTL).Unix(), now.Unix(), holder)
	if err != nil {
		return fmt.Errorf("acquire lease: %w", err)
	}
	if n, _ := res.RowsAffected(); n == 0 {
		return errLeaseHeld
	}
	return nil
}
''')
code("c02", "pg_ai", "internal/store/lease_renew.go", r'''
var ErrLeaseExpired = errors.New("store: lease expired, writes are fenced")

// renew extends the lease; the caller must stop writing if it fails.
func (s *Store) renew(ctx context.Context) error {
	if s.clock.Now().After(s.held.expires) {
		return ErrLeaseExpired
	}
	if err := s.acquireLease(ctx, s.held.holder); err != nil {
		return err
	}
	s.held.expires = s.clock.Now().Add(leaseTTL)
	return nil
}
''')
code("c03", "pg_ai", "cmd/server/main.go", r'''
func main() {
	cfg, err := config.Load(os.Args[1:], os.Getenv)
	if err != nil {
		fmt.Fprintln(os.Stderr, "config:", err)
		os.Exit(2)
	}
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	if err := run(ctx, cfg); err != nil && !errors.Is(err, context.Canceled) {
		slog.Error("server stopped", "err", err)
		os.Exit(1)
	}
}
''')
code("c04", "pg_ai", "internal/manifest/assets.go", r'''
type Asset struct {
	Path   string `json:"path"`
	Size   int64  `json:"size"`
	SHA256 string `json:"sha256"`
}

type Manifest struct {
	mu     sync.RWMutex
	assets map[string]Asset
}

func (m *Manifest) Lookup(path string) (Asset, bool) {
	m.mu.RLock()
	defer m.mu.RUnlock()
	a, ok := m.assets[path]
	return a, ok
}
''')
code("c05", "pg_ai", "internal/config/load.go", r'''
func envInt(get func(string) string, name string, def int) (int, error) {
	raw := get(name)
	if raw == "" {
		return def, nil
	}
	n, err := strconv.Atoi(raw)
	if err != nil || n < 0 {
		return 0, fmt.Errorf("%s: want a non-negative integer, got %q", name, raw)
	}
	return n, nil
}
''')
code("c06", "harbor", "src/archive/manifest.rs", r'''
#[derive(Debug, Serialize, Deserialize)]
pub struct BackupManifestV3 {
    pub archive_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub chunks: Vec<ChunkRef>,
    pub exclusions: Vec<String>,
}

impl BackupManifestV3 {
    pub fn total_bytes(&self) -> u64 {
        self.chunks.iter().map(|c| c.len as u64).sum()
    }
}
''')
code("c07", "harbor", "src/crypto/recipients.rs", r'''
pub fn recipients_from_env() -> Result<Vec<age::x25519::Recipient>> {
    let raw = std::env::var("HARBOR_AGE_RECIPIENTS").unwrap_or_default();
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.parse().map_err(|e| anyhow!("bad recipient {s:?}: {e}")))
        .collect()
}
''')
code("c08", "harbor", "src/archive/chunker.rs", r'''
const MIN_CHUNK: usize = 256 * 1024;
const MAX_CHUNK: usize = 4 * 1024 * 1024;
const MASK: u64 = (1 << 20) - 1;

pub fn next_cut(buf: &[u8], seed: u64) -> usize {
    let mut h = seed;
    for (i, &b) in buf.iter().enumerate().take(MAX_CHUNK) {
        h = h.rotate_left(1) ^ GEAR[b as usize];
        if i >= MIN_CHUNK && h & MASK == 0 {
            return i + 1;
        }
    }
    buf.len().min(MAX_CHUNK)
}
''')
code("c09", "harbor", "src/walk.rs", r'''
pub fn walk(root: &Path, opts: &WalkOpts) -> impl Iterator<Item = Result<Entry>> {
    WalkDir::new(root)
        .follow_links(opts.follow_symlinks)
        .same_file_system(!opts.cross_device)
        .into_iter()
        .filter_entry(move |e| !opts.excluded(e.path()))
        .map(|r| r.map(Entry::from).map_err(Into::into))
}
''')
code("c10", "harbor", "src/cli/args.rs", r'''
#[derive(Parser)]
#[command(name = "harbor", version)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,
    /// Print machine-readable output, one JSON object per line.
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Subcommand)]
pub enum Cmd {
    Backup { source: PathBuf, dest: PathBuf },
    List { archive: PathBuf },
    Verify { archive: PathBuf, #[arg(long)] deep: bool },
}
''')
code("c11", "lumen", "web/src/api/client.ts", r'''
export async function api<T>(path: string, init: RequestInit = {}): Promise<T> {
  const res = await fetch(`${API_BASE}${path}`, {
    ...init,
    headers: { "Content-Type": "application/json", ...init.headers },
    credentials: "include",
  });
  if (!res.ok) {
    throw new ApiError(res.status, await res.text());
  }
  return (await res.json()) as T;
}
''')
code("c12", "lumen", "web/src/components/KpiCard.tsx", r'''
export function KpiCard({ label, value, delta, sparkline }: KpiCardProps) {
  const tone = delta === 0 ? "flat" : delta > 0 ? "up" : "down";
  return (
    <section className={`kpi kpi--${tone}`} aria-label={label}>
      <h3>{label}</h3>
      <p className="kpi__value">{value}</p>
      <p className="kpi__delta">{formatDelta(delta)}</p>
      {sparkline && <Sparkline points={sparkline} />}
    </section>
  );
}
''')
code("c13", "lumen", "web/src/lib/formatCurrency.ts", r'''
const formatters = new Map<string, Intl.NumberFormat>();

export function formatCurrency(amount: number, currency = "THB", locale = "th-TH") {
  const key = `${locale}:${currency}`;
  let f = formatters.get(key);
  if (!f) {
    f = new Intl.NumberFormat(locale, { style: "currency", currency });
    formatters.set(key, f);
  }
  return f.format(amount);
}
''')
code("c14", "lumen", "web/src/store/filters.ts", r'''
export const useFilters = create<FiltersState>()((set) => ({
  range: "30d",
  segment: "all",
  setRange: (range) => set({ range }),
  setSegment: (segment) => set({ segment }),
  reset: () => set({ range: "30d", segment: "all" }),
}));
''')
code("c15", "lumen", "web/src/hooks/useLocalStorage.ts", r'''
export function useLocalStorage<T>(key: string, initial: T) {
  const [value, setValue] = useState<T>(() => {
    try {
      const raw = window.localStorage.getItem(key);
      return raw === null ? initial : (JSON.parse(raw) as T);
    } catch {
      return initial;
    }
  });
  useEffect(() => {
    try { window.localStorage.setItem(key, JSON.stringify(value)); } catch { /* quota or private mode */ }
  }, [key, value]);
  return [value, setValue] as const;
}
''')
code("c16", "personal_dev", "scripts/seed_demo_data.py", r'''
def make_accounts(rng, n):
    first = ["Ada", "Ben", "Cleo", "Dev", "Eli", "Fay", "Gus", "Hana"]
    for i in range(n):
        yield {
            "id": i + 1,
            "name": f"{rng.choice(first)} Test{i:03d}",
            "email": f"user{i:03d}@example.test",
            "plan": rng.choices(["free", "pro", "team"], weights=[6, 3, 1])[0],
        }
''')
code("c17", "personal_dev", "tools/check_links.py", r'''
LINK = re.compile(r"\[[^\]]+\]\(([^)#\s]+)(?:#[^)]*)?\)")

def broken_links(md_path: Path):
    text = md_path.read_text(encoding="utf-8")
    for target in LINK.findall(text):
        if "://" in target:
            continue
        if not (md_path.parent / target).exists():
            yield target
''')
code("c18", "personal_dev", "scripts/summarize_logs.py", r'''
def summarize(lines):
    counts = collections.Counter()
    for line in lines:
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            counts["unparsable"] += 1
            continue
        counts[rec.get("level", "info")] += 1
    return dict(counts)
''')
code("c19", "harbor", ".github/workflows/ci.yml", r'''
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - run: cargo test --all-features
  lint:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: cargo clippy --all-targets -- -D warnings
''')
code("c20", "personal_dev", "scripts/dev_up.sh", r'''
#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
docker compose up -d db cache
until docker compose exec -T db pg_isready -q; do sleep 1; done
echo "database ready"
''')

# --------------------------------------------------------------------------
# Queries.  relevant maps drawer KEY -> grade (resolved to ids at the end).
# --------------------------------------------------------------------------
Q = []


def q(qid, text, category, lexical_ok, relevant):
    Q.append({"id": qid, "q": text, "category": category, "lexical_ok": lexical_ok, "relevant": relevant})


q("q01", "Why did we choose zstd instead of xz for backup compression?", "fact", True, {"e02": 2, "c1": 2})
q("q02", "What is the rate limit per API token?", "fact", True, {"e04": 2, "e04_dup": 2})
q("q03", "How many monthly snapshots does the harbor retention policy keep?", "fact", True, {"e09": 2, "e09_dup": 2})
q("q04", "How do I rotate API tokens with a grace period?", "fact", True, {"h04": 2})
q("q05", "What caused the goroutine leak in the websocket hub?", "fact", True, {"b01": 2, "b01_dup": 2})
q("q06", "Why did dates show up one day early for users in UTC-8?", "fact", True, {"b06": 2})
q("q07", "How do I cross-compile harbor for Linux ARM from macOS?", "fact", True, {"h02": 2})
q("q08", "Which exit code means another backup holds the lock?", "fact", True, {"e12": 2})
q("q09", "How much did moving to Vite cut the build time?", "fact", True, {"e10": 2, "e1": 2})
q("q10", "How do I run the k6 load test and with how many virtual users?", "fact", True, {"h11": 2})
q("q11", "What was the deadlock in the snapshot writer?", "fact", True, {"b10": 2, "a3": 2})
q("q12", "Which library replaced Moment.js in the dashboard?", "fact", True, {"e06": 2})
q("q13", "How do I profile CPU usage in pg_ai with pprof?", "fact", True, {"h07": 2, "h07_dup": 2})
q("q14", "Why did the chart flicker when the window was resized?", "fact", True, {"b03": 2})

q("q15", "เหตุผลที่ย้ายที่เก็บเซสชันออกจากหน่วยความจำในโปรเซส", "thai", True, {"t01": 2})
q("q16", "ปีขึ้นเป็นปฏิทินพุทธทั้งที่ต้องการ ค.ศ. แก้ยังไง", "thai", True, {"t08": 2})
q("q17", "ดิสก์เต็มตอนระบบล่มคืนวันเสาร์เพราะอะไร", "thai", True, {"t14": 2})
q("q18", "อัปโหลดไฟล์ได้สูงสุดกี่เมกะไบต์", "thai", True, {"t12": 2})
q("q19", "ตัวหนังสือแสดงผลผิดปกติในเอกสารพร้อมพิมพ์ที่ระบบทำออกมา", "thai", False, {"t06": 2})
q("q20", "งบคลาวด์รายเดือนตั้งเพดานไว้เท่าไร", "thai", True, {"t25": 2})
q("q21", "ผู้ใช้กดปุ่มบันทึกซ้ำแล้วเกิดข้อมูลซ้ำ แก้ยังไง", "thai", True, {"t03": 2})
q("q22", "ย้อนสิ่งที่เผลอพุชไปแล้วโดยไม่ลบประวัติ", "thai", True, {"t13": 2})

q("q23", "composite index ของ user_id กับ created_at ช่วยให้ query เร็วขึ้นกี่เท่า", "mixed", True, {"m04": 2})
q("q24", "JWT access token หมดอายุกี่นาที", "mixed", True, {"m06": 2})
q("q25", "webhook retry ใช้ backoff แบบไหน", "mixed", True, {"m09": 2})
q("q26", "Docker image เหลือกี่ MB หลังทำ multi-stage build", "mixed", True, {"m07": 2})
q("q27", "feature flag new_checkout เปิดให้ผู้ใช้กี่เปอร์เซ็นต์ก่อน", "mixed", True, {"m01": 2})
q("q28", "แก้ปัญหา N+1 ใน GraphQL ด้วยอะไร", "mixed", True, {"m15": 2})

q("q29", "HostLinkReconnect", "identifier", True, {"i01": 2})
q("q30", "writer_lease", "identifier", True, {"i02": 2, "c01": 1})
q("q31", "internal/teacher/jev/client.go", "identifier", True, {"i03": 2})
q("q32", "ErrLeaseExpired", "identifier", True, {"i04": 2, "c02": 1})
q("q33", "HARBOR_AGE_RECIPIENTS", "identifier", True, {"i05": 2, "c07": 1})
q("q34", "BackupManifestV3", "identifier", True, {"i06": 2, "c06": 1})
q("q35", "what does max_inflight_frames do", "identifier", True, {"i07": 2})
q("q36", "X-Request-Shard", "identifier", True, {"i08": 2})

q("q37", "Why do we avoid renting a toggle-management product from outside?", "paraphrase", False, {"e13": 2})
q("q38", "The preferences screen locked up repainting itself non-stop; what was the underlying reason?", "paraphrase", False, {"b09": 2})
q("q39", "Why was reinstating data onto my Mac rejected?", "paraphrase", False, {"b05": 2})
q("q40", "Did we ever consider breaking up the codebase into several version-control projects?", "paraphrase", False, {"e07": 2})
q("q41", "Why did our network connections to a failing dependency never give up?", "paraphrase", False, {"b04": 2})
q("q42", "What does it take to get a new edition of the backup tool out to end users?", "paraphrase", False, {"h09": 2})

q("q43", "ทำไมถึงเลือกเครื่องมือเข้ารหัสสำหรับสำรองข้อมูลตัวใหม่แทนเครื่องมือเก่า", "cross_lingual", False, {"e05": 2})
q("q44", "ทำไมไม่ใช้ตัวจัดการสถานะส่วนกลางแบบเดิมสำหรับข้อมูลที่ดึงมาจากเซิร์ฟเวอร์", "cross_lingual", False, {"e03": 2})
q("q45", "ดีบักการทดสอบเบราว์เซอร์โดยดูหน้าจอจริงทีละขั้น", "cross_lingual", False, {"h03": 2})

q("q46", "Which port did we pick for the admin listener?", "raw_only", True, {"a4": 2})
q("q47", "What magic bytes begin a harbor archive file?", "raw_only", True, {"c2": 2})
q("q48", "How long should the deploy script wait before running the smoke tests?", "raw_only", True, {"e2": 2})


# --------------------------------------------------------------------------
def build():
    rng = random.Random(SEED)
    rows = []  # (key, wing, room, source_file, kind, text, chunk_index)
    for key, kind, wing, room, src, text in D:
        rows.append((key, wing, room, src, kind, text, None))
    idx = {}
    for key, wing, file, text in T:
        i = idx.get(file, 0)
        idx[file] = i + 1
        rows.append((key, wing, "transcript", file, "transcript", text, i))
    for key, wing, file, text in C:
        rows.append((key, wing, "code", file, "code", text, 0))
    rng.shuffle(rows)
    key2id = {}
    drawers = []
    for n, (key, wing, room, src, kind, text, chunk) in enumerate(rows, 1):
        did = f"g{n:03d}"
        key2id[key] = did
        added_by = "claude" if kind in ("diary", "transcript") else rng.choice(["claude", "user", "user"])
        day = rng.randint(0, 80)
        hh, mm = rng.randint(7, 22), rng.randint(0, 59)
        own = re.search(r"\d{4}-\d{2}-\d{2}", text if kind == "diary" else (src or "") if kind == "transcript" else "")
        if own:
            date = own.group(0)
        else:
            date = f"2026-{day // 28 + 1:02d}-{day % 28 + 1:02d}"
        created = f"{date}T{hh:02d}:{mm:02d}:00Z"
        meta = {"wing": wing, "room": room, "added_by": added_by, "source_file": src, "kind": kind, "created_at": created}
        if chunk is not None:
            meta["chunk_index"] = chunk
        drawers.append({"id": did, "text": text, "meta": meta})
    queries = []
    for qq in Q:
        rel = {}
        for k, g in qq["relevant"].items():
            rel[key2id[k]] = g
        queries.append({**qq, "relevant": dict(sorted(rel.items()))})
    return {"drawers": drawers, "queries": queries}, key2id


# --------------------------------------------------------------------------
# Audit (mechanical re-check of the grades; the report table comes from here)
# --------------------------------------------------------------------------
STOP = set("""a an the of to in on for and or is are was were be been do does did we our us i it its this that these those
with from by at as how what why which who when where should would could can will not no any into than then so if
about did does have has had get got use using used one two per""".split())


def stem(w):
    for suf in ("ing", "ed", "es", "s"):
        if w.endswith(suf) and len(w) - len(suf) >= 3:
            return w[: -len(suf)]
    return w


def latin_terms(text):
    return {stem(w) for w in re.findall(r"[a-z0-9_]+", text.lower()) if w not in STOP and len(w) > 1}


def thai_grams(text, n=4):
    out = set()
    for seg in re.findall(r"[฀-๿]+", text):
        for i in range(len(seg) - n + 1):
            out.add(seg[i : i + n])
    return out


def ident_of(query):
    toks = re.findall(r"[A-Za-z0-9_./-]+", query)
    odd = [t for t in toks if "_" in t or "/" in t or "-" in t or t[1:] != t[1:].lower()]
    return (odd or toks)[-1].lower()


def audit(corpus, key2id):
    id2key = {v: k for k, v in key2id.items()}
    docs = {d["id"]: d["text"] for d in corpus["drawers"]}
    problems = []
    lines = []
    for qq in corpus["queries"]:
        ql, qt = latin_terms(qq["q"]), thai_grams(qq["q"])
        graded = qq["relevant"]
        cands = []
        for did, text in docs.items():
            if did in graded:
                continue
            lo, th = ql & latin_terms(text), qt & thai_grams(text)
            strong = len(lo) >= 2 or len(th) >= 3 or (qq["category"] == "identifier" and ident_of(qq["q"]) in text.lower())
            if strong:
                cands.append(f"{id2key[did]}({did}):{','.join(sorted(lo))}{'+th%d' % len(th) if th else ''}")
        shared_g2 = [
            (len(ql & latin_terms(docs[d])), len(qt & thai_grams(docs[d]))) for d, g in graded.items() if g == 2
        ]
        cat = qq["category"]
        if cat in ("paraphrase", "cross_lingual"):
            for d in graded:
                if ql & latin_terms(docs[d]):
                    problems.append(f"{qq['id']}: shares Latin words {ql & latin_terms(docs[d])} with {id2key[d]}")
                if qt & thai_grams(docs[d]):
                    problems.append(f"{qq['id']}: shares Thai grams with {id2key[d]}")
            if cat == "cross_lingual" and re.search(r"[A-Za-z]", qq["q"]):
                problems.append(f"{qq['id']}: Latin letters in a cross-lingual query")
        elif cat == "thai":
            shared = any(qt & thai_grams(docs[d]) for d, g in graded.items() if g == 2)
            if shared != qq["lexical_ok"]:
                problems.append(f"{qq['id']}: Thai lexical_ok={qq['lexical_ok']} but n-gram share={shared}")
        elif qq["lexical_ok"]:
            if not any(a >= 1 or b >= 1 for a, b in shared_g2) or (cat == "fact" and not any(a >= 2 for a, _ in shared_g2)):
                problems.append(f"{qq['id']}: lexical_ok but too little overlap {shared_g2}")
        else:
            if any(a >= 2 for a, _ in shared_g2):
                problems.append(f"{qq['id']}: lexical_ok false but overlap {shared_g2}")
        if cat == "identifier":
            ident = ident_of(qq["q"])
            holders = {d for d, t in docs.items() if ident in t.lower()}
            if holders != set(graded):
                problems.append(f"{qq['id']}: identifier holders {sorted(id2key[h] for h in holders)} != graded {sorted(id2key[g] for g in graded)}")
        lines.append(
            f"| {qq['id']} | {cat} | {'yes' if qq['lexical_ok'] else 'no'} | "
            + ", ".join(f"{id2key[d]}={g}" for d, g in graded.items())
            + (" [shared th: " + "/".join(sorted(qt & thai_grams(docs[next(iter(graded))]))) + "]" if cat in ("thai", "cross_lingual") and not qq["lexical_ok"] else "")
            + " | "
            + ("; ".join(cands) if cands else "none")
            + " |"
        )
    return lines, problems


def main():
    corpus, key2id = build()
    # structural invariants from the brief
    assert len(corpus["queries"]) == 48
    kinds = {}
    for d in corpus["drawers"]:
        kinds[d["meta"]["kind"]] = kinds.get(d["meta"]["kind"], 0) + 1
    assert kinds == {
        "knowledge_en": 40, "knowledge_th": 25, "knowledge_mixed": 15, "identifier": 20,
        "transcript": 35, "code": 20, "duplicate": 10, "diary": 5,
    }, kinds
    assert len({d["meta"]["source_file"] for d in corpus["drawers"] if d["meta"]["kind"] == "transcript"}) == 6
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(json.dumps(corpus, ensure_ascii=False, indent=1) + "\n", encoding="utf-8")
    print(f"wrote {OUT} ({len(corpus['drawers'])} drawers, {len(corpus['queries'])} queries)")
    lines, problems = audit(corpus, key2id)
    if "--audit" in sys.argv:
        print("| query | category | lexical_ok | graded | other candidates (>=2 shared content words) |")
        print("|---|---|---|---|---|")
        print("\n".join(lines))
    if problems:
        print("AUDIT PROBLEMS:")
        print("\n".join(problems))
        sys.exit(1)
    print("audit: no mechanical problems")


if __name__ == "__main__":
    main()
