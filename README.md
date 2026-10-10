# rustranslator

Markdown translation where **the local model writes and the cloud model reviews** — so that only the
flagged residue ever needs to be read by a person.

One executable: it translates a document, it serves as an MCP server, and it needs no runtime beyond
itself.

---

## The models

This is the pair it was built and measured with, and naming them matters — the whole arrangement
depends on the two being different:

| role | model | where it runs |
|---|---|---|
| **translator** | **Tencent Hunyuan-MT** (`hy-mt2-7b`, Q4_K_M) — a model trained specifically for translation | locally, on your own machine |
| **reviewer** | a general-purpose cloud model, **stronger than the translator** (in the measured setup, `deepseek-v4.1-flash:cloud`) | any OpenAI-compatible endpoint |

Both are set in the configuration, one per role. The *server* in front of them is interchangeable:
the code speaks the OpenAI dialect at `/v1/chat/completions` and does not care whether that is
Ollama, vLLM, LM Studio, OpenRouter or Groq.

**Why two models.** A small, local, free translator gets roughly one block in seven wrong on its own
(measured: 13% over a 420-block document). The reviewer does not replace it — it **measures the
residue**, and only that residue reaches a human. The reviewer must be stronger than the translator:
a local judge from another family was tried and **approved bad translations**.

---

## How it works

Four stages, with a gate between them:

```
A. PARSE       the markdown becomes units of translation: a paragraph, a heading, a table cell.
               Code fences, raw HTML and structure are not units and are never touched.
                        |
B. TRANSLATE   the local model translates the unit, with links, URLs, inline code and
               placeholders protected as slots.
                        |
C. REVIEW      the cloud model reads (source, translation) and answers
               {"ok": true} or {"ok": false, "correction": "..."}.
                        |
D. READ        only the flagged units need to be read by a person. That is the residue.
```

**Why units, and not lines.** Markdown prose is wrapped at ~78 columns, often mid-sentence.
Translating line by line mangles the text (the model sees half a sentence and invents the rest);
translating the whole document at once makes it hallucinate. A paragraph is the unit that keeps the
meaning intact — and a long paragraph is cut at its sentence endings, because that is how much text
the model reads before it loses the thread.

**Why the translation is a splice.** Each unit is recorded with its exact byte span in the file, and
the translation replaces those bytes and nothing else. Every marker, every pipe, every indentation
and every code fence comes out exactly as it went in, because it was never copied anywhere. With
nothing translated, the document comes back byte for byte — and that property is a test.

**Why slot protection.** A link, a URL, an inline snippet or a `{{placeholder}}` that goes through
the model comes back damaged, or missing. They are swapped for `[[0]]`-style slots before the call
and restored afterwards, and the translation is re-asked if the model drops one.

**Raw HTML is left alone.** Inside an HTML block — a `<p align="center">` wrapper, a badge row — the
content is HTML, not markdown, so there is no unit there and it comes out untouched. The
alternative, which an earlier version of this program did, is to hand the model the surrounding tags
along with the text; a model that then rewrites `<a href="x">` breaks the page, which is worse than
leaving it in English.

---

## Install

### Debian package

```bash
sudo dpkg -i rustranslator_0.1.2_amd64.deb
```

The package installs the binary at `/usr/bin/rustranslator`, the service at
`/usr/lib/systemd/user/rustranslator.service`, and the manual as `rustranslator(1)`.

**The package does not enable the service by itself** — a package should not make that decision for
the user:

```bash
mkdir -p ~/.config/rustranslator
cp /usr/share/doc/rustranslator/config.example.json ~/.config/rustranslator/config.json
chmod 600 ~/.config/rustranslator/config.json     # this file will hold your token

systemctl --user daemon-reload
systemctl --user enable --now rustranslator.service
loginctl enable-linger "$USER"                    # so it survives logout
```

### From source

```bash
git clone https://github.com/djairjr/rustranslator
cd rustranslator
cargo build --release
```

Requires `cargo` and `rustc` 1.99 or newer.

---

## Command line

```bash
rustranslator translate README.md                  # the whole document
rustranslator translate README.md --limit 10       # ten blocks, to try it out
rustranslator translate README.md --from 40        # pick up where you stopped
rustranslator translate README.md --force          # redo everything, ignoring the registry
rustranslator parse README.md                      # what the parser sees, as JSON lines
rustranslator config show                          # what the program REALLY read, token masked
rustranslator mcp                                  # MCP server over stdio (the client spawns it)
rustranslator mcp --transport http --address 127.0.0.1:9720
```

`translate` writes the translated document next to the source as `<name>_<locale>.md` — `README.md`
becomes `README_pt-br.md`.

It is **resumable**: a block already in the registry is skipped, so stopping and starting again costs
nothing. Measured on a five-block document: 15.7 s on the first run, **0.003 s** on the second.

The exit code says what happened, so a script can branch on it: `0` everything was approved, `1`
something needs a person, `2` the run could not start.

---

## The MCP API

The server implements the Model Context Protocol and exposes seven tools. Every call is answered
from disk state or from the configured provider; nothing important is held in memory, so restarting
the service loses nothing.

### `submit` — translate a whole document, in the background

| parameter | type | required | description |
|---|---|---|---|
| `document` | string | yes | path to the markdown file |
| `target` | string | no | locale code; overrides the configured `target` |
| `dir` | string | no | where the registry and report live; defaults to the configured `output` |
| `force` | bool | no | translate every block again, ignoring the registry |
| `limit` | number | no | translate at most this many blocks; `0` means all |

Returns at once, with the job's name and the document's block count. It returns immediately on
purpose: a 400-block document runs for the better part of an hour, and an MCP call that ran for an
hour would be a hung conversation.

### `status` — how a document is going

| parameter | type | required | description |
|---|---|---|---|
| `document` | string | yes | base name, without the extension (e.g. `README`) |
| `dir` | string | no | where the registry lives |

Returns JSON, read from the registry on disk:

```json
{
  "summary": "262 processed (the total was never declared) · 227 approved · 35 corrected (13%)",
  "running": true,
  "progress": { "processed": 262, "approved": 227, "corrected": 35, "correction_rate": 13.35 }
}
```

Fields nobody has declared come back `null` rather than guessed: the program does not invent a
denominator it was not given, and it says so instead of rounding down in silence.

### `result` — the residue

Returns the blocks a person still has to read: those the reviewer corrected, those it could not
judge, and those whose translation was refused — each with its source, sorted by line. **Approved
blocks are not included.** That is the point of having a reviewer: if this returned the whole
document, the reviewer would have bought nothing.

### `cancel` — stop, keeping everything finished

The flag is read between blocks and never inside one, so nothing half-recorded is left behind. The
registry keeps every block that was done, and the next run continues from there.

### `translate` / `review` — one piece of text at a time

`translate` translates a single piece of text; `review` checks a single (source, translation) pair.
Useful on their own, and useful for diagnosing a provider.

### `echo` — living proof

Returns the text it was given, plus the instant of the call and the `pid`. It exists so a client can
tell "the server answered just now" apart from "the client returned a cached value".

### Connecting an agent

```yaml
mcp_servers:
  rustranslator:
    url: http://127.0.0.1:9720/mcp     # HTTP: the systemd service
    command: /usr/bin/rustranslator    # stdio: the client spawns it, no service needed
    args: [mcp]
    enabled: true
    timeout: 30
```

Both forms in one entry, on purpose: with `url` the client uses HTTP; with the service down,
`command`/`args` are still the stdio path. It is the same binary in both modes, with no duplicated
code.

---

## Configuration

One JSON file, with a block per role:

```json
{
  "target": "pt-BR",
  "translator": {
    "base_url": "http://127.0.0.1:11434/v1",
    "api_key": "",
    "model": "hy-mt2-7b",
    "temperature": 0.0, "top_p": 0.8, "top_k": 20, "repeat_penalty": 1.05
  },
  "reviewer": {
    "base_url": "http://127.0.0.1:11434/v1",
    "api_key": "",
    "model": "deepseek-v4.1-flash:cloud",
    "temperature": 0.0, "response_format": "json_object"
  },
  "output": "~/.local/share/rustranslator",
  "doc_destination": "next_to_original"
}
```

| field | what it is |
|---|---|
| `target` | locale code: `pt-BR`, `en`, `es-419`. It goes straight into the prompt |
| `model` | the model's name **as the server knows it** — the Ollama tag `hy-mt2-7b` is Tencent's Hunyuan-MT |
| `top_k`, `repeat_penalty` | knobs **only Ollama's API** accepts, which is why they live in the translator block |
| `response_format` | `json_object`, so the reviewer answers with structured JSON |
| `doc_destination` | `next_to_original` or a path; a call may override it |

An empty `api_key` means *"do not send the Authorization header"* — the right thing for a local
daemon that already holds its own credential.

**An unknown field is an error, not silence.** A `base_ur` typo fails on the spot, naming the file
and the line, instead of quietly falling back to a default while nobody understands why the
configuration did not take effect.

### Search order — the first that exists wins

1. `--config <path>`
2. `$RUSTRANSLATOR_CONFIG`
3. `~/.config/rustranslator/config.json`
4. `/etc/rustranslator/config.json`

### The token

**The token does not go in `/etc`.** Two concrete reasons: `/etc` belongs to root, so every token
change would need `sudo`; and files there are born world-readable, so any user on the machine could
read it. The place for a secret is the user's own directory, with tight permissions. The program
helps: if the file it resolved holds a token and is more open than `0600`, it warns, naming the
current mode and the fix — and when it prints the configuration in force, the value comes out
**masked**.

---

## Files

| path | what it is |
|---|---|
| `~/.config/rustranslator/config.json` | your configuration, with the token (0600) |
| `/etc/rustranslator/config.json` | system default, holding no secret |
| `~/.local/share/rustranslator/<name>.pipeline.jsonl` | the registry: one JSON line per block |
| `~/.local/share/rustranslator/<name>.task.json` | what the launcher knows: block count, pid, state |
| `~/.local/share/rustranslator/<name>.review.md` | the report of flagged blocks |
| `/usr/bin/rustranslator` | the binary |

### The registry

One JSON line per block, appended as the work happens and flushed immediately — a process that dies
holds its work and its evidence in the same place:

```json
{"id":"6:9ae100dda933a22d","line":6,"source":"Run `./start-here.sh` and open the browser.","translation":"Execute `./start-here.sh` e abra o navegador.","ok":true,"final":"Execute `./start-here.sh` e abra o navegador."}
```

`ok` is the reviewer's verdict: `true` approved, `false` corrected (the `correction` carries the
replacement), `null` could not answer. **The keys are English, and they are a format, not a
message** — they never follow the locale, because a localized key would mean one registry per
locale and every row already written would stop being readable. Rows recorded before that rule was
settled carry the Portuguese keys (`linha`, `fonte`, `traducao`, `erro`); they are read through
aliases, so no history is orphaned and nothing has to be rewritten.

The `id` is a hash of the text with the line in front, and it does not move between runs. That is
what makes the resumption work. The registry this project replaced hashed with a function that is
randomised per process, so its ids could never match a second time and its resume silently re-did
everything — seventeen of its 262 rows are the same block recorded twice, under two ids.

---

## Developing

```bash
cargo build --release
cargo test --release                              # 57 tests
bash packaging/build-deb.sh                       # builds the .deb
python3 tests/parser_differential.py              # how the parser differs from the recorded corpus
python3 tests/tier2_test.py                       # proves the server under a real MCP client
```

The Tier 2 test deserves a word: a server that answers a hand-written JSON-RPC script is **not
proven**. Differences in keepalive, lifecycle and version negotiation only show up under a real
client, and the test uses the actual `mcp` library, exactly as an agent does.

---

## Credits

**The markdown parsing is not this project's work, and it should not be.** It is
[`markdown-rs`](https://github.com/wooorm/markdown-rs) by **[Titus Wormer](https://wooorm.com/)**
(`markdown` on crates.io, MIT): a CommonMark-compliant parser with GFM extensions, whose syntax tree
gives every node its exact byte span in the source.

Thank you. An earlier version of this program parsed markdown by hand. It got block quotes, nested
lists, setext headings and indented code blocks wrong, and quietly ate two characters out of indented
list items. Adopting a parser that had already settled those questions is what made the translation
a splice — replace exactly the bytes that hold text, and leave everything else alone — which in turn
made the round trip exact. That correction is the reason this works.

---

## License

MIT — see [LICENSE](LICENSE).

Djair Guilherme da Silva Junior <djair.jr@gmail.com>
