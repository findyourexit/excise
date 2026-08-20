# Configuration

Excise resolves configuration in this order:

1. command-line options;
2. environment variables;
3. a versioned TOML file; and
4. built-in defaults.

Unknown keys, unsupported versions, invalid ranges, invalid enum values, and conflicting custom keys are errors.

## Select a file

Use `--config FILE` or `EXCISE_CONFIG`. Without either, Excise reads `config.toml` from the operating system's standard per-user configuration directory for the `excise` application when that file exists.

## TOML schema

```toml
version = 1

[scanner]
threads = 8
event_buffer = 256
apparent_size = false
cross_filesystems = false
exclusions = [".git/", "target/"]

[model]
process_memory_mib = 512

[runtime]
reduced_motion = false
theme = "excise-dark"
ascii = false
mouse = false
keymap = "vim"
format = "tui"
```

Custom movement requires four distinct, unmodified printable ASCII keys that do not replace normal-mode commands:

```toml
[runtime]
keymap = "custom"

[runtime.custom_keys]
left = "a"
down = "s"
up = "w"
right = "d"
```

`runtime.output` is valid only when `runtime.format` is `table` or `json`.

## Fields

| Field | Meaning | Valid values |
|---|---|---|
| `scanner.threads` | Scanner worker count | `1..=32` |
| `scanner.event_buffer` | Bounded worker-event capacity | `16..=4096` |
| `scanner.apparent_size` | Prefer logical length in the UI | boolean |
| `scanner.cross_filesystems` | Traverse beyond the starting filesystem | boolean |
| `scanner.exclusions` | Ordered gitignore-style patterns | string array |
| `model.process_memory_mib` | Whole-process memory envelope | at least 128 MiB and no more than detected memory |
| `runtime.reduced_motion` | Disable nonessential transitions | boolean |
| `runtime.theme` | Built-in semantic theme | run `excise --help` for names |
| `runtime.ascii` | Use ASCII symbols and borders | boolean |
| `runtime.mouse` | Enable mouse capture and selection | boolean |
| `runtime.keymap` | Movement preset | `vim`, `emacs`, `custom` |
| `runtime.format` | Output mode | `tui`, `table`, `json` |
| `runtime.output` | Noninteractive report destination | path |

The default memory envelope is 512 MiB or detected available memory when lower. Excise reserves 25% as process headroom and limits the model/index portion to 75%.

## Environment variables

| Variable | Corresponding setting |
|---|---|
| `EXCISE_CONFIG` | explicit configuration file |
| `EXCISE_ROOT` | scan root |
| `EXCISE_SCAN_THREADS` | `scanner.threads` |
| `EXCISE_EVENT_BUFFER` | `scanner.event_buffer` |
| `EXCISE_APPARENT_SIZE` | `scanner.apparent_size` |
| `EXCISE_CROSS_FILESYSTEMS` | `scanner.cross_filesystems` |
| `EXCISE_EXCLUDE` | semicolon-separated exclusion patterns |
| `EXCISE_MEMORY_MIB` | `model.process_memory_mib` |
| `EXCISE_REDUCED_MOTION` | `runtime.reduced_motion` |
| `EXCISE_THEME` | `runtime.theme` |
| `EXCISE_ASCII` | `runtime.ascii` |
| `EXCISE_MOUSE` | `runtime.mouse` |
| `EXCISE_KEYMAP` | `runtime.keymap` |
| `EXCISE_FORMAT` | `runtime.format` |
| `EXCISE_OUTPUT` | `runtime.output` |
| `NO_COLOR` | force monochrome rendering when present |

Boolean environment values accept `true`/`false`, `yes`/`no`, `on`/`off`, and `1`/`0`, case-insensitively.

## Deletion confirmation

`--disable-delete-confirmation` does not remove deletion guardrails. It enables a visible, session-only reduced confirmation mode. It is intentionally not available in the persistent configuration file or environment.
