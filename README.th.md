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
> **สถานะ: กำลังออกแบบ** ตอนนี้ยังไม่มีอะไรให้ติดตั้ง README นี้อธิบายระบบตามที่ออกแบบไว้ ส่วนวิธีติดตั้งและวิธีใช้คือ *interface ที่วางแผนไว้* และจะอัปเดตทุกครั้งที่แต่ละ milestone เสร็จ ติดตามได้ที่ [roadmap](#roadmap) หรือกด Watch repo เพื่อรับแจ้งเมื่อออก release

---

## ทำไมต้อง Alice

AI coding agent ลืมทุกอย่างเมื่อจบ session เครื่องมือด้านความจำช่วยแก้ปัญหานี้ได้ แต่พอเปิด agent หลายตัวพร้อมกันจะเจอปัญหาใหม่ เช่น เรียก memory แล้วค้างเพราะอีก session กำลังเขียนอยู่, ข้อมูลซ้ำหรือถูกเก็บผิดที่, transcript ดิบกลบความรู้ที่ต้องการจริงๆ และไม่มีทางดูหรือแก้ได้ง่ายๆ ว่า agent จำอะไรไว้บ้าง

Alice ออกแบบมาเพื่อการทำงานแบบนี้โดยเฉพาะ คือ agent หลายตัว โปรเจกต์หลายโปรเจกต์ แต่ใช้ความจำเดียวที่เร็ว สะอาด และถูกต้องเสมอ

## จุดเด่น

- **agent ไม่ต้องรอ memory อีก** มี daemon ตัวเดียวเป็นเจ้าของฐานข้อมูล การเขียนตอบกลับภายใน 50ms ส่วนงานหนักอย่าง embedding, การตัดสินของ Judge และการ import ทำเบื้องหลังทั้งหมด ถ้า daemon ไม่ได้รัน hook จะพักข้อมูลลงดิสก์ไว้ก่อน ข้อมูลไม่หาย
- **ความรู้มาก่อน transcript ดิบเรียกเมื่อต้องการ** ข้อมูลมี 2 ชั้น ชั้นแรกคือ *memory* สั้นๆ ที่กลั่นแล้ว (decision, การแก้บั๊ก, fact, how-to, preference, warning, handoff) ซึ่งค้นหาเป็นค่าเริ่มต้น ชั้นที่สองคือ transcript ดิบที่เก็บไว้เป็นหลักฐาน ค้นเมื่อขอหรือเมื่อชั้นแรกมีผลไม่พอ
- **ค้นเข้าใจทั้งโค้ดและภาษาไทย** รวมการค้น 4 แบบ: vector หลายภาษา, BM25, trigram สำหรับภาษาที่ไม่เว้นวรรค (ไทย จีน ญี่ปุ่น) และการค้นที่เข้าใจชื่อในโค้ด (`HostLinkReconnect`, `writer_lease`, path ไฟล์) ผลค้นหาให้น้ำหนักกับโปรเจกต์ปัจจุบัน ความสดใหม่ ความสำคัญ และสถานะ
- **ความจำแก้ตัวเองได้** memory ใหม่จะถูกเทียบกับของเดิมที่คล้ายกัน ถ้าซ้ำจะรวมกัน ถ้าเป็นข้อมูลที่ใหม่กว่าจะแทนที่ (supersede) ของเก่า ถ้าขัดแย้งกันจะเข้าคิว review ทุกการเปลี่ยนแปลงถูกบันทึกไว้และ undo ได้
- **มี AI Judge ให้เลือกเปิด** ต่อกับ [TypeSafe Jev](https://typesafe.ai) เพื่อช่วยตัดสินใจ (กันซ้ำ, จัดเข้าโปรเจกต์, เรียงผลค้นหา) ด้วยความน่าจะเป็นที่เชื่อถือได้ ตั้งเกณฑ์ความมั่นใจได้ มี cache และจำกัดงบรายวัน ถ้าไม่เปิด Alice จะใช้กฎที่ทำงานในเครื่องแทน
- **MCP tool น้อยแต่ตรงจุด** มี tool หลัก 7 ตัว (`recall`, `remember`, `context`, `get_memory`, `update_memory`, `query_facts`, `assert_fact`) แทนที่จะมีหลายสิบตัว ผลลัพธ์กระชับและคุมจำนวน token ได้ มีคำแนะนำการใช้งานในตัว พร้อม slash command และ resource
- **ได้สรุปสั้น ไม่ใช่ข้อมูลทั้งก้อน** `context` สรุปสิ่งสำคัญของโปรเจกต์ปัจจุบันให้อยู่ในงบ token ที่กำหนด: memory หลัก, decision ล่าสุด, warning, handoff ล่าสุด และ preference ของคุณ
- **Knowledge graph ที่รู้เรื่องเวลา** เก็บ entity และ relation พร้อมช่วงเวลาที่ข้อมูลยังเป็นจริง ถามย้อนเวลาได้ด้วย `as_of` และดูเป็น timeline ได้ มีพจนานุกรม predicate กันไม่ให้เกิด `has_bug` กับ `had_bug` ปนกัน
- **Git sync เป็นไฟล์ Markdown** push/pull ความจำไปที่ git remote ไหนก็ได้ เก็บ memory ละหนึ่งไฟล์ Markdown เปิดอ่านบน GitHub ได้ เปิดใน Obsidian ได้ และ sync ข้ามเครื่องได้โดย merge ทีละ record ไฟล์ฐานข้อมูลไม่เคยถูกใส่ใน git
- **ไฟล์เดียวจบ** daemon, CLI, MCP server และ hook อยู่ใน binary Rust ไฟล์เดียว ใช้ได้บน macOS, Linux และ Windows ไม่ต้องมี Python env หรือ Docker
- **เป็นส่วนตัวเป็นค่าเริ่มต้น** ข้อมูลอยู่ในเครื่อง ตัด secret ออกก่อนบันทึกหรือส่งออกทุกครั้ง เก็บ API key ใน Keychain ของ OS และ server รับเฉพาะการเชื่อมต่อจากเครื่องตัวเอง โดยตรวจ token และ origin ทุกคำขอ
- **UI สำหรับดู graph และจัดการข้อมูล** *(วางแผนไว้ใน SP3)* ดู graph ความจำ แก้ memory ตัดสินรายการใน review ตั้งค่าได้ทุกอย่าง และสั่ง git push/pull ได้จากเบราว์เซอร์หรือแอป desktop
- **ย้ายมาจาก MemPalace ได้** import palace เดิมได้ครบทั้ง drawer, diary และ knowledge graph และยังเรียก tool ชื่อ `mempalace_*` ได้ผ่าน compatibility layer ที่เลือกเปิดได้

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

## การติดตั้ง (วางแผนไว้)

```bash
# Homebrew (macOS, Linux)
brew install icueth/tap/alice

# สคริปต์ติดตั้ง (macOS, Linux)
curl -fsSL https://github.com/icueth/alice/releases/latest/download/alice-installer.sh | sh

# PowerShell (Windows)
irm https://github.com/icueth/alice/releases/latest/download/alice-installer.ps1 | iex

# npm (ถ้ามี Node อยู่แล้วจากการใช้ agent)
npm install -g alice-memory

# Cargo
cargo install alice-memory
```

โมเดล embedding (~120MB รองรับหลายภาษา) จะดาวน์โหลดตอนใช้งานครั้งแรก ถ้าเครื่องไม่มีเน็ต ให้รัน `alice model fetch` เตรียมไว้ก่อน

## เริ่มใช้งาน (วางแผนไว้)

```bash
alice setup                    # หา agent ที่ติดตั้งในเครื่องแล้วต่อให้อัตโนมัติ (MCP + hooks)
alice import mempalace         # (ไม่บังคับ) ย้ายข้อมูลจาก MemPalace
alice remember --kind decision "ใช้ SQLite WAL กับ writer thread ตัวเดียว"
alice recall "ทำไมใช้ writer ตัวเดียว"
alice context                  # สรุปของโปรเจกต์ปัจจุบัน
alice ui                       # เปิด web UI
```

## การเชื่อมต่อ agent

`alice setup` ตั้งค่าให้อัตโนมัติทั้งหมด ถ้าอยากตั้งเอง ให้ลงทะเบียน `alice mcp` เป็น MCP server แบบ stdio:

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
| `recall` | ค้นหา memory กรองตามโปรเจกต์, topic, kind และช่วงวันได้ กำหนดงบ `max_tokens` ได้ และ `mode: precise` จะให้ Judge เรียงผลใหม่ |
| `remember` | บันทึก memory (`kind`, `title`, `topics`, `importance`, `supersedes`) เรียกซ้ำได้ปลอดภัย |
| `context` | สรุปของโปรเจกต์ปัจจุบันภายในงบ token |
| `get_memory` | ดู memory ฉบับเต็ม พร้อม link และแหล่งที่มา |
| `update_memory` | แก้ไข, supersede, archive, forget หรือ restore |
| `query_facts` | ถาม knowledge graph ได้ทั้งภาพ ณ วันที่ระบุ (`as_of`) และ timeline |
| `assert_fact` | เพิ่ม แทนที่ หรือปิดอายุ fact พร้อมแนะนำ predicate ที่มีอยู่แล้ว |

เปิดชุด tool สำหรับดูแลระบบ (`review_list`, `review_resolve`, `merge_memories`, `stats`) ได้ใน settings

## CLI (วางแผนไว้)

```
alice recall <query> [--project p] [--kind k] [--raw] [--precise] [--json]
alice remember [--kind k] [--title t] [--topic t]... (<text> | -)
alice context | get <id> | facts <entity> [--as-of date] [--timeline]
alice review [list | resolve <id> <choice>]      alice undo <event-id>
alice sync [init <remote> | push | pull | status]
alice import mempalace [--palace path] [--dry-run]
alice config [get | set | list | edit | set-secret]
alice doctor | bench | ui | daemon | service | mcp
```

## การตั้งค่า

ค่าต่างๆ อยู่ใน `~/.alice/config.toml` แต่ละ repo เขียนทับได้ด้วย `.alice.toml` และทุกค่าแก้ผ่าน UI ได้ เปลี่ยนแล้วมีผลทันทีโดยไม่ต้อง restart

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

secret อย่าง API key ของ Jev เก็บใน Keychain ของ OS ด้วยคำสั่ง `alice config set-secret judge.jev.api_key`

## Git sync

```bash
alice sync init git@github.com:you/alice-memory.git
alice sync push
alice sync pull
```

memory แต่ละอันกลายเป็นไฟล์ `<project>/<kind>/<year>/<id>-<slug>.md` ที่มี YAML frontmatter ส่วน knowledge graph เก็บเป็น JSONL ที่เรียงลำดับแล้ว Alice merge ทีละ record สองเครื่องจึงแก้ memory คนละอันได้โดยไม่ชนกัน ถ้าแก้ memory อันเดียวกัน ฉบับที่ใหม่กว่าชนะ และอีกฉบับเข้าคิว review **ควรใช้ private repo** เพราะ memory อาจมีรายละเอียดของโปรเจกต์ที่อ่อนไหว แม้จะตัด secret ออกแล้วก็ตาม

## ย้ายมาจาก MemPalace

Alice ได้แรงบันดาลใจจาก [MemPalace](https://github.com/MemPalace/mempalace) และ import palace ของ MemPalace ได้:

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

## Roadmap

| ช่วง | ขอบเขต | สถานะ |
|---|---|---|
| **SP1 · M1** | ที่เก็บข้อมูล, การค้นหาแบบผสม, daemon, MCP, CLI, HTTP API | กำลังออกแบบ |
| **SP1 · M2** | Judge (กฎในเครื่อง + Jev), คิว review, undo, ตัด secret, backup, settings ที่มีผลทันที | วางแผนแล้ว |
| **SP1 · M3** | ตัว import จาก MemPalace และ compatibility layer | วางแผนแล้ว |
| **SP1 · M4** | Git sync | วางแผนแล้ว |
| **SP2** | เก็บและกลั่นความรู้อัตโนมัติ, `alice setup`, hook/plugin สำหรับทุก agent ที่รองรับ | วางแผนแล้ว |
| **SP3** | Web UI (graph, จัดการ memory, review, settings, sync, สุขภาพระบบ) แล้วตามด้วยแอป desktop | วางแผนแล้ว |

รายละเอียดการออกแบบ: [Alice Core design spec](docs/superpowers/specs/2026-10-13-alice-core-design.md) (ภาษาอังกฤษ)

## ร่วมพัฒนา

ตอนนี้โปรเจกต์อยู่ในขั้นออกแบบ เปิด issue หรือร่วมคุยเรื่อง spec ได้เลย ส่วนการส่งโค้ดจะเปิดหลัง M1 เสร็จ โดยจะเขียนโครง workspace และแนวทางการเขียนโค้ดไว้ใน `CONTRIBUTING.md`

## License

[MIT](LICENSE) Alice ได้แรงบันดาลใจจาก MemPalace (MIT, © MemPalace Contributors) โค้ดส่วนใดที่ดัดแปลงมาจะคงข้อความลิขสิทธิ์เดิมไว้
