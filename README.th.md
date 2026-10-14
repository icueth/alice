<h1 align="center">Alice</h1>

<p align="center">
  <b>ความจำระยะยาวสำหรับ AI coding agent</b><br>
  ความจำเดียวที่เร็วและอยู่ในเครื่อง ใช้ร่วมกันได้ทั้ง Claude Code, Codex, Cursor, Windsurf, OpenCode และ Gemini CLI
</p>

<p align="center">
  <i>A.L.I.C.E. — Agent Long-term Intelligent Context Engine</i>
</p>

<p align="center">
  <a href="README.md">English</a> ·
  <a href="docs/superpowers/specs/2026-10-13-alice-core-design.md">Design spec</a> ·
  <a href="#roadmap">Roadmap</a>
</p>

> [!IMPORTANT]
> **สถานะ: M1 เสร็จแล้ว — รุ่น alpha** ที่เก็บข้อมูล, การค้นหาแบบผสม (ไทย อังกฤษ และชื่อในโค้ด), daemon, MCP tool ทั้ง 7 ตัว, CLI และ HTTP API ใช้งานได้แล้ว CI build และทดสอบบน macOS และ Linux ส่วนบน Windows นั้น CI build ทั้งหมดและรันชุดทดสอบแบบ in-process แต่การทดสอบ CLI และ process ของ daemon รันเฉพาะบน Unix (ดู[แพลตฟอร์มที่รองรับ](#แพลตฟอร์มที่รองรับ)) ยังไม่มี release แบบแพ็กเกจ ให้[ติดตั้งจาก source](#ติดตั้งจาก-source) ความสามารถที่มีป้าย *(วางแผนไว้)* จะตามมาใน milestone ถัดไป ดูได้ที่ [roadmap](#roadmap) ก่อนถึง 1.0 อาจมีการเปลี่ยนที่ไม่เข้ากับของเดิม

---

## ทำไมต้อง Alice

AI coding agent ลืมทุกอย่างเมื่อจบ session เครื่องมือด้านความจำช่วยแก้ปัญหานี้ได้ แต่พอเปิด agent หลายตัวพร้อมกันจะเจอปัญหาใหม่ เช่น เรียก memory แล้วค้างเพราะอีก session กำลังเขียนอยู่, ข้อมูลซ้ำหรือถูกเก็บผิดที่, transcript ดิบกลบความรู้ที่ต้องการจริงๆ และไม่มีทางดูหรือแก้ได้ง่ายๆ ว่า agent จำอะไรไว้บ้าง

Alice ออกแบบมาเพื่อการทำงานแบบนี้โดยเฉพาะ คือ agent หลายตัว โปรเจกต์หลายโปรเจกต์ แต่ใช้ความจำเดียวที่เร็ว สะอาด และถูกต้องเสมอ

## จุดเด่น

- **agent ไม่ต้องรอ memory อีก** มี daemon ตัวเดียวเป็นเจ้าของฐานข้อมูล การเขียนตอบกลับภายใน 50ms ส่วนงานหนักอย่าง embedding และการทำ index ทำเบื้องหลังทั้งหมด ถ้า daemon ไม่ได้รันหรือยังเปิดไม่เสร็จ การเขียนจะพักลงดิสก์ไว้ก่อน ข้อมูลไม่หาย
- **ความรู้มาก่อน transcript ดิบเรียกเมื่อต้องการ** ข้อมูลมี 2 ชั้น ชั้นแรกคือ *memory* สั้นๆ ที่กลั่นแล้ว (decision, การแก้บั๊ก, fact, how-to, preference, warning, handoff) ซึ่งค้นหาเป็นค่าเริ่มต้น ชั้นที่สองคือ transcript ดิบที่เก็บไว้เป็นหลักฐาน ค้นเมื่อขอหรือเมื่อชั้นแรกมีผลไม่พอ *(การเก็บ transcript และกลั่นความรู้อัตโนมัติ: วางแผนไว้ใน SP2)*
- **ค้นเข้าใจทั้งโค้ดและภาษาไทย** รวมการค้น 4 แบบ: vector หลายภาษา, BM25, trigram สำหรับภาษาที่ไม่เว้นวรรค (ไทย จีน ญี่ปุ่น) และการค้นที่เข้าใจชื่อในโค้ด (`HostLinkReconnect`, `writer_lease`, path ไฟล์) ผลค้นหาให้น้ำหนักกับโปรเจกต์ปัจจุบัน ความสดใหม่ ความสำคัญ และสถานะ
- **ความจำแก้ตัวเองได้** *(วางแผนไว้ใน M2)* memory ใหม่จะถูกเทียบกับของเดิมที่คล้ายกัน ถ้าซ้ำจะรวมกัน ถ้าเป็นข้อมูลที่ใหม่กว่าจะแทนที่ (supersede) ของเก่า ถ้าขัดแย้งกันจะเข้าคิว review ทุกการเปลี่ยนแปลงถูกบันทึกไว้และ undo ได้
- **มี AI Judge ให้เลือกเปิด** *(วางแผนไว้ใน M2)* ต่อกับ [TypeSafe Jev](https://typesafe.ai) เพื่อช่วยตัดสินใจ (กันซ้ำ, จัดเข้าโปรเจกต์, เรียงผลค้นหา) ด้วยความน่าจะเป็นที่เชื่อถือได้ ตั้งเกณฑ์ความมั่นใจได้ มี cache และจำกัดงบรายวัน ถ้าไม่เปิด Alice จะใช้กฎที่ทำงานในเครื่องแทน
- **MCP tool น้อยแต่ตรงจุด** มี tool หลัก 7 ตัว (`recall`, `remember`, `context`, `get_memory`, `update_memory`, `query_facts`, `assert_fact`) แทนที่จะมีหลายสิบตัว ผลลัพธ์กระชับและคุมจำนวน token ได้ มีคำแนะนำการใช้งานในตัว พร้อม slash command และ resource
- **ได้สรุปสั้น ไม่ใช่ข้อมูลทั้งก้อน** `context` สรุปสิ่งสำคัญของโปรเจกต์ปัจจุบันให้อยู่ในงบ token ที่กำหนด: memory หลัก, decision ล่าสุด, warning, handoff ล่าสุด และ preference ของคุณ
- **Knowledge graph ที่รู้เรื่องเวลา** เก็บ entity และ relation พร้อมช่วงเวลาที่ข้อมูลยังเป็นจริง ถามย้อนเวลาได้ด้วย `as_of` และดูเป็น timeline ได้ มีพจนานุกรม predicate กันไม่ให้เกิด `has_bug` กับ `had_bug` ปนกัน
- **Git sync เป็นไฟล์ Markdown** *(วางแผนไว้ใน M4)* push/pull ความจำไปที่ git remote ไหนก็ได้ เก็บ memory ละหนึ่งไฟล์ Markdown เปิดอ่านบน GitHub ได้ เปิดใน Obsidian ได้ และ sync ข้ามเครื่องได้โดย merge ทีละ record ไฟล์ฐานข้อมูลไม่เคยถูกใส่ใน git
- **ไฟล์เดียวจบ** daemon, CLI และ MCP server (และ hook ใน SP2) อยู่ใน binary Rust ไฟล์เดียวที่มีตัวรัน embedding ในตัว ไม่ต้องมี Python env หรือ Docker
- **เป็นส่วนตัวเป็นค่าเริ่มต้น** ข้อมูลอยู่ในเครื่อง และ server รับเฉพาะการเชื่อมต่อจากเครื่องตัวเอง โดยตรวจ token และ origin ทุกคำขอ *(วางแผนไว้ใน M2: ตัด secret ออกก่อนบันทึกหรือส่งออกทุกครั้ง และเก็บ API key ในไฟล์ส่วนตัว โดยเลือกใช้ Keychain ของ OS ได้)*
- **UI สำหรับดู graph และจัดการข้อมูล** *(วางแผนไว้ใน SP3)* ดู graph ความจำ แก้ memory ตัดสินรายการใน review ตั้งค่าได้ทุกอย่าง และสั่ง git push/pull ได้จากเบราว์เซอร์หรือแอป desktop
- **ย้ายมาจาก MemPalace ได้** *(วางแผนไว้ใน M3)* import palace เดิมได้ครบทั้ง drawer, diary และ knowledge graph และยังเรียก tool ชื่อ `mempalace_*` ได้ผ่าน compatibility layer ที่เลือกเปิดได้

## หลักการทำงาน

```
 Claude Code / Codex / Cursor / Windsurf / OpenCode / Gemini CLI
   ├─ MCP ── `alice mcp` (stdio) หรือ http://127.0.0.1:<port>/mcp ─┐
   └─ hooks ─ `alice hook <event>` ─────────────────────────────────┤
 Terminal ── `alice recall …` ──────────────────────────────────────┼──► alice daemon
 Browser ─── `alice ui` ────────────────────────────────────────────┘      ├─ writer (ทำแต่ transaction สั้นๆ)
                                                                            ├─ workers: embed · judge · import · sync
                                                                            ├─ scheduler: backup · ตรวจสุขภาพ · เก็บกวาด
                                                                            └─ SQLite (WAL) + FTS5 + HNSW vectors
```

| แนวคิด | ความหมาย |
|---|---|
| **Project** | ตรวจหาเองจาก git repo ของคุณ ชื่อที่เขียนต่างกัน (`my-app`, `my_app`) ถือเป็นโปรเจกต์เดียวกัน ส่วนโปรเจกต์ `global` เก็บ preference ที่ใช้กับทุกโปรเจกต์ |
| **Memory** | ความรู้สั้นๆ ที่จบในตัว มี kind, topic, ความสำคัญ และสถานะ |
| **Source / chunk** | หลักฐานดิบ (transcript, ไฟล์, ข้อมูลที่ import มา) ที่ memory ชี้กลับไปหาได้ |
| **Fact** | ความสัมพันธ์ใน knowledge graph ระหว่าง entity พร้อมช่วงเวลาที่ยังเป็นจริง |
| **Review** | เรื่องที่ระบบมั่นใจไม่พอจะตัดสินเอง จึงรอให้คนตัดสิน |

## ติดตั้งจาก source

ต้องมี Rust 1.99 ขึ้นไป ([rustup](https://rustup.rs)) และ compiler C/C++ (Xcode Command Line Tools บน macOS, `build-essential` บน Debian/Ubuntu, MSVC Build Tools บน Windows)

```bash
cargo install --git https://github.com/icueth/alice alice-memory
alice model fetch      # ครั้งเดียวก่อนเริ่มใช้: ดาวน์โหลดโมเดล embedding (~136MB) ไว้ที่ ~/.alice/models
```

รัน `alice model fetch` หนึ่งครั้งก่อนเริ่มใช้งาน ถ้าไม่รัน daemon จะดาวน์โหลดโมเดลเองตอนเปิดครั้งแรก และระหว่างนั้นคำสั่งอ่านจะตอบว่า "memory daemon is starting (downloading the embedding model …)" ส่วนคำสั่งเขียนจะเข้าคิวไว้ก่อน `alice daemon start` จะรอจนดาวน์โหลดเสร็จและแสดงความคืบหน้า ไฟล์โมเดลล็อกไว้ที่ revision เดียวและตรวจ SHA-256 ทุกครั้ง ตั้ง `HF_ENDPOINT` เพื่อใช้ mirror ของ Hugging Face ได้ ถ้า daemon เปิดไม่ขึ้น (ไม่มีเน็ตให้ดาวน์โหลด หรือ `alice.db` เสียหาย) ทุกคำสั่งจะบอกสาเหตุ และ `alice status` แสดงความล้มเหลวครั้งล่าสุด

### แพลตฟอร์มที่รองรับ

| แพลตฟอร์ม | สถานะ |
|---|---|
| macOS ชิป Apple Silicon | รองรับ, embedding ในเครื่อง |
| Linux (glibc) x86_64 และ aarch64 | รองรับ, embedding ในเครื่อง |
| Windows x86_64 | build ใน CI และรันชุดทดสอบแบบ in-process ส่วนการทดสอบ CLI และ process ของ daemon รันเฉพาะบน Unix; embedding ในเครื่องตรวจแล้วบน Windows ด้วย CI workflow `windows-real-model` (model จริง โดยโฟลเดอร์ข้อมูลอยู่ใต้ path ที่ไม่ใช่ ASCII) ข้อจำกัดที่รู้แล้ว: `alice daemon start` และ `alice status` มองไม่เห็น daemon ที่ยังเริ่มไม่เสร็จ *(วางแผนแก้แล้ว)*; `alice service install` (scheduled task) *(วางแผนไว้)* |
| macOS ชิป Intel, Linux musl | ยังไม่รองรับ: ONNX Runtime ไม่มี library สำเร็จรูปให้ จึงรัน embedding ในเครื่องไม่ได้ จะใช้ได้เมื่อมี embedding provider ผ่าน HTTP (วางแผนไว้ใน M2) |

### ตัวติดตั้งแบบแพ็กเกจ (วางแผนไว้)

```bash
# Homebrew (macOS, Linux)
brew install icueth/tap/alice

# สคริปต์ติดตั้ง (macOS, Linux)
curl -fsSL https://github.com/icueth/alice/releases/latest/download/alice-installer.sh | sh

# PowerShell (Windows)
irm https://github.com/icueth/alice/releases/latest/download/alice-installer.ps1 | iex

# npm (ถ้ามี Node อยู่แล้วจากการใช้ agent)
npm install -g alice-memory

# Cargo (จาก crates.io)
cargo install alice-memory
```

## เริ่มใช้งาน

```bash
alice model fetch              # ครั้งเดียว: โมเดล embedding (~136MB)
alice remember --kind decision "ใช้ SQLite WAL กับ writer thread ตัวเดียว"   # เปิด daemon ให้เองถ้ายังไม่รัน
alice recall "ทำไมใช้ writer ตัวเดียว"
alice context                  # สรุปของโปรเจกต์ปัจจุบัน
alice status                   # สถานะ daemon, จำนวนข้อมูล, คิว embedding
```

โปรเจกต์ตรวจหาจาก directory ที่ทำงานอยู่ (git remote หรือชื่อ directory) memory ที่เขียนนอกโปรเจกต์ใดๆ จะเข้า `global` ที่วางแผนไว้: `alice setup` ต่อ agent ทุกตัวในเครื่องให้อัตโนมัติ (SP2), `alice import mempalace` (M3) และ `alice ui` (SP3)

## การเชื่อมต่อ agent

ลงทะเบียน `alice mcp` เป็น MCP server แบบ stdio (ใน SP2 `alice setup` จะทำให้อัตโนมัติ) ตัว shim จะเปิด daemon ให้เองเมื่อจำเป็น และถ้า client ส่ง workspace root มา โปรเจกต์จะยึดตาม root นั้น:

<details>
<summary><b>Claude Code</b></summary>

```bash
claude mcp add --scope user alice -- alice mcp
```
</details>

<details>
<summary><b>Codex CLI</b> — <code>~/.codex/config.toml</code></summary>

```toml
[mcp_servers.alice]
command = "alice"
args = ["mcp"]
```
</details>

<details>
<summary><b>Cursor</b> — <code>~/.cursor/mcp.json</code> · <b>Windsurf</b> — <code>~/.codeium/windsurf/mcp_config.json</code> · <b>Gemini CLI</b> — <code>~/.gemini/settings.json</code></summary>

```json
{
  "mcpServers": {
    "alice": { "command": "alice", "args": ["mcp"] }
  }
}
```
</details>

<details>
<summary><b>OpenCode</b> — <code>opencode.json</code></summary>

```json
{
  "mcp": {
    "alice": { "type": "local", "command": ["alice", "mcp"] }
  }
}
```
</details>

agent ที่ใช้ MCP ไม่ได้ เรียกผ่าน CLI แทนได้ (`alice recall`, `alice remember` และใส่ `--json` เพื่อรับผลแบบมีโครงสร้าง)

## MCP tools

| Tool | หน้าที่ |
|---|---|
| `recall` | ค้นหา memory กรองตามโปรเจกต์, topic, kind และช่วงวันได้ ใส่ `include_raw` เพื่อค้น transcript ด้วย กำหนดงบ `max_tokens` ได้ `mode: precise` ให้ Judge เรียงผลอันดับต้น ๆ ใหม่ (ช้ากว่า ต้องตั้ง `judge.provider = jev`) |
| `remember` | บันทึก memory (`kind`, `title`, `topics`, `importance`, `supersedes`) เรียกซ้ำได้ปลอดภัย |
| `context` | สรุปของโปรเจกต์ปัจจุบันภายในงบ token |
| `get_memory` | ดู memory ฉบับเต็ม พร้อม link และแหล่งที่มา |
| `update_memory` | แก้ไข, supersede, archive, forget หรือ restore |
| `query_facts` | ถาม knowledge graph ได้ทั้งภาพ ณ วันที่ระบุ (`as_of`) และ timeline |
| `assert_fact` | เพิ่ม แทนที่ หรือปิดอายุ fact พร้อมแนะนำ predicate ที่มีอยู่แล้ว |

server ส่งคำแนะนำการใช้งานให้ agent ด้วย มี prompt `recall` และ `remember-session` และ resource `memory://context` กับ `memory://m/{id}` ส่วนชุด tool สำหรับดูแลระบบ (`review_list`, `review_resolve`, `merge_memories`, `stats`) วางแผนไว้ใน M2

## CLI

```
alice recall <query> [--project p] [--kind k]... [--topic t]... [--raw] [--precise] [--limit n]
             [--max-tokens n] [--since date] [--before date]
alice remember [--kind k] [--title t] [--topic t]... [--project p] [--importance 1-5]
               [--supersedes id]... (<text> | -)
alice context [--project p] [--max-tokens n]
alice get <id>... [--provenance]
alice update <id> --action edit|supersede|archive|forget|restore [--title t] [--body b] [--by id]
alice facts <entity> [--predicate p] [--as-of date] [--timeline]
alice fact add|supersede|end <subject> <predicate> <object> [--from date]
alice projects | status
alice daemon start | stop | status | logs | run
alice mcp                      # MCP server แบบ stdio สำหรับ agent
alice model fetch
alice bench [--chunks n] [--memories n] [--samples n] [--real] [--dir d] [--out file.json]
```

ทุกคำสั่งใส่ `--json` ได้ ที่วางแผนไว้: `review`, `undo` (M2), `config`, `doctor`, `service`, `token rotate` (M2), `import mempalace` (M3), `sync` (M4), `hook` (SP2), `ui` (SP3)

## การตั้งค่า

M1 ยังไม่มีไฟล์ตั้งค่า อ่านค่าจาก environment variable เหล่านี้:

| ตัวแปร | ความหมาย |
|---|---|
| `ALICE_HOME` | directory เก็บข้อมูล (ค่าเริ่มต้น `~/.alice`) |
| `ALICE_LOG` | ตัวกรอง log เช่น `info` หรือ `alice_daemon=debug` (daemon เขียน log ที่ `$ALICE_HOME/logs/daemon.log`) |
| `ALICE_AGENT` | ชื่อผู้เขียนที่ CLI บันทึกไว้กับ memory (ค่าเริ่มต้น `cli`) |
| `ALICE_NO_AUTOSTART` | ถ้าตั้งค่า (และไม่ใช่ `0`) คำสั่งต่างๆ จะไม่เริ่ม daemon เอง การเขียนตอนที่ไม่มี daemon จะเข้าคิวใน `$ALICE_HOME/spool` (ลบความลับก่อน) แล้วถูกบันทึกเมื่อ daemon เริ่มทำงาน |
| `HF_ENDPOINT` | mirror ของ Hugging Face สำหรับ `alice model fetch` |

*(วางแผนไว้ใน M2)* ค่าต่างๆ จะอยู่ใน `~/.alice/config.toml` แต่ละ repo เขียนทับได้ด้วย `.alice.toml` และทุกค่าแก้ผ่าน UI ได้ เปลี่ยนแล้วมีผลทันทีโดยไม่ต้อง restart

```toml
[embedding]
provider = "local"                  # local | ollama | openai | voyage
model = "multilingual-e5-small"

[search]
default_max_tokens = 1500

[judge]
provider = "heuristic"              # heuristic | jev
auto_apply_threshold = 0.85         # มั่นใจเกินค่านี้ ระบบลงมือเอง
review_threshold = 0.5              # มั่นใจเกินค่านี้ แต่ไม่ถึงข้างบน ให้คนตัดสิน
daily_budget_usd = 1.0

[privacy]
redact = true
cloud_denied_projects = ["client-work"]   # โปรเจกต์ที่ห้ามส่งไป judge บน cloud

[sync]
remote = "git@github.com:you/alice-memory.git"
auto = "idle"                       # off | idle | 15m
include_raw = false

[backup]
daily = 7
weekly = 4
```

secret อย่าง API key ของ Jev จะเก็บใน `~/.alice/secrets.toml` (สิทธิ์ 0600) ด้วยคำสั่ง `alice config set-secret judge.jev.api_key` และเลือกใช้ Keychain ของ OS แทนได้

## Git sync *(วางแผนไว้ใน M4)*

```bash
alice sync init git@github.com:you/alice-memory.git
alice sync push
alice sync pull
```

memory แต่ละอันกลายเป็นไฟล์ `<project>/<kind>/<year>/<id>-<slug>.md` ที่มี YAML frontmatter ส่วน knowledge graph เก็บเป็น JSONL ที่เรียงลำดับแล้ว Alice merge ทีละ record สองเครื่องจึงแก้ memory คนละอันได้โดยไม่ชนกัน ถ้าแก้ memory อันเดียวกัน ฉบับที่ใหม่กว่าชนะ และอีกฉบับเข้าคิว review **ควรใช้ private repo** เพราะ memory อาจมีรายละเอียดของโปรเจกต์ที่อ่อนไหว แม้จะตัด secret ออกแล้วก็ตาม

## ย้ายมาจาก MemPalace *(ตัว import วางแผนไว้ใน M3)*

Alice ได้แรงบันดาลใจจาก [MemPalace](https://github.com/MemPalace/mempalace) และจะ import palace ของ MemPalace ได้:

```bash
alice import mempalace --dry-run   # ดูก่อนว่า wing ไหนจะกลายเป็น project ไหน
alice import mempalace
```

| | MemPalace | Alice |
|---|---|---|
| ตัวโปรแกรม | Python package | binary ไฟล์เดียว |
| การเขียนพร้อมกัน | hub ที่ถือสิทธิ์เขียนได้ทีละคน | daemon เป็นเจ้าของ DB คนเดียว งานหนักไม่ถือสิทธิ์เขียน |
| การจัดระเบียบ | wing, room, drawer, closet, tunnel | project, topic, memory, source, link |
| ข้อมูลดิบกับความรู้ | เก็บรวมกัน | แยกชั้น ค้นความรู้ก่อน |
| การแก้ตัวเอง | agent ต้องเรียก tool เฉพาะเอง (เช่น ตรวจซ้ำ, KG supersede) | Judge ช่วยกันซ้ำ, supersede และส่งเรื่องขัดแย้งเข้า review อัตโนมัติ |
| MCP tools | ~45 | หลัก 7 ตัว + ชุด admin และ compat ที่เลือกเปิดได้ |
| การแชร์ | shared-brain hub | git เป็น Markdown พร้อม merge ทีละ record |

drawer ที่ mine มาจาก transcript และไฟล์กลายเป็น source ดิบ ส่วน diary และ drawer ที่ agent เขียนเองกลายเป็น memory wing กลายเป็น project (ชื่อที่เขียนต่างกันรวมเป็นอันเดียว) room กลายเป็น topic และ knowledge graph ย้ายมาครบ ถ้ายังอยากเรียก tool `mempalace_*` ให้เปิด `compat.mempalace`

## ประสิทธิภาพ

วัดตอนจบ M1 บน Apple M4 Pro ด้วย `alice bench` ([ผลทั้งหมด](docs/benchmarks/m1.md)) ค่า p95:

| | เป้าหมาย | 100k chunk, โมเดลในเครื่อง | 1M chunk |
|---|---|---|---|
| `remember` ตอบรับ | < 50ms | 34ms (เครื่องทำงานหนัก) | 10.0ms |
| `recall` (ชั้นความรู้) | < 200ms | 31ms | 9.7ms |
| `recall` แบบ `include_raw` | < 200ms | 49ms | 139–177ms |
| `context` | < 150ms | 3.4ms | 0.8ms |
| หน่วยความจำของ daemon: RSS ปกติ / สูงสุด | RSS < 800MB ที่ 1M chunk | 0.58–0.76 / 0.91GB | ไม่รวมโมเดล 0.67–0.78 / 0.86GB ถ้ารวมโมเดลคาดว่า ≈1.4 / ≈1.6–1.7GB (ยังไม่ผ่านเป้า) |
| physical footprint ปกติ / สูงสุด | | 0.69–0.75 / 0.85GB | ไม่รวมโมเดล 0.77–0.86 / 0.86GB ถ้ารวมโมเดลคาดว่า ≈1.4–1.5 / ≈1.55GB |

รอบ 1M chunk ใช้ embedder จำลอง ส่วนโมเดลในเครื่องที่โหลดแล้วหนึ่งชุดกินหน่วยความจำราว 600MB ด้วยตัวมันเอง

## Roadmap

| ช่วง | ขอบเขต | สถานะ |
|---|---|---|
| **SP1 · M1** | ที่เก็บข้อมูล, การค้นหาแบบผสม, daemon, MCP, CLI, HTTP API | เสร็จแล้ว (alpha) |
| **SP1 · M2** | Judge (กฎในเครื่อง + Jev), คิว review, undo, ตัด secret, backup, settings ที่มีผลทันที, embedding provider ผ่าน HTTP | ลำดับถัดไป |
| **SP1 · M3** | ตัว import จาก MemPalace และ compatibility layer | วางแผนแล้ว |
| **SP1 · M4** | Git sync | วางแผนแล้ว |
| **SP2** | เก็บและกลั่นความรู้อัตโนมัติ, `alice setup`, hook/plugin สำหรับทุก agent ที่รองรับ | วางแผนแล้ว |
| **SP3** | Web UI (graph, จัดการ memory, review, settings, sync, สุขภาพระบบ) แล้วตามด้วยแอป desktop | วางแผนแล้ว |

รายละเอียดการออกแบบ: [Alice Core design spec](docs/superpowers/specs/2026-10-13-alice-core-design.md) (ภาษาอังกฤษ)

## ร่วมพัฒนา

เปิด issue, ร่วมคุยเรื่อง spec หรือส่ง pull request ได้เลย workspace มี 4 crate: `alice-core` (โมเดลข้อมูล, ที่เก็บ, การค้นหา), `alice-embed` (embedding provider), `alice-daemon` (writer, worker, HTTP API, MCP server, client) และ `alice` (ตัว binary, แพ็กเกจ `alice-memory`) ก่อนส่ง pull request ให้รัน:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## License

[MIT](LICENSE) Alice ได้แรงบันดาลใจจาก MemPalace (MIT, © MemPalace Contributors) โค้ดส่วนใดที่ดัดแปลงมาจะคงข้อความลิขสิทธิ์เดิมไว้
