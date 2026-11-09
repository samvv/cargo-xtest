# Command fixtures

A `kind = "command"` fixture runs your own setup and teardown commands, for
anything that isn't a container or a cluster. Command fixtures can only be
services (`uses`), not environments.

```toml
[fixture.greeter]
kind = "command"
setup = "./scripts/greeter-up.sh"
teardown = "rm -f {handle}"
```

```sh
#!/bin/sh
# scripts/greeter-up.sh
set -e
state=$(mktemp)
echo hi > "$state"
printf '{"handle": "%s", "env": {"GREETER_STATE": "%s"}}\n' "$state" "$state"
```

## Keys

| Key | Default | Meaning |
|---|---|---|
| `setup` | required | shell command that sets the fixture up |
| `teardown` | none | shell command that tears it down; `{handle}` is replaced by the handle from setup |

## The setup contract

`setup` runs on the test's host with `sh -c`, in the workspace root, inside
the run's sandbox. It sees the same paths as the tests.

When it's done, it prints one JSON object on stdout:

```json
{ "handle": "anything", "env": { "NAME": "value" } }
```

| Field | Meaning |
|---|---|
| `handle` | a string that identifies what was created, e.g. a path, a PID or an ID. Optional. |
| `env` | env vars the tests get. Optional. |

Printing nothing is fine too: no handle and no env vars. Setup fails if the
command exits with a non-zero status or prints something that isn't JSON.

## Teardown

`teardown` runs the same way as `setup`, after the fixture's
[scope](README.md#scopes) ends. `{handle}` is replaced by the handle,
shell-quoted, so `rm -f {handle}` is safe for any path. A failing teardown is
reported but doesn't fail any test.
