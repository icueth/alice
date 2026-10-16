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
> **สถานะ: M2 เสร็จแล้ว — รุ่น alpha** ที่เก็บข้อมูล, การค้นหาแบบผสม (ไทย อังกฤษ และชื่อในโค้ด), daemon, MCP tool ทั้ง 7 ตัว, CLI และ HTTP API (M1) ตอนนี้มีเส้นทางการเขียนที่ดูแลตัวเองได้ครบ: ตัด secret ออกก่อนบันทึก, Judge ที่ทำงานในเครื่องพร้อม TypeSafe Jev เป็นตัวเลือก, คิว review, ประวัติและ undo, backup และ restore, ค่าตั้งที่มีผลทันทีพร้อมไฟล์ secret ส่วนตัว, `alice doctor`, `alice service install` และ embedding provider ผ่าน HTTP (M2) CI build และทดสอบทั้งหมดนี้บน macOS, Linux และ Windows ส่วนบน Windows การทดสอบ CLI และ process ของ daemon รันเฉพาะบน Unix (ดู[แพลตฟอร์มที่รองรับ](#แพลตฟอร์มที่รองรับ)) ยังไม่มี release แบบแพ็กเกจ ให้[ติดตั้งจาก source](#ติดตั้งจาก-source) ความสามารถที่มีป้าย *(วางแผนไว้)* จะตามมาใน milestone ถัดไป ดูได้ที่ [roadmap](#roadmap) ก่อนถึง 1.0 อาจมีการเปลี่ยนที่ไม่เข้ากับของเดิม

---

## ทำไมต้อง Alice

AI coding agent ลืมทุกอย่างเมื่อจบ session เครื่องมือด้านความจำช่วยแก้ปัญหานี้ได้ แต่พอเปิด agent หลายตัวพร้อมกันจะเจอปัญหาใหม่ เช่น เรียก memory แล้วค้างเพราะอีก session กำลังเขียนอยู่, ข้อมูลซ้ำหรือถูกเก็บผิดที่, transcript ดิบกลบความรู้ที่ต้องการจริงๆ และไม่มีทางดูหรือแก้ได้ง่ายๆ ว่า agent จำอะไรไว้บ้าง

Alice ออกแบบมาเพื่อการทำงานแบบนี้โดยเฉพาะ คือ agent หลายตัว โปรเจกต์หลายโปรเจกต์ แต่ใช้ความจำเดียวที่เร็ว สะอาด และถูกต้องเสมอ

## จุดเด่น

- **agent ไม่ต้องรอ memory อีก** มี daemon ตัวเดียวเป็นเจ้าของฐานข้อมูล การเขียนตอบกลับภายใน 50ms ส่วนงานหนักอย่าง embedding และการทำ index ทำเบื้องหลังทั้งหมด ถ้า daemon ไม่ได้รันหรือยังเปิดไม่เสร็จ การเขียนจะพักลงดิสก์ไว้ก่อน ข้อมูลไม่หาย
- **ความรู้มาก่อน transcript ดิบเรียกเมื่อต้องการ** ข้อมูลมี 2 ชั้น ชั้นแรกคือ *memory* สั้นๆ ที่กลั่นแล้ว (decision, การแก้บั๊ก, fact, how-to, preference, warning, handoff) ซึ่งค้นหาเป็นค่าเริ่มต้น ชั้นที่สองคือ transcript ดิบที่เก็บไว้เป็นหลักฐาน ค้นเมื่อขอหรือเมื่อชั้นแรกมีผลไม่พอ *(การเก็บ transcript และกลั่นความรู้อัตโนมัติ: วางแผนไว้ใน SP2)*
- **ค้นเข้าใจทั้งโค้ดและภาษาไทย** รวมการค้น 4 แบบ: vector หลายภาษา, BM25, trigram สำหรับภาษาที่ไม่เว้นวรรค (ไทย จีน ญี่ปุ่น) และการค้นที่เข้าใจชื่อในโค้ด (`HostLinkReconnect`, `writer_lease`, path ไฟล์) ผลค้นหาให้น้ำหนักกับโปรเจกต์ปัจจุบัน ความสดใหม่ ความสำคัญ และสถานะ
- **ความจำแก้ตัวเองได้** memory ใหม่จะถูกเทียบกับของเดิมที่คล้ายกัน ข้อความที่เหมือนกันเป๊ะจะรวมเข้ากับอันเก่า ถ้าเปิด Judge จะให้ข้อมูลใหม่แทนที่ (supersede) ของเก่าได้ และเรื่องขัดแย้งจะเข้าคิว review (`alice review`) ทุกการเปลี่ยนแปลงถูกบันทึกไว้ (`alice history`) และ undo ได้ (`alice undo`)
- **มี AI Judge ให้เลือกเปิด** ต่อกับ [TypeSafe Jev](https://typesafe.ai) เพื่อช่วยตัดสินใจ (กันซ้ำ, ระบุ kind, จัดเข้าโปรเจกต์, ให้คะแนนผลค้นหา) ด้วยความน่าจะเป็นที่เชื่อถือได้ ตั้งเกณฑ์ความมั่นใจได้ มี cache และมีงบรายวันที่ใช้เกินไม่ได้ ถ้าไม่เปิด Alice จะใช้กฎที่ทำงานในเครื่อง ซึ่งรวมเฉพาะข้อความที่เหมือนกันทุกตัวอักษร ส่วนที่เหลือทิ้งไว้ให้คุณตรวจ
- **MCP tool น้อยแต่ตรงจุด** มี tool หลัก 7 ตัว (`recall`, `remember`, `context`, `get_memory`, `update_memory`, `query_facts`, `assert_fact`) แทนที่จะมีหลายสิบตัว ผลลัพธ์กระชับและคุมจำนวน token ได้ มีคำแนะนำการใช้งานในตัว พร้อม slash command และ resource ชุด tool สำหรับดูแลระบบ (review, รวม memory, สถิติ) พร้อมให้เปิดเมื่อต้องการ
- **ได้สรุปสั้น ไม่ใช่ข้อมูลทั้งก้อน** `context` สรุปสิ่งสำคัญของโปรเจกต์ปัจจุบันให้อยู่ในงบ token ที่กำหนด: memory หลัก, decision ล่าสุด, warning, handoff ล่าสุด และ preference ของคุณ
- **Knowledge graph ที่รู้เรื่องเวลา** เก็บ entity และ relation พร้อมช่วงเวลาที่ข้อมูลยังเป็นจริง ถามย้อนเวลาได้ด้วย `as_of` และดูเป็น timeline ได้ มีพจนานุกรม predicate กันไม่ให้เกิด `has_bug` กับ `had_bug` ปนกัน
- **เป็นส่วนตัวเป็นค่าเริ่มต้น** ข้อมูลอยู่ในเครื่อง และ server รับเฉพาะการเชื่อมต่อจากเครื่องตัวเอง โดยตรวจ token และ origin ทุกคำขอ secret ในข้อความ (API key, token, รหัสผ่าน, private key) ถูกตัดออกก่อนบันทึก เข้าคิว หรือส่งออกทุกครั้ง API key เก็บในไฟล์ส่วนตัว (`secrets.toml` สิทธิ์ 0600) และเลือกใช้ Keychain ของ OS แทนได้ โปรเจกต์ไหนจะห้ามส่งไปบริการ cloud ใดๆ ก็ได้
- **ดูแลตัวเองได้** ตอน daemon ว่าง จะสำรองข้อมูลรายวันพร้อมหมุนเวียนไฟล์, ตรวจความสมบูรณ์, เก็บตามนโยบายการเก็บรักษา และล้างถังขยะ `alice doctor` บอกว่าอะไรผิดและควรทำอะไรต่อ `alice service install` ให้ daemon เริ่มเองตอน login และค่าตั้งเปลี่ยนแล้วมีผลทันทีโดยไม่ต้อง restart
- **ไฟล์เดียวจบ** daemon, CLI และ MCP server (และ hook ใน SP2) อยู่ใน binary Rust ไฟล์เดียวที่มีตัวรัน embedding ในตัว ไม่ต้องมี Python env หรือ Docker ถ้าอยากใช้ embedding จากที่อื่น (Ollama, endpoint แบบ OpenAI, Voyage) ก็แค่ตั้งค่า
- **Git sync เป็นไฟล์ Markdown** *(วางแผนไว้ใน M4)* push/pull ความจำไปที่ git remote ไหนก็ได้ เก็บ memory ละหนึ่งไฟล์ Markdown เปิดอ่านบน GitHub ได้ เปิดใน Obsidian ได้ และ sync ข้ามเครื่องได้โดย merge ทีละ record ไฟล์ฐานข้อมูลไม่เคยถูกใส่ใน git
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
                                                                            └─ SQLite (WAL) + FTS5 + vector index
```

| แนวคิด | ความหมาย |
|---|---|
| **Project** | ตรวจหาเองจาก git repo ของคุณ ชื่อที่เขียนต่างกัน (`my-app`, `my_app`) ถือเป็นโปรเจกต์เดียวกัน ส่วนโปรเจกต์ `global` เก็บ preference ที่ใช้กับทุกโปรเจกต์ |
| **Memory** | ความรู้สั้นๆ ที่จบในตัว มี kind, topic, ความสำคัญ และสถานะ |
| **Source / chunk** | หลักฐานดิบ (transcript, ไฟล์, ข้อมูลที่ import มา) ที่ memory ชี้กลับไปหาได้ |
| **Fact** | ความสัมพันธ์ใน knowledge graph ระหว่าง entity พร้อมช่วงเวลาที่ยังเป็นจริง |
| **Review** | เรื่องที่ระบบมั่นใจไม่พอจะตัดสินเอง จึงรอให้คนตัดสิน |
| **Event** | การเปลี่ยนแปลงหนึ่งครั้งที่ถูกบันทึกไว้ (ใคร ทำอะไร ก่อนและหลัง) `alice undo` ย้อนกลับได้ |

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
| Windows x86_64 | build และ link ผ่าน CI รัน `fmt`, `clippy` และชุดทดสอบแบบ in-process ส่วนการทดสอบ CLI และ process ของ daemon รันเฉพาะบน Unix; โมเดลในเครื่องตรวจแล้วบน Windows ด้วย CI workflow `windows-real-model` ที่สั่งรันเอง (โมเดลจริง โดยโฟลเดอร์ข้อมูลอยู่ใต้ path ที่ไม่ใช่ ASCII) `alice daemon start`, `status` และ `stop` หา daemon ที่กำลังเริ่มหรือกำลังทำงานเจอผ่าน `daemon.pid` (pid และเวลาเริ่ม วางไว้ข้างไฟล์ lock เพราะบน Windows อ่านไฟล์ lock ที่ถูกล็อกอยู่ไม่ได้) กับการตรวจ process ด้วย Win32 ส่วนนี้ครอบคลุมด้วย unit test บน Windows CI แต่ยังไม่มี process test; `alice service install` ลงทะเบียน scheduled task ที่เริ่ม daemon ตอน logon ตัวแผนมีการทดสอบแล้ว แต่ยังไม่เคยลองบนเครื่อง Windows จริง |
| macOS ชิป Intel, Linux musl | ยังไม่รองรับ ONNX Runtime ไม่มี library สำเร็จรูปให้ จึง build โมเดลในเครื่องบนแพลตฟอร์มนี้ไม่ได้ embedding provider ผ่าน HTTP (`ollama`, `openai`, `voyage` ดู[การตั้งค่า](#การตั้งค่า)) เขียนและทดสอบแล้ว และ `alice-embed` compile ได้โดยไม่มีโมเดลในเครื่อง แต่แพ็กเกจ `alice` ยังไม่มีแบบ build ที่ตัดโมเดลออก และ CI ก็ยังไม่ build เป้าหมายเหล่านี้ ตอนนี้จึงถือว่ายังไม่รองรับ |

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
alice status                   # daemon, จำนวนข้อมูล, คิว embedding และ judge, review ที่ค้าง, backup ล่าสุด
alice doctor                   # ถ้ามีอะไรผิด จะบอกว่าผิดตรงไหนและควรทำอะไรต่อ
```

โปรเจกต์ตรวจหาจาก directory ที่ทำงานอยู่ (git remote หรือชื่อ directory) memory ที่เขียนนอกโปรเจกต์ใดๆ จะเข้า `global` `alice import mempalace` ย้าย MemPalace เดิมเข้ามา ([รายละเอียด](#import-จาก-mempalace)) ที่วางแผนไว้: `alice setup` ต่อ agent ทุกตัวในเครื่องให้อัตโนมัติ (SP2) และ `alice ui` (SP3)

## การเชื่อมต่อ agent

ลงทะเบียน `alice mcp` เป็น MCP server แบบ stdio (ใน SP2 `alice setup` จะทำให้อัตโนมัติ) ตัว shim จะเปิด daemon ให้เองเมื่อจำเป็น และถ้า client ส่ง workspace root มา โปรเจกต์จะยึดตาม root นั้น ชื่อของ client จะถูกบันทึกเป็นผู้เขียนของทุกอย่างที่มันเขียน:

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
| `recall` | ค้นหา memory กรองตามโปรเจกต์, topic, kind และช่วงวันได้ ใส่ `include_raw` เพื่อค้น transcript ด้วย กำหนดงบ `max_tokens` ได้ เมื่อตั้ง model judge ไว้ (`judge.provider = jev`) recall จะเป็นแบบ **precise** เป็นค่าเริ่มต้น: Judge ให้คะแนนผลอันดับต้น ๆ 20 อันของทุกชั้น รวมถึง chunk ของ transcript และช่วยดึงคำตอบที่ต่ำกว่าเกณฑ์ vector เพียงนิดเดียวกลับมา (ช้ากว่า ราวครึ่งวินาที) `mode: "fast"` ข้าม Judge และ `mode: "precise"` บังคับใช้ ถ้าไม่มี model judge recall เป็นแบบ fast เสมอ ถ้า Judge ช้าหรือล้มเหลว recall เสียเวลาไม่เกิน `search.precise_timeout_ms` (ค่าเริ่มต้น 2.5 วินาที และถ้าล้มเหลวติดกัน 3 ครั้งจะข้าม Judge ไปหนึ่งนาที) แล้วตอบแบบ fast พร้อมหมายเหตุบอกเหตุผล recall จาก repository ที่ยังไม่เคยเขียนอะไรลง Alice จะถูกตัดสินด้วย `.alice.toml` ของ repository นั้นเองและรายการ global เหมือนการ embed query ผ่านผู้ให้บริการภายนอก |
| `remember` | บันทึก memory (`kind`, `title`, `topics`, `importance`, `supersedes`) เรียกซ้ำได้ปลอดภัย |
| `context` | สรุปของโปรเจกต์ปัจจุบันภายในงบ token |
| `get_memory` | ดู memory ฉบับเต็ม พร้อม link และแหล่งที่มา ถ้าขอ memory ที่ Judge รวมเข้ากับอันเก่าไปแล้ว จะได้อันเก่ากลับมา |
| `update_memory` | แก้ไข, supersede, archive, forget หรือ restore |
| `query_facts` | ถาม knowledge graph ได้ทั้งภาพ ณ วันที่ระบุ (`as_of`) และ timeline |
| `assert_fact` | เพิ่ม แทนที่ หรือปิดอายุ fact พร้อมแนะนำ predicate ที่มีอยู่แล้ว |

server ส่งคำแนะนำการใช้งานให้ agent ด้วย มี prompt `recall` และ `remember-session` และ resource `memory://context` กับ `memory://m/{id}` ส่วนชุด tool สำหรับดูแลระบบ (`review_list`, `review_resolve`, `merge_memories`, `stats`) ปิดอยู่เป็นค่าเริ่มต้น เปิดได้ด้วย `alice config set mcp.admin_tools true` (stdio shim ถาม daemon ทุกครั้งที่แสดงรายการ tool จึงไม่ต้อง restart)

## CLI

```
alice recall <query> [--project p] [--kind k]... [--topic t]... [--raw] [--precise | --fast] [--limit n]
             [--max-tokens n] [--since date] [--before date]
alice remember [--kind k] [--title t] [--topic t]... [--project p] [--importance 1-5]
               [--supersedes id]... (<text> | -)
alice context [--project p] [--max-tokens n]
alice get <id>... [--provenance]
alice update <id> --action edit|supersede|archive|forget|restore [--title t] [--body b] [--by id]
alice update <id> --action move --project <slug>   (CLI and HTTP only, not MCP; undoable)
alice facts <entity> [--predicate p] [--as-of date] [--timeline]
alice fact add|supersede|end <subject> <predicate> <object> [--from date]
alice history [target] [--actor a] [--limit n]       # ใครเปลี่ยนอะไร
alice undo <event-id>                                 # ย้อนการเปลี่ยนแปลง (ทั้งกลุ่ม)
alice review [list [--type t] [--all] | show <id> | resolve <id> <choice>]
alice stats                    # จำนวนข้อมูล, review ที่ค้าง, ค่าใช้จ่ายของ judge
alice backup now | list       alice restore [backup] [--yes]
alice maintenance status | run <job>
alice config list | get <key> | set <key> <value> | unset <key> | edit
             | set-secret <name> | delete-secret <name> | test <provider>
alice projects [merge <from> <into> [--yes]] | status
alice doctor                   # ตรวจพร้อมบอกว่าควรทำอะไรต่อ; exit 1 เมื่อมีข้อที่ไม่ผ่าน
alice daemon start | stop | status | logs | run
alice token rotate [--show]
alice service install | uninstall [--dry-run] [--target-dir d] | status
alice mcp                      # MCP server แบบ stdio สำหรับ agent
alice model fetch
alice bench [--chunks n] [--memories n] [--samples n] [--real] [--dir d] [--out file.json]
alice import mempalace [--palace dir] [--dry-run] [--since date] [--map-wing wing=project]...
                       [--collection name] [--allow-remote-embedding]
alice import status
```

`--action move` รับแค่ `--project` (flag อื่นจะถูกปฏิเสธ และ `--project` คู่กับ action อื่นก็ถูกปฏิเสธเช่นกัน) เหมือนการ merge project คือจะไม่ย้าย memory จาก project ที่ถูกกันไม่ให้ส่งขึ้น cloud ไปยัง project ที่ส่งได้ และจะตรวจข้อความกับกฎความเป็นส่วนตัวของ project ปลายทาง: โหมด `block` ปฏิเสธข้อความที่กฎนั้นจะเปลี่ยน ส่วนโหมดอื่นจะย้ายไปพร้อมข้อความที่ถูกปกปิดตามกฎนั้น

ทุกคำสั่งใส่ `--json` ได้ ที่วางแผนไว้: `sync` (M4), `hook` (SP2), `ui` (SP3)

## การตั้งค่า

ค่าตั้งอยู่ใน `~/.alice/config.toml` ลำดับชั้น (ชั้นหลังชนะ): ค่าเริ่มต้นในโปรแกรม, `config.toml`, `.alice.toml` ของ repo (เฉพาะหมวด privacy ดูข้างล่าง) และ environment variable อ่านและเปลี่ยนทุกค่าได้ด้วย `alice config list | get | set | unset | edit` และแก้แล้วมีผลโดยไม่ต้อง restart daemon

```toml
[judge]
provider = "jev"                    # heuristic (ค่าเริ่มต้น) | jev
auto_apply_threshold = 0.85         # มั่นใจตั้งแต่ค่านี้ ระบบลงมือเอง
review_threshold = 0.5              # มั่นใจตั้งแต่ค่านี้แต่ไม่ถึงข้างบน ให้คนตัดสิน
daily_budget_usd = 1.0              # หยุดทันทีเมื่อครบ ต่อวัน UTC
timeout_ms = 10000

[privacy]
redact = true
redact_mode = "redact"              # redact | block (ปฏิเสธการเขียนที่มี secret)
cloud_denied_projects = ["client-work"]   # ห้ามส่งไป Jev หรือ embedding provider ที่อยู่นอกเครื่อง
custom_patterns = [{ name = "ticket", regex = "TCK-[0-9]{6}" }]

[embedding]
provider = "ollama"                 # local (ค่าเริ่มต้น) | ollama | openai | voyage
model = "nomic-embed-text"          # ชื่อโมเดลของ provider
endpoint = "http://127.0.0.1:11434/v1"   # ไม่ใส่ก็ได้ แต่ละ provider มีค่าเริ่มต้น
# idle_unload = "10m"               # เฉพาะโมเดลในเครื่อง: ถอนโมเดลออกเมื่อไม่ได้ใช้นานเท่านี้ ("0" = ไม่ถอน)

[search]
default_max_tokens = 1500
default_mode = "auto"                # auto (precise เมื่อมี model judge ไม่งั้น fast) | fast | precise
precise_timeout_ms = 2500               # งบเวลาทั้งหมดของการเรียก Judge ใน recall แบบ precise

[maintenance]
enabled = true
idle_secs = 60                      # งานเบื้องหลังเริ่มเมื่อเงียบมานานเท่านี้
trash_days = 30                     # memory ที่ forget แล้วถูกลบถาวรเมื่อครบกี่วัน
events_days = 365                   # ประวัติที่เก่ากว่านี้ถูกลบ

[backup]
daily = 7
weekly = 4
```

Environment variable:

| ตัวแปร | ความหมาย |
|---|---|
| `ALICE_<SECTION>_<KEY>` | ตั้งค่าใดก็ได้ ใช้ชื่อ key แบบมีจุดแล้วแปลงเป็นตัวพิมพ์ใหญ่และเปลี่ยนจุดเป็น `_`: `judge.review_threshold` คือ `ALICE_JUDGE_REVIEW_THRESHOLD`, `judge.jev.rate_per_sec` คือ `ALICE_JUDGE_JEV_RATE_PER_SEC` environment ชนะไฟล์ ชื่อ `ALICE_*` ที่ไม่ใช่ค่าตั้งจะถูกข้ามพร้อมคำเตือน (ดู `alice config list` และ `alice doctor`) ค่าอื่นไม่ได้รับผลกระทบ |
| `ALICE_HOME` | directory เก็บข้อมูล (ค่าเริ่มต้น `~/.alice`) |
| `ALICE_LOG` | ตัวกรอง log เช่น `info` หรือ `alice_daemon=debug` (daemon เขียน log ที่ `$ALICE_HOME/logs/daemon.log`) |
| `ALICE_AGENT` | ชื่อผู้เขียนที่ CLI บันทึกไว้กับ memory (ค่าเริ่มต้น `cli`) |
| `ALICE_NO_AUTOSTART` | ถ้าตั้งค่า (และไม่ใช่ `0`) คำสั่งต่างๆ จะไม่เริ่ม daemon เอง การเขียนตอนที่ไม่มี daemon จะเข้าคิวใน `$ALICE_HOME/spool` (ลบความลับก่อน) แล้วถูกบันทึกเมื่อ daemon เริ่มทำงาน |
| `TYPESAFE_API_KEY`, `ALICE_EMBEDDING_API_KEY` | key ของ Jev และ key ของ embedding provider ผ่าน HTTP ชนะค่าที่เก็บไว้ และไม่ถูกเขียนลงที่ไหนเลย |
| `HF_ENDPOINT` | mirror ของ Hugging Face สำหรับ `alice model fetch` |

**Secret** key ไม่อยู่ใน `config.toml` `alice config set-secret jev_api_key` (และ `embedding_api_key` สำหรับ `openai` หรือ `voyage`) อ่านค่าจาก standard input ไม่รับจาก command line แล้วเก็บใน `~/.alice/secrets.toml` (สิทธิ์ 0600) settings API บอกได้แค่ว่าตั้งไว้หรือยัง ตั้ง `secrets.backend = "keychain"` เพื่อใช้ Keychain ของ OS แทน (บน macOS อาจถามสิทธิ์ทุกครั้งที่อัปเดต) มีสองเรื่องที่ควรรู้: `secrets.toml` ที่แก้ด้วยมือ Judge จะอ่านตอนมันถูกสร้างใหม่ครั้งถัดไป จึงควรสั่ง `alice daemon stop` หลังแก้ (คำสั่งถัดไปจะเปิด daemon ให้ใหม่) และการสลับ `secrets.backend` ไม่ย้าย secret ที่เก็บไว้ ต้องตั้งใหม่

**ความเป็นส่วนตัวระดับ repo** repo ใส่ `.alice.toml` ไว้ที่ root ได้ ไฟล์นี้รับเฉพาะหมวด `[privacy]` และเฉพาะค่าที่ทำให้ Alice ระวังมากขึ้น ค่าอื่นจะถูกปฏิเสธ:

```toml
[privacy]
block_secrets = true      # ปฏิเสธการเขียนในโปรเจกต์นี้ที่มี secret
cloud_denied = true       # ห้ามส่งข้อความของโปรเจกต์นี้ไป Jev หรือ embedding provider ที่อยู่นอกเครื่อง
custom_patterns = [{ name = "internal_id", regex = "ID-[0-9]{8}" }]
```

**การเปลี่ยนค่าทำอย่างระมัดระวัง** `config.toml` ถูกตรวจทุก 2 วินาที ไฟล์หรือค่าที่ไม่ถูกต้องจะไม่ทับค่าที่ใช้งานได้: ค่าเดิมยังอยู่ และ error แสดงใน `alice config list`, `/api/v1/settings` และ `alice doctor` การเปลี่ยนที่ทำให้การป้องกันอ่อนลง (ปิด redaction, เอาโปรเจกต์ออกจาก `cloud_denied_projects`, เพิ่มงบ, สลับไปใช้ Jev หรือ embedding provider นอกเครื่อง, เปิด admin tools) จะมีผลก็ต่อเมื่ออ่านไฟล์ได้เหมือนกัน 3 ครั้งติดกัน (ราว 6 วินาที) ไฟล์ที่เซฟค้างครึ่งทางจึงปิด redaction แม้ชั่วขณะไม่ได้ ส่วน `alice config set` มีผลทันทีและบอกไว้ในผลลัพธ์ ตราบที่ยังไม่เคยโหลด `config.toml` ที่ถูกต้องสำเร็จ จะไม่ส่งอะไรไปบริการ cloud เลย ค่าเหล่านี้ต้อง restart daemon (`alice daemon stop`): `server.port`, `secrets.backend`, `embedding.idle_unload`, `embedding.batch_size` ส่วนการเปลี่ยน embedding provider หรือโมเดลไม่ต้อง restart: โมเดลใหม่ถูกเติมเบื้องหลังและสลับเข้าแทนแบบ atomic เมื่อครบ

**redaction ตัดอะไรบ้าง** ก่อนบันทึกหรือส่งออก memory, chunk ของ transcript, snapshot ในประวัติ, งานเขียนที่รอในคิว หรือคำขอไป Jev Alice จะแทน secret ด้วย `[REDACTED:<type>]` ได้แก่ private key, JWT, key ของ AWS, GitHub, GitLab, Slack, Stripe, SendGrid, Vault, Anthropic และ OpenAI, รหัสผ่านใน URL, การกำหนดค่าแบบ `password=`/`token=` และค่าที่สุ่มสูงยาวๆ ที่ตามหลังชื่อคล้าย key รวมถึง `custom_patterns` ของคุณเอง ถ้าตั้ง `redact_mode = "block"` จะปฏิเสธการเขียนแทน โดย error บอกชื่อ field และ detector (ไม่บอก secret)

**embedding ในเครื่องหรือไม่** ค่าเริ่มต้นคือโมเดลหลายภาษาในเครื่อง provider ผ่าน HTTP (`ollama` ในเครื่องหรือในเครือข่ายของคุณ, `openai` สำหรับ endpoint ที่เข้ากันกับ OpenAI, `voyage`) จะได้รับข้อความเฉพาะโปรเจกต์ที่นโยบาย cloud อนุญาต และ `ollama` บน `localhost` ถือว่าเป็นในเครื่อง รายละเอียด: [docs/benchmarks/m2-embedding.md](docs/benchmarks/m2-embedding.md)

## Judge, review และ undo

เมื่อเขียน memory ระบบจะเทียบกับ memory ที่มีอยู่แล้วในโปรเจกต์เดียวกันเบื้องหลัง แล้วจึงเปลี่ยนสถานะเป็น `active`:

- **Heuristic (ค่าเริ่มต้น ทำงานในเครื่อง ฟรี)** รวม memory ใหม่เข้ากับอันเก่าก็ต่อเมื่อข้อความเหมือนกันหลัง normalize Unicode และยุบช่องว่าง (ตัวพิมพ์เล็กใหญ่และเครื่องหมายวรรคตอนมีผล) ไม่เคยรวมเพราะความคล้ายอย่างเดียว: ถ้าน่าจะซ้ำ น่าจะเป็นการอัปเดต หรือน่าจะอยู่คนละโปรเจกต์ จะกลายเป็นรายการในคิว review และไม่ตั้ง kind ให้
- **Jev (เลือกเปิด)** โมเดล Jev ของ TypeSafe ตอบคำถามแบบเลือกคำตอบพร้อมความน่าจะเป็น Alice ถามว่า memory ใหม่ซ้ำ, อัปเดต, ขัดแย้ง หรือไม่เกี่ยวกับเพื่อนบ้านที่ใกล้ที่สุดแต่ละอัน และ kind กับโปรเจกต์ไหนเหมาะ ถ้าถึง `judge.auto_apply_threshold` จะลงมือเลย ถ้าอยู่ระหว่างสองเกณฑ์จะเป็นรายการ review ต่ำกว่านั้นไม่ทำอะไร คำตอบถูก cache ตามคำขอ จำกัดอัตราการเรียก และงบรายวันถูกจองไว้ก่อนเรียกทุกครั้ง ยอดใช้ในวันจึงไม่เกิน `judge.daily_budget_usd` ถ้า Jev ล่ม งบหมด หรือโปรเจกต์นั้นไม่ได้รับอนุญาต heuristic จะตัดสินแทนและ memory จะถูกตัดสินใหม่ภายหลัง ปัญหาของ Judge ไม่เคยทำให้การเขียนล้มเหลว จากการประเมินแบบสังเคราะห์ใน [docs/benchmarks/m2-jev.md](docs/benchmarks/m2-jev.md) ค่าใช้จ่ายอยู่ที่ **$0.0113 ต่อ 1,000 การตัดสินใจ** (หนึ่งการตัดสินใจคือเทียบ memory หนึ่งคู่ ราคา input token $0.042 ต่อล้าน token) และ smoke test สดของการเทียบหนึ่งครั้งอยู่ที่ราว **$0.00004** การประเมินนั้น (216 คู่ที่เขียนขึ้นเพื่อการนี้ รันครั้งเดียว) ได้ความแม่นยำของ Jev ในการระบุความสัมพันธ์ 0.917 เทียบกับ heuristic 0.287 ควรอ่านข้อควรระวังในหน้านั้นก่อนเชื่อตัวเลขสำหรับการใช้งานจริง
- **คิว review** `alice review` แสดงสิ่งที่ Alice ตัดสินเองไม่ได้ (supersede, contradiction, merge, predicate, project, undo) ส่วน `alice review show <id>` กับ `alice review resolve <id> <choice>` ใช้ตอบ
- **ประวัติและ undo** ทุกการเปลี่ยนแปลงเป็น event: `alice history [id]` แสดงว่าใครเปลี่ยนอะไร `alice undo <event-id>` ย้อนกลับพร้อมกับการเปลี่ยนอื่นที่เกิดจากการกระทำเดียวกัน (เช่น merge หรือ supersede) ถ้าเป้าหมายเปลี่ยนไปแล้ว Alice จะเปิดรายการ review แทนการเขียนทับ การเปลี่ยนของ memory, fact และ predicate ย้อนได้ ส่วน memory ที่ถูก purge แล้วย้อนกลับมาไม่ได้

## Backup และ restore

```bash
alice backup now          # สำเนา alice.db ที่ตรวจแล้ว เป็นส่วนตัว ใน ~/.alice/backups
alice backup list
alice restore             # backup ล่าสุด หรือ: alice restore <ชื่อหรือ path> [--yes]
```

daemon ยังสำรองข้อมูลรายวันเองตอนว่าง โดยเก็บอันล่าสุดของแต่ละวันใน 7 วัน และของแต่ละสัปดาห์ใน 4 สัปดาห์ (`backup.daily`, `backup.weekly`) backup ที่สั่งเองเก็บ 5 อันล่าสุด สำเนาที่ทำก่อน migrate ฐานข้อมูลหรือก่อน restore เก็บ 30 วัน (อันล่าสุดของแต่ละชนิดเก็บไว้เสมอ) backup มีแต่ฐานข้อมูล ไม่มี `secrets.toml` หรือ token

`alice restore` ตรวจ backup ก่อน (ไฟล์ SQLite ที่ผ่าน `quick_check` และ schema ที่ build นี้อ่านได้ ถ้าใหม่กว่าจะปฏิเสธก่อนหยุดอะไรทั้งนั้น) แล้วหยุด daemon เก็บฐานข้อมูลปัจจุบันไว้เป็น `backups/alice-before-restore-<เวลา>.db` สลับ backup เข้าที่ด้วย rename ครั้งเดียว แล้วเริ่ม daemon ใหม่ ซึ่งสร้าง vector index ใหม่และนำงานเขียนที่ agent พักไว้ใน spool มาใช้ `alice.db` เป็นฐานข้อมูลเก่าที่สมบูรณ์หรือฐานข้อมูลที่กู้แล้วที่สมบูรณ์เสมอ ถ้าขั้นใดล้มเหลว ฐานข้อมูลเก่ายังอยู่ backup ที่ schema เก่ากว่าจะถูก migrate ตอนเริ่ม (พร้อม backup ก่อน migrate ของมันเอง) `secrets.toml`, `config.toml` และ spool ไม่ถูกแตะ ต้องมีที่ว่างพอสำหรับสำเนาที่เก็บไว้และไฟล์ที่กู้ ถ้าไม่พอจะปฏิเสธโดยไม่เปลี่ยนอะไร

**ความจำที่เขียนหลังเวลาที่ backup จะออกจากฐานข้อมูลที่ใช้งานอยู่** ความจำเหล่านั้นอยู่ในสำเนา before-restore ซึ่งเก็บอย่างน้อย 30 วัน (อันล่าสุดเก็บไว้เสมอ) ถ้าจะยกเลิกการ restore ให้รัน `alice restore alice-before-restore-<เวลา>.db` อีกสองอย่างที่ restore ไม่ย้อนให้: ความจำที่คุณ purge หลังทำ backup จะกลับมาพร้อม backup และข้อความที่เก็บไว้ภายใต้กฎ redaction รุ่นเก่าจะไม่ถูกสแกนใหม่

กับฐานข้อมูลใหญ่ daemon จะหยุดนานเท่ากับการคัดลอกสองครั้งและการตรวจ (หลายนาทีที่ 2.8 GB) ระหว่างนั้นงานเขียนของ agent ถูกพักใน spool ทันทีและนำมาใช้ภายหลัง

งาน maintenance ทำงานเองเมื่อ daemon ว่างมาหนึ่งนาที และหลีกทางทันทีที่มีคำขอเข้ามา: รายวันคือ backup, ตรวจความสมบูรณ์, ตรวจ vector index, นโยบายการเก็บ raw, ล้างถังขยะ และลบประวัติเก่า รายสัปดาห์คือ merge ของ full-text index และข้อเสนอให้รวม memory ที่คล้ายกัน รายชั่วโมงคือลองตัดสินใหม่กับ memory ที่ judge ล้มเหลว `alice maintenance status` แสดงว่าแต่ละงานรันล่าสุดเมื่อไร และ `alice maintenance run <job>` สั่งรันงานนั้นทันที

## สุขภาพระบบ token และ service

```bash
alice status                  # daemon, จำนวนข้อมูล, judge, review ที่ค้าง, backup ล่าสุด, หน่วยความจำที่ใช้
alice doctor                  # ทุกการตรวจ พร้อมบอกว่าควรทำอะไรต่อ; --json สำหรับสคริปต์; exit 1 เมื่อมีข้อที่ไม่ผ่าน
alice token rotate [--show]   # API token ใหม่; daemon ที่ทำงานอยู่เปลี่ยนตามทันที
alice service install         # ให้ daemon ทำงานตอน login
```

`alice doctor` ใช้ได้โดยไม่ต้องมี daemon (และไม่เปิด daemon ให้) ถ้าไม่มี daemon จะตรวจ `config.toml` กับตัวแปร `ALICE_*`, สิทธิ์ของโฟลเดอร์ข้อมูล, token และ `secrets.toml`, โมเดล embedding บนดิสก์ และดูว่ามี daemon หรือ service unit หรือไม่ ถ้ามี daemon จะตรวจเพิ่มเฉพาะสิ่งที่ daemon เท่านั้นที่เห็น ได้แก่ ฐานข้อมูล (read-only เมื่อดิสก์เต็ม), งาน maintenance และ backup, vector index, embedding ที่ล้มเหลวหรือที่นโยบาย cloud กันไว้, การ re-embed ที่กำลังทำ, judge (ไม่มี key ของ Jev, งบรายวัน), review ที่ค้าง, การเขียนที่รออยู่ใน spool, ค่าตั้งที่ daemon ปฏิเสธหรือรอรีสตาร์ท, หน่วยความจำ และพื้นที่ดิสก์ว่าง ทุกบรรทัดที่ไม่ปกติจะบอกว่าควรทำอะไรต่อ และแสดงกุญแจเพียงว่าตั้งไว้หรือไม่ ไม่แสดงค่า

`alice token rotate` เขียน token สุ่มใหม่ลง `~/.alice/token` (mode 0600 และแทนที่ไฟล์แบบ atomic) daemon ที่ทำงานอยู่จะเปลี่ยนใช้ทันทีและปฏิเสธ token เก่าตั้งแต่คำขอถัดไป `alice mcp` และ CLI จะพบ token ใหม่ในการเรียกครั้งถัดไป ถ้าไม่มี daemon จะแทนที่ไฟล์ขณะที่คำสั่งถือ daemon lock อยู่ และจะแสดง token ก็ต่อเมื่อใส่ `--show`

`alice service install` ลงทะเบียน launchd agent (macOS, `~/Library/LaunchAgents/dev.alice.daemon.plist`), systemd user unit (Linux, `~/.config/systemd/user/alice.service`) หรือ scheduled task ตอน logon (Windows ชื่อ `dev.alice.daemon` ยังไม่เคยลองบนเครื่อง Windows จริง) unit จะรัน `alice daemon run` กับโฟลเดอร์ข้อมูลเดียวกับที่ใช้ตอนสั่งคำสั่ง และไม่มีความลับอยู่ในนั้น `--dry-run` พิมพ์ไฟล์และคำสั่งโดยไม่เปลี่ยนอะไร ส่วน `--target-dir d` เขียนไฟล์ไว้ใต้ `d` และไม่รันคำสั่งใด บน Linux user service จะหยุดเมื่อ logout เว้นแต่ตั้ง `loginctl enable-linger $USER` `alice daemon stop` ปิด daemon อย่างเรียบร้อย unit จึงไม่เริ่มมันใหม่จนกว่าจะ login ครั้งหน้า `alice restore` หยุด unit ก่อนสลับฐานข้อมูลแล้วเริ่มใหม่ภายหลัง unit ที่เริ่ม daemon ใหม่เองจึงไม่เปิด daemon กลางการ restore `alice service uninstall` ถอด unit ออก

## Git sync *(วางแผนไว้ใน M4)*

```bash
alice sync init git@github.com:you/alice-memory.git
alice sync push
alice sync pull
```

memory แต่ละอันกลายเป็นไฟล์ `<project>/<kind>/<year>/<id>-<slug>.md` ที่มี YAML frontmatter ส่วน knowledge graph เก็บเป็น JSONL ที่เรียงลำดับแล้ว Alice merge ทีละ record สองเครื่องจึงแก้ memory คนละอันได้โดยไม่ชนกัน ถ้าแก้ memory อันเดียวกัน ฉบับที่ใหม่กว่าชนะ และอีกฉบับเข้าคิว review **ควรใช้ private repo** เพราะ memory อาจมีรายละเอียดของโปรเจกต์ที่อ่อนไหว แม้จะตัด secret ออกแล้วก็ตาม

## ย้ายมาจาก MemPalace

Alice ได้แรงบันดาลใจจาก [MemPalace](https://github.com/MemPalace/mempalace) และ import palace ของ MemPalace ได้ ความต่างหลักๆ:

| | MemPalace | Alice |
|---|---|---|
| ตัวโปรแกรม | Python package | binary ไฟล์เดียว |
| การเขียนพร้อมกัน | hub ที่ถือสิทธิ์เขียนได้ทีละคน | daemon เป็นเจ้าของ DB คนเดียว งานหนักไม่ถือสิทธิ์เขียน |
| การจัดระเบียบ | wing, room, drawer, closet, tunnel | project, topic, memory, source, link |
| ข้อมูลดิบกับความรู้ | เก็บรวมกัน | แยกชั้น ค้นความรู้ก่อน |
| การแก้ตัวเอง | agent ต้องเรียก tool เฉพาะเอง (เช่น ตรวจซ้ำ, KG supersede) | Judge ช่วยกันซ้ำ, supersede และส่งเรื่องขัดแย้งเข้า review อัตโนมัติ พร้อม undo |
| MCP tools | ~45 | หลัก 7 ตัว + ชุด admin และ compat ที่เลือกเปิดได้ |
| การแชร์ | shared-brain hub | git เป็น Markdown พร้อม merge ทีละ record |

## Import จาก MemPalace

```bash
alice import mempalace --dry-run   # รายงานว่าจะ import อะไรบ้าง ไม่เปลี่ยนแปลงอะไร
alice import mempalace             # import จริง (ถ้า daemon ยังไม่ทำงานจะถูกเปิดให้)
alice import status                # ความคืบหน้าของ import ที่กำลังรัน หรือรายงานครั้งล่าสุด
```

- **รันซ้ำได้อย่างปลอดภัย** drawer ทุกอันเก็บ id เดิมจาก MemPalace ไว้ รันซ้ำจึง import เฉพาะของใหม่ (`--since 2026-04-16` จำกัดเฉพาะ drawer ที่ใหม่กว่าวันที่ระบุ) ถ้า import หยุดกลางทาง หรือคุณกด Ctrl-C (daemon ยังทำงานต่อ) ก็รันคำสั่งเดิมอีกครั้งเพื่อทำต่อ
- **ไม่ต้องปิด MemPalace** Alice คัดลอก palace ผ่าน backup API ของ SQLite ไปไว้ในโฟลเดอร์ชั่วคราวส่วนตัวแล้วอ่านจากสำเนานั้น ไฟล์ palace ของคุณถูกเปิดแบบอ่านอย่างเดียวและไม่ถูกแก้ไข สำเนาใช้พื้นที่ดิสก์ว่างราวขนาดของ palace และถูกลบเมื่อเสร็จ
- **หา palace จากที่ไหน** `--palace <โฟลเดอร์>` หรือ `$MEMPALACE_PALACE_PATH` หรือ `$MEMPAL_PALACE_PATH` หรือ `palace_path` ใน `~/.mempalace/config.json` อ่านได้ทั้ง backend `sqlite_exact` และ `chroma` ใช้ `--collection <ชื่อ>` ถ้าไม่ใช่ collection `mempalace_drawers`
- **อะไรไปอยู่ที่ไหน**
  - drawer จาก transcript ของ session (`*.jsonl`, wing `sessions`) และจากไฟล์ที่ mine ไว้ กลายเป็นชั้นข้อมูลดิบ: หนึ่ง source ต่อหนึ่ง transcript หรือไฟล์ และ drawer เป็น chunk เรียงลำดับ ข้อความที่ซ้ำกันเป๊ะใน source เดียวกันจะรวมเป็นอันเดียว
  - diary และ drawer ที่ agent เขียนเอง (ไม่มีไฟล์ต้นทาง หรืออยู่ใน room ที่คัดมาแล้ว เช่น `decisions`, `runbook`) กลายเป็น memory โดยดู kind จากชื่อ room (`decisions` เป็น decision, `bug-classes` เป็น bugfix และอื่นๆ) บันทึกเป็นสถานะ active และไม่ผ่าน judge
  - wing กลายเป็น project (`wing_pg_ai` กับ `pg_ai` เป็น project เดียวกัน) wing ของ agent เช่น `wing_claude_code` ไปอยู่ที่ `global` โดยมี agent เป็นผู้เขียน เว้นแต่ข้อความระบุชื่อ project `--map-wing wing_project_alice=alice` (ใส่ซ้ำได้) แก้ project ของ wing ตอน import ครั้งแรก ส่วน `alice projects merge <from> <into>` แก้ภายหลัง (ดู "แก้ project ที่ผิด" ด้านล่าง) การ import ซ้ำจะเพิ่มเฉพาะของใหม่ และไม่ย้าย memory ที่ import ไปแล้วไปไว้ project อื่น: ถ้าจะย้ายให้ใช้ `alice projects merge` หรือ import เข้า home ใหม่
  - diary เลือก project ตามกฎเหล่านี้ ข้อแรกที่ตรงชนะ (`--map-wing` ชนะเสมอ): topic ของ diary เท่ากับชื่อ project ที่รู้จัก หรือขึ้นต้นด้วย `<project>-` (เว้นแต่ project ของ wing เองมีชื่อนั้นอยู่ในตัว เช่น `alice` ใน `project-alice`) wing แบบ project ระบุ project ของมัน wing ของ agent ที่ชื่อลงท้ายด้วย project ที่รู้จัก (`wing_claude_code_pgai`) ระบุ project นั้น จากนั้นดูโฟลเดอร์ของ transcript แล้วดู project ที่ข้อความระบุ แล้วจึง `global` ไม่มีการเดา `test` จาก diary
  - diary auto-save ของ MemPalace (topic `checkpoint`) ไม่ใช่ memory แต่เป็น transcript ที่ปลอมตัวมา จึง import เป็น raw chunk ของ session แยก source ตาม wing และวัน (`mempalace-checkpoint:<wing>:<yyyy-mm-dd>`, agent `mempalace checkpoint`) เรียงตามเวลาที่บันทึก และใช้ project ตามกฎของ diary ด้านบน ค้นเจอด้วย `recall --raw` (และผ่าน raw fallback เมื่อ memory ตรงน้อย) แต่ไม่โผล่ในผลค้นหา knowledge หรือใน briefing diary อื่นคง importance 2
  - MemPalace หั่น drawer ยาว (รวมถึง diary และ drawer ที่มี source file) เป็นชิ้นละ 800 ตัวอักษร id ลงท้ายด้วย `_chunk_000000`, `_chunk_000001` ฯลฯ การ import ต่อชิ้นของ drawer เดียวกันกลับเป็น memory เดียว (หรือ raw chunk เดียวสำหรับ checkpoint และ drawer ที่มี source file) ก่อนทำอย่างอื่น secret ที่ถูกตัดตรงรอยต่อจึงครบทั้งก้อนตอนตรวจหา: ชื่อเดียว ข้อความครบ เวลาที่บันทึกเร็วที่สุด และ topic ของทุกชิ้น id ของทุกชิ้นยังชี้มาที่มัน รวมถึง id ที่ MemPalace ตอบกลับสำหรับทั้ง drawer (ซึ่ง MemPalace ไม่ได้เก็บ drawer ไว้ใต้ id นั้น) home ที่ import ไว้ก่อนจะได้ id นี้ในการรันครั้งถัดไป และรายงานนับ `fragments joined` ไว้ drawer จะถูกรวมก็ต่อเมื่อไม่มี id ของชิ้นไหนถูกเก็บแล้ว drawer ที่เวอร์ชันก่อนเก็บทีละชิ้นไว้ (รวมถึงชิ้นของ raw file) จะคงเดิม (รายงานเตือนเมื่อขาดบางชิ้น) ถ้าต้องการให้ชิ้นเก่าถูกรวม ให้ import เข้า `ALICE_HOME` ใหม่
  - room กลายเป็น topic ส่วน closet ถูกทิ้ง (Alice สร้าง index เอง)
  - knowledge graph (`knowledge_graph.sqlite3` ในโฟลเดอร์ palace หรือไม่ก็ `~/.mempalace/`) กลายเป็น fact พร้อมช่วงเวลาที่ใช้ได้ ชื่อ predicate ที่คล้ายกันจะกลายเป็นรายการให้ review แทนการรวมเอง entity ที่ไม่มี fact ไม่ถูก import tunnel ที่ระบุ drawer สองอันที่ import ได้จะกลายเป็น link `relates_to`
- **แก้ project ที่ผิด** ถ้า wing ไปอยู่ผิด project หรือสอง project จริงๆ แล้วเป็นอันเดียวกัน (เช่น repository นี้โผล่เป็นทั้ง `alice` และ `project-alice`) ให้รัน `alice projects merge alice project-alice` คำสั่งนี้ย้าย memory, chunk, source และ fact ทั้งหมดของ project แรกไปไว้ใน project ที่สอง (fact ตามไปกับ memory ที่มันมาจาก) และทำให้ slug, key และ alias ของ project แรกกลายเป็นชื่อของ project ที่สอง การเขียนจาก checkout เดิมและการ import wing นั้นในภายหลังจึงไปลงใน project ที่คุณเก็บไว้ คำสั่งจะแสดงสิ่งที่จะย้ายและถาม `[y/N]` บน terminal สคริปต์ใช้ `--yes` และถ้าไม่มี terminal กับไม่มี `--yes` คำสั่งจะปฏิเสธ `global` ใช้เป็นต้นทางหรือปลายทางของการ merge ไม่ได้ `alice projects` แสดง slug ทั้งหมด การ merge เป็นหนึ่ง event: `alice undo <event-id>` (คำสั่งพิมพ์ให้ และดูได้ใน `alice history`) คืน project, ชื่อของมัน และแถวที่ย้ายไปเป๊ะๆ ใช้ได้ทาง CLI และ HTTP API (`POST /api/v1/projects/merge`) ไม่มีเป็นเครื่องมือ MCP เพื่อไม่ให้ agent จัด project ของคุณใหม่เอง
- **เป็นส่วนตัวโดยการออกแบบ** ทุกอย่างที่ import เขียนลงไปถูกปกปิด secret เสมอ ไม่ว่า `privacy.redact` จะตั้งไว้อย่างไร ไม่มีอะไรออกจากเครื่องและไม่มี judge ทำงาน เมื่อ import แล้ว memory เหล่านี้ก็เป็น memory ปกติ เวลาที่ MemPalace เก็บโดยไม่มี timezone จะถูกอ่านด้วย UTC offset ของเครื่องนี้ (รายงานแสดง offset ที่ใช้)
- **รายงาน** นับ drawer ทุกอัน: `drawers_seen` ต้องเท่ากับจำนวนที่ import, รวมเป็นอันเดียว, ข้าม หรือมีอยู่แล้ว ถ้าไม่เท่ากันจะนับเป็น error และจบด้วย exit code ที่ไม่ใช่ 0 รายงานยังแสดงแผนที่ wing ไป project, รายการที่รอ review และสิ่งที่ข้ามพร้อมเหตุผล
- **embedding คำนวณเบื้องหลัง** ด้วยโมเดลของ Alice เอง (ไม่ใช้ vector ของ MemPalace) ค้นหาด้วยคำได้ทันทีที่ import จบ ส่วนการค้นตามความหมายในชั้นข้อมูลดิบจะค่อยๆ ครบเมื่อ chunk ถูก embed
- **embedding provider ระยะไกล** ถ้า `embedding.provider` ไม่ใช่โมเดลในเครื่อง import จะไม่เริ่มทำงาน เพราะทุก chunk ของประวัติคุณจะถูกส่งไปยัง provider นั้น ใส่ `--allow-remote-embedding` เพื่อยืนยันว่ายอมรับได้

palace ขนาด 122,000 drawer import เสร็จในราว 2 นาที (ใช้หน่วยความจำสูงสุดไม่ถึง 200 MB) เมื่อไม่รวมการ embed และราว 4 นาทีเมื่อใช้โมเดลจริง ซึ่งตอนนั้นชั้นความรู้ embed เสร็จแล้ว ส่วน raw chunk อีกราว 116,000 อันจะ embed ต่อด้วยความเร็วราว 34 อันต่อวินาที การ import palace ที่ใหญ่มากในขณะที่มี agent ทำงานหนักอยู่หลายตัวอาจทำให้ `recall` ช้าลงจนกว่า import จะจบ ดู [docs/benchmarks/m3.md](docs/benchmarks/m3.md)

## ความเข้ากันได้กับ MemPalace

prompt และ hook ที่เขียนไว้สำหรับ MemPalace เรียก tool ชื่อ `mempalace_*` Alice ตอบ 15 ตัวจากข้อมูลของตัวเองในรูปแบบเดียวกับที่ MemPalace ส่งกลับ ได้แก่ `status`, `search`, `add_drawer`, `get_drawer`, `update_drawer`, `delete_drawer`, `diary_write`, `diary_read`, `kg_query`, `kg_add`, `kg_invalidate`, `kg_supersede`, `kg_timeline`, `list_wings` และ `list_rooms`

layer นี้คือการตั้งค่า `compat.mempalace` การ import จริง (ไม่ใช่ `--dry-run`) จะเปิดให้เองถ้าคุณไม่เคยตั้งค่านี้ มิฉะนั้นใช้:

```bash
alice config set compat.mempalace true     # หรือ false เพื่อซ่อน tool อีกครั้ง
```

การเปลี่ยนมีผลกับ client ที่เปิดอยู่ภายในหนึ่งวินาที ไม่ต้องรีสตาร์ท ถ้าติดต่อ daemon ไม่ได้ tool เหล่านี้จะถูกซ่อน และการเรียก tool เหล่านี้จะตอบว่าติดต่อ daemon ไม่ได้ (ให้รัน `alice status`) ไม่ใช่บอกว่าปิดอยู่

ลงทะเบียน `alice mcp` ด้วยชื่อ server ที่ prompt ของคุณคาดหวัง Claude Code ตั้งชื่อ tool เป็น `mcp__<server>__<tool>` ดังนั้น prompt ที่เรียก `mcp__plugin_mempalace_mempalace__mempalace_search` ต้องใช้ชื่อ server ว่า `plugin_mempalace_mempalace`:

```bash
claude mcp add --scope user plugin_mempalace_mempalace -- alice mcp
```

client อื่นให้ใช้ชื่อเดียวกันในไฟล์ตั้งค่าของมัน (`mcp_servers.plugin_mempalace_mempalace` ใน Codex เป็นต้น) ปิด plugin MemPalace ตัวจริงก่อน ไม่เช่นนั้นจะมีสอง server ใช้ชื่อเดียวกัน ลงทะเบียนอันเดียวก็พอ: มันให้ tool หลักของ Alice 7 ตัว และเมื่อเปิด layer ไว้ก็ให้ tool `mempalace_*` อีก 15 ตัว

สิ่งที่ควรรู้:

- `similarity` ในผลค้นหาเป็นค่าสัมพัทธ์: hit ที่ดีที่สุดได้ 1.0 ที่เหลือเทียบกับมัน คะแนนของ Alice ไม่ใช่ระยะ cosine ดังนั้น prompt ที่ตั้งเกณฑ์ตามระยะของ MemPalace จะทำงานต่างไป
- การเขียนผ่านเส้นทางปกติของ Alice: ปกปิด secret, ผ่าน judge, บันทึกใน `alice history` และ undo ได้ `wing` ที่ไม่ใช่ project ใน Alice จะเก็บ memory ไว้ใน project ปัจจุบัน (คำตอบบอกไว้) agent ไม่สร้าง project ด้วยวิธีนี้
- id ของ drawer จาก palace ของคุณยังใช้ได้ (`mempalace_get_drawer`, `update_drawer`, `delete_drawer`) รวมถึง id ของ drawer ที่ถูกรวมเข้ากับอันเดิมที่เหมือนกัน และ id ที่ MemPalace ตอบกลับสำหรับ entry ยาวที่มันเก็บเป็นชิ้นๆ การ get, update หรือ delete ผ่าน id ของ drawer ที่ถูกรวมไปแล้วจะทำกับ memory ที่มันถูกรวมเข้าไป และคำตอบจะบอกไว้
- tool อื่นของ MemPalace (`mine`, `traverse`, tunnel, `check_duplicate` ฯลฯ) ไม่ถูกแสดงในรายการ และถ้าเรียกจะได้ข้อความบอกว่าใน Alice ใช้อะไรแทน ไม่ใช่ error

## ประสิทธิภาพ

วัดตอนจบ M2 บน Apple M4 Pro ด้วย `alice bench` ([ผลทั้งหมด](docs/benchmarks/m2.md)) รันอย่างละหนึ่งครั้ง ค่า p95:

| | เป้าหมาย | 100k chunk, โมเดลในเครื่อง | 1M chunk, โมเดลในเครื่อง |
|---|---|---|---|
| `remember` ตอบรับ | < 50ms | 4.4ms | 13.4ms |
| `recall` (ชั้นความรู้) | < 200ms | 6.2ms | 15.6ms |
| `recall` แบบ `include_raw` | < 200ms | 26ms | 119ms |
| `context` | < 150ms | 0.43ms | 0.50ms |
| `alice mcp` เริ่มทำงาน (ตั้งแต่สั่งรันจนตอบ `initialize`) | < 20ms | 4.8ms (p50), 5.2ms (p95) | |
| หน่วยความจำของ daemon: RSS สูงสุด | RSS < 800MB ที่ 1M chunk | 534MB | **564MB ผ่านเป้า** |
| physical footprint สูงสุด | | 511MB | 585MB |

ตอนจบ M1 หน่วยความจำยังไม่ผ่านเป้า (คาดว่าราว 1.4GB เมื่อรวมโมเดล) M2 ทำให้ผ่านได้: vector ของ chunk ย้ายไปอยู่ใน index แบบ sign-bit ขนาด 53MB (กราฟ int8 เดิมใช้ 508MB) และโมเดล embedding รันบน ONNX Runtime โดยตรง (โหลดแล้วราว 350MB แทนที่จะเป็น 600MB) รอบ 1M chunk เติม index ด้วย vector สังเคราะห์ที่ใช้ id ของโมเดลจริง (การ embed ล้านชิ้นใช้เวลาหลายชั่วโมง และ vector กินที่ใน index เท่ากันไม่ว่าค่าข้างในจะเป็นอะไร) แล้วเปิด daemon จริงที่โหลดโมเดลจริง embed memory ใหม่ 256 อัน และรับ recall พร้อมกัน 64 คำขอ 4 ชุด (ไม่มี error) คุณภาพการค้นไม่ตก: ชุด gold ยังได้ MRR 1.000 โดยทั้ง 13 คำถามอยู่ใน top 3 และ recall@10 ของ chunk index เทียบกับ top 10 ที่แท้จริงอยู่ที่ 0.99 กับ passage จริง 96,000 ชิ้น

## Roadmap

| ช่วง | ขอบเขต | สถานะ |
|---|---|---|
| **SP1 · M1** | ที่เก็บข้อมูล, การค้นหาแบบผสม, daemon, MCP, CLI, HTTP API | เสร็จแล้ว (alpha) |
| **SP1 · M2** | Judge (กฎในเครื่อง + Jev), คิว review, undo, ตัด secret, backup และ restore, maintenance, settings ที่มีผลทันที, doctor, service install, embedding provider ผ่าน HTTP, build บน Windows, เป้าหน่วยความจำ 800MB | เสร็จแล้ว (alpha) |
| **SP1 · M3** | ตัว import จาก MemPalace และ compatibility layer | สร้างเสร็จแล้ว รอ review สุดท้าย (alpha) |
| **SP1 · M4** | Git sync | วางแผนแล้ว |
| **SP2** | เก็บและกลั่นความรู้อัตโนมัติ, `alice setup`, hook/plugin สำหรับทุก agent ที่รองรับ | วางแผนแล้ว |
| **SP3** | Web UI (graph, จัดการ memory, review, settings, sync, สุขภาพระบบ) แล้วตามด้วยแอป desktop | วางแผนแล้ว |

รายละเอียดการออกแบบ: [Alice Core design spec](docs/superpowers/specs/2026-10-13-alice-core-design.md) (ภาษาอังกฤษ)

## ร่วมพัฒนา

เปิด issue, ร่วมคุยเรื่อง spec หรือส่ง pull request ได้เลย workspace มี 4 crate: `alice-core` (โมเดลข้อมูล, ที่เก็บ, การค้นหา, judge), `alice-embed` (embedding provider), `alice-daemon` (writer, worker, HTTP API, MCP server, client) และ `alice` (ตัว binary, แพ็กเกจ `alice-memory`) ก่อนส่ง pull request ให้รัน:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## License

[MIT](LICENSE) Alice ได้แรงบันดาลใจจาก MemPalace (MIT, © MemPalace Contributors) โค้ดส่วนใดที่ดัดแปลงมาจะคงข้อความลิขสิทธิ์เดิมไว้
