# Extensions

Dawngasm extensions are separate executables named `dawngasm-plugin-*`. They
add capabilities without being linked into the TUI: new audio hosts, alternate
feed sources, post filters, post enrichment, Cloudflare clearance and
user-invokable commands.

A plugin is a normal process. The host starts it, writes one JSON request to
stdin, closes stdin, and reads one JSON response from stdout. Rust has no stable
ABI, so this keeps plugins independent of the compiler and crate versions used
to build the TUI, and a crashing plugin cannot take the player down.

## Discovery

Executables named `dawngasm-plugin-*` are loaded from, in precedence order:

1. every directory in `DAWNGASM_PLUGIN_PATH`, split like `PATH`
2. `<exe dir>/plugins/` (the release archive layout)
3. `<exe dir>/` (so `cargo run` finds the bundled helper)
4. `<data dir>/plugins/`

Entries are sorted, so equal-priority plugins resolve deterministically, and
the same file reached through two directories is only loaded once. On Unix the
file must be executable; on Windows it must end in `.exe`.

Discovery runs once on first use. `Extensions::reload()` re-scans in-process;
`dawngasm-tui extensions list` always starts from a fresh scan.

## Manifest

`plugin --describe` must print one JSON object:

```json
{
  "protocol": 2,
  "id": "example",
  "name": "Example extractor",
  "version": "1.0.0",
  "description": "Resolves example.com tracks.",
  "capabilities": [
    {
      "name": "extract",
      "methods": ["extract.resolve"],
      "hosts": ["example.com", "*.example.org"],
      "priority": 10,
      "override_builtin": false,
      "timeout_ms": 30000
    }
  ],
  "commands": [
    {"id": "hello", "title": "Say hello", "description": "Shows a toast."}
  ]
}
```

Fields:

- `protocol`: `1` or `2`. Missing means `1`.
- `id`: stable plugin id, used in errors and `extensions call`.
- `name`, `version`, `description`: display metadata.
- `capabilities`: objects, or the version 1 shorthand `["extract"]`.
- `commands`: commands exposed through the `:` palette. Each has an `id`, a
  `title`, a `description`, and an optional `key`. The host does not bind
  `key` yet; it is reserved so a manifest can suggest a future shortcut.
  Commands run from the palette, not from manifest keybindings.

A capability object accepts:

- `name`: `clearance`, `extract`, `feed`, `post.filter`, `post.enrich` or
  `command`.
- `methods`: methods it answers. Empty uses the capability's defaults. A
  non-empty list narrows dispatch: a `feed` plugin declaring only
  `["feed.tags"]` is never asked for `feed.fetch`.
- `hosts`: for `extract`, exact domains (`example.com`) or subdomain patterns
  (`.example.com`, `*.example.com`). Empty means "any host".
- `priority`: higher runs first when several plugins advertise the same
  capability. Default `0`.
- `override_builtin`: for `extract`, try this plugin before the built-in
  extractor even when the host is already supported.
- `timeout_ms`: per-call timeout. Defaults depend on the capability.

## Protocol

`plugin run` reads one request and writes one response:

```json
{"protocol":2,"id":"1234-0","method":"extract.resolve","params":{"url":"https://example.com/track"}}
{"protocol":2,"id":"1234-0","ok":true,"result":{"url":"https://cdn.example/track.m4a"}}
```

Failure:

```json
{"protocol":2,"id":"1234-0","ok":false,"error":{"code":"unsupported","message":"not my host","retryable":false}}
```

The host sends `"protocol": 1` to version 1 plugins and accepts the older
`"error": "message"` form. It also accepts a missing `id` from version 1
plugins, but new plugins should echo both fields.

A plugin can decline work in two ways:

- return `{"handled": false}` with `ok: true`
- return an error whose `code` is `unsupported`, `not_found` or `unhandled`

Both make the host try the next matching plugin.

## Capabilities

### `extract`

Method: `extract.resolve`

Params include the whole post: `id`, `name`, `url`, `selftext`, `title`,
`author`, `score`, `num_comments`, `created_utc`.

Result:

```json
{
  "url": "https://cdn.example/track.m4a",
  "headers": {"Referer": "https://example.com/"},
  "duration_ms": 180000
}
```

`url` must be a plain ranged HTTP media URL. `headers` are attached to every
ranged request. `duration_ms` is optional; it helps containers whose length the
demuxer cannot infer.

The host tries plugins by host specificity, then priority. For a host that a
built-in extractor already handles, only plugins with `override_builtin: true`
run first. Hotaudio stays built in because it needs a decrypting stream, not a
plain URL.

### `clearance`

Method: `clearance.capture`

Params (optional):

```json
{"target": "audiochan"}
```

When omitted, `target` defaults to `"hotaudio"`.

Result:

```json
{"cookie": "cf_clearance=...; other=...", "user_agent": "Mozilla/5.0 ..."}
```

The bundled `dawngasm-plugin-clearance` implements this with the system
webview.

Clearance is the one blocking capability: the host runs it synchronously on the
UI thread because the system webview must own the window and event loop,
especially on macOS. A hung clearance plugin can therefore freeze the TUI until
its timeout (330s by default); every other capability runs off the UI thread.

### `feed`

Methods: `feed.fetch`, `feed.tags`

`feed.fetch` params:

```json
{"sort":"top","period":"day","tags":"f4m","exclude_tags":"","query":"","page":0}
```

Result:

```json
{"posts":[{ "id":"...", "name":"t3_...", "title":"...", "url":"..." }], "has_more":true}
```

`feed.tags` params: `{"prefix":"fem"}`. Result is either
`{"tags":[{"tag":"femdom","count":42}]}` or the bare array.

If the plugin is missing, declines, or errors, the built-in dawngasm API is
used. This makes a feed plugin a full replacement rather than a requirement.

### `post.filter`

Method: `post.filter`

Params:

```json
{"posts":[...],"filters":{"tags":"f4m","exclude_tags":"","query":""}}
```

Result:

```json
{"keep":["t3_abc","t3_def"]}
```

Missing `keep` keeps everything. Every `post.filter` plugin runs in priority
order, and each one sees the posts the previous filter kept. The hook is
best-effort: an error leaves the page untouched.

### `post.enrich`

Method: `post.enrich`

Params: `{"posts":[...]}`

Result:

```json
{
  "annotations": {
    "t3_abc": {"badges":["HD","64k"], "tags":["ramblefap"]}
  }
}
```

Badges and `#tags` are rendered next to the host badge. Every `post.enrich`
plugin runs in priority order and their annotations merge. The hook is
best-effort.

### `command`

Method: `command.invoke`

Manifest commands are listed by `:` in the TUI. Params:

```json
{
  "id": "hello",
  "context": {
    "view": "Feed",
    "query": "",
    "tags": "",
    "blocked_tags": "",
    "post": {"id":"...","name":"t3_...","title":"..."}
  }
}
```

`post` is `null` when the list is empty. Result:

```json
{"message":"Hello from example.", "error": false}
```

`message` is shown as a toast. `error: true` renders it as an error.

## Limits

Every `--describe` and `run` call has a timeout and an output cap. A plugin
that hangs or floods its pipes is killed and reported as an error.

| Variable | Default | Meaning |
| --- | --- | --- |
| `DAWNGASM_PLUGIN_TIMEOUT_MS` | capability-dependent | global call timeout |
| `DAWNGASM_PLUGIN_DESCRIBE_TIMEOUT_MS` | `3000` | manifest timeout |
| `DAWNGASM_PLUGIN_MAX_OUTPUT` | `1048576` | stdout/stderr cap in bytes |

Capability defaults: `clearance` 330s, `feed` 60s, `extract` 30s, everything
else 15s. A capability's `timeout_ms` wins over the global default.

## CLI

```sh
# What was found, where it came from, and what failed to load
dawngasm-tui extensions list
dawngasm-tui extensions list --json

# Invoke a method by hand
dawngasm-tui extensions call clearance clearance.capture
dawngasm-tui extensions call example extract.resolve '{"url":"https://example.com/x"}'
```

`extensions call` accepts either a plugin id or a capability name.

## Minimal plugin

A shell plugin that handles one host and adds a command:

```sh
#!/bin/sh
case "$1" in
  --describe)
    cat <<'JSON'
{"protocol":2,"id":"example","name":"Example","version":"1.0.0",
 "capabilities":[{"name":"extract","hosts":["example.com"]},
                 {"name":"command"}],
 "commands":[{"id":"hello","title":"Say hello"}]}
JSON
    ;;
  run)
    request=$(cat)
    id=$(printf '%s' "$request" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
    case "$request" in
      *'"method":"extract.resolve"'*)
        printf '{"protocol":2,"id":"%s","ok":true,"result":{"url":"https://cdn.example/track.m4a"}}\n' "$id"
        ;;
      *'"method":"command.invoke"'*)
        printf '{"protocol":2,"id":"%s","ok":true,"result":{"message":"Hello from example."}}\n' "$id"
        ;;
      *)
        printf '{"protocol":2,"id":"%s","ok":false,"error":{"code":"unsupported","message":"unknown method"}}\n' "$id"
        ;;
    esac
    ;;
esac
```

Save it as `dawngasm-plugin-example`, make it executable, and put it in
`plugins/` next to the binary or in a `DAWNGASM_PLUGIN_PATH` directory.

## Trust

Extensions run as your user with your environment and filesystem access. There
is no signature check or sandbox. Only install plugins you trust, and prefer
directories you control over shared ones.
