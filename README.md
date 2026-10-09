# rustranslator

Markdown translation where **the local model writes and the cloud model reviews** — so that only
the flagged residue ever needs to be read by a person.

One executable: it translates, it serves as an MCP server, and it needs no runtime beyond itself.

---

## Status — read before installing

**The MCP surface works; the document engine does not exist yet.** Version 0.1.0 ships:

| done | what |
|---|---|
| ✅ | the binary, the configuration, and the task-progress structure |
| ✅ | the MCP server, over **stdio** and **HTTP**, plus a systemd unit |
| ✅ | four tools: `echo`, `progress`, `translate`, `review` |
| ✅ | translation of a *piece* of text, with markdown protection, and a reviewer that returns a verdict |

**What is missing is the document engine:** walking a markdown file block by block, the resumable
registry, and assembling the translated document back together. Until that lands, `translate`
translates the text you hand it — it does not translate a file.

The differential test against a 262-block golden corpus is the gate that will prove the port;
it comes with the engine.

---

## How it works

Four stages, with a gate between them:

```
A. PARSER      the markdown becomes SHORT, semantically complete blocks:
               prose and list items become SENTENCES; a table cell becomes a cell;
               headings and code fences are left untouched.
                        |
B. TRANSLATOR  a local model (Ollama) translates the block, with links, URLs,
               inline code and placeholders protected as slots.
                        |
C. REVIEWER    a CLOUD model reads (source, translation) and answers
               {"ok": true} or {"ok": false, "correction": "..."}.
                        |
D. READING     only the flagged blocks need to be read by a person.
```

**Why two models.** A small, local, free translator gets roughly one block in seven wrong on its
own (measured: 13% on a 420-target document). The reviewer does not replace the translator — it
**measures the residue**, and only that residue reaches a human. The reviewer must be stronger
than the translator; using a local judge from another family was tried and **approved bad
translations**.

**Why short blocks.** Markdown prose is wrapped at ~78 columns, mid-sentence. Translating line by
line mangles the text; translating the whole document at once hallucinates. The block is the unit
that preserves meaning.

**Why slot protection.** A link, a URL, an inline snippet or a `{{placeholder}}` that goes through
the model comes back damaged — or missing. They are swapped for `[[0]]`-style slots before the
call and restored after, and the translation is re-asked if the model drops one.

---

## Install

### Debian package

```bash
sudo dpkg -i rustranslator_0.1.0_amd64.deb
```

The package installs the binary at `/usr/bin/rustranslator`, the service at
`/usr/lib/systemd/user/rustranslator.service`, and the manual as `rustranslator(1)`.

**The package does not enable the service by itself** — a package should not make that decision
for the user:

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
# the binary lands in target/release/rustranslator
```

Requires `cargo` and `rustc` 1.99 or newer.

---

## Command line

```bash
rustranslator config show                       # what the program REALLY read, token masked
rustranslator config show --config /other.json  # ignoring the search order
rustranslator mcp                               # MCP server over stdio (the client spawns it)
rustranslator mcp --transport http --address 127.0.0.1:9720
```

`config show` exists for a practical reason: when the program reads a configuration that is not
the one you edited, you need to **see** it. The originating path comes on the first line, and the
API key is masked — `sk-…c3f9`, never in full.

---

## The MCP API

The server implements the Model Context Protocol. Four tools. Every call is answered from disk
state or from the configured provider; nothing is held in memory between calls, so restarting the
service loses nothing.

### `echo`

Returns the text it received, plus the instant of the call and the `pid`. It exists to prove the
server answered *just now*, rather than a cached value.

| parameter | type | required | description |
|---|---|---|---|
| `text` | string | yes | the text that must come back, unchanged |

Returns a string: `"<text> | echo at <epoch> | pid <n>"`.

### `progress`

Reports how a document's translation is going, read **from the registry on disk**. It does not
depend on anything running in memory.

| parameter | type | required | description |
|---|---|---|---|
| `document` | string | yes | base name of the document, without extension (e.g. `README`) |
| `dir` | string | no | where the registry lives; if omitted, uses the configured `output` |

Returns JSON:

```json
{
  "summary": "262 processed (the total was never declared) · 227 approved · 35 corrected (13%) · unknown",
  "directory": "/home/…/.local/share/rustranslator/golden",
  "progress": {
    "document": "README",
    "targets": null,
    "processed": 262,
    "approved": 227,
    "corrected": 35,
    "undecided": 0,
    "unreadable": 0,
    "remaining": null,
    "percent": null,
    "correction_rate": 13.36,
    "situation": "unknown",
    "last_line": 286
  }
}
```

Two honesties are built into the design:

- `targets`, `remaining` and `percent` come back `null` when **nobody declared the total**. The
  program does not invent a denominator — it says it does not know. The total lives in
  `<base>.task.json`, which the submit tool will write.
- `unreadable` counts registry lines that could not be parsed. If it is above zero, the snapshot is
  **incomplete**, and it says so instead of rounding down in silence.

### `translate`

Translates one piece of text into the target locale.

| parameter | type | required | description |
|---|---|---|---|
| `text` | string | yes | the text to translate |
| `target` | string | no | locale code; if omitted, uses the configured `target` |

Protects whole markdown links, inline code, bare URLs and `{{placeholders}}`, and re-asks up to
three times if the model drops a slot. Returns the translated string, or a tool-level error
explaining exactly what failed.

### `review`

Checks a translation against its source.

| parameter | type | required | description |
|---|---|---|---|
| `source` | string | yes | the original text |
| `translation` | string | yes | the translation to check |
| `target` | string | no | locale code; if omitted, uses the configured `target` |

Returns JSON: `{"ok": true}`, or `{"ok": false, "correction": "..."}`, or
`{"ok": null, "error": "..."}` when the reviewer could not answer — a named third state rather
than a bare `null`.

### Connecting an agent

In a client's config (Hermes format):

```yaml
mcp_servers:
  rustranslator:
    url: http://127.0.0.1:9720/mcp     # HTTP: the systemd service (primary)
    command: /usr/bin/rustranslator    # stdio: the client spawns it, no service needed
    args: [mcp]
    enabled: true
    timeout: 30
```

**Both forms in one entry, on purpose:** with `url` present the client uses HTTP; with the service
down, `command`/`args` are still the stdio path. It is the same binary in both modes, with no
duplicated code.

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

**Each role points at whichever provider you want**, as long as it speaks the OpenAI dialect
(`/v1/chat/completions`). That includes local Ollama, Ollama Cloud, OpenRouter, Groq, vLLM, LM
Studio. An empty `api_key` means *"do not send the Authorization header"* — the right thing for a
local daemon that already holds its own credential.

| field | what it is |
|---|---|
| `target` | locale code: `pt-BR`, `en`, `es-419`. It goes straight into the prompt |
| `top_k`, `repeat_penalty` | knobs **only Ollama** accepts — which is why they live in the translator block |
| `response_format` | `json_object`, so the reviewer answers with structured JSON |
| `doc_destination` | `next_to_original` or a path; a call may override it |

### Search order — the first that exists wins

1. `--config <path>`
2. `$RUSTRANSLATOR_CONFIG`
3. `~/.config/rustranslator/config.json`
4. `/etc/rustranslator/config.json`

An unknown field in the JSON is an **error**, not silence: a `base_ur` typo fails on the spot,
naming the file and the line — instead of quietly falling back to a default while nobody
understands why the config did not take effect.

---

## Where things live

| path | what it is |
|---|---|
| `~/.config/rustranslator/config.json` | **your configuration, with the token** (0600) |
| `/etc/rustranslator/config.json` | system default, **with no secret in it** |
| `~/.local/share/rustranslator/` | working directory: registry, task state, report |
| `/usr/bin/rustranslator` | the binary |

### About the token

**The token does not go in `/etc`.** Two concrete reasons: `/etc` belongs to root — every token
change would need `sudo` — and files there are born world-readable (`0644`), so any user on the
machine would read it. The place for a secret is the user's own directory, with tight permissions.

The program helps: if the file it resolved holds a token and is more open than `0600`, it **warns**
(it does not refuse), naming the current mode and the fix. And when it prints the configuration in
force, the value comes out **masked**.

---

## Developing

```bash
cargo build --release
cargo test --release                              # 30 tests
bash packaging/build-deb.sh                       # builds the .deb
python3 testes/tier2_test.py                      # proves the server under a real MCP client
```

The Tier 2 test deserves a word: a server that answers a hand-written JSON-RPC script is **not
proven**. Differences in keepalive, lifecycle and version negotiation only show up under a real
client. The test uses the actual `mcp` library, exactly as an agent does.

---

## Roadmap

| phase | what | state |
|---|---|---|
| A | the Rust MCP server mould | ✅ |
| B | workspace, configuration, locale | ✅ |
| C | block parser and resumable registry | next |
| D | OpenAI-compatible HTTP client | ✅ |
| E | translate → review → record loop | pending |
| F | differential test against a 262-block golden corpus | pending |
| G | task MCP: `submit` / `status` / `result` / `cancel` | pending |

---

## License

MIT — see [LICENSE](LICENSE).

Djair Guilherme da Silva Junior <djair.jr@gmail.com>
