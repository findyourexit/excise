# Configuration

Excise reads configuration in this order:

1. Command-line options
2. Environment variables
3. A versioned TOML file
4. Built-in defaults

Unknown keys, unsupported versions, invalid ranges, invalid choices, and conflicting custom keys cause a configuration error.

## Select A File

Use `--config FILE` or `EXCISE_CONFIG` to choose a file. Without either, Excise reads `config.toml` from the operating system's standard per-user configuration directory when that file exists.

## TOML File

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

Custom movement requires four different, unmodified printable ASCII keys. The keys must not replace normal commands:

```toml
[runtime]
keymap = "custom"

[runtime.custom_keys]
left = "a"
down = "s"
up = "w"
right = "d"
```

`runtime.output` works only when `runtime.format` is `table` or `json`.

## Fields

| Field | Meaning | Valid values |
|---|---|---|
| `scanner.threads` | Number of scanner workers | 1 through 32 |
| `scanner.event_buffer` | Capacity of the worker event queue | 16 through 4096 |
| `scanner.apparent_size` | Prefer logical length in the interface | true or false |
| `scanner.cross_filesystems` | Traverse beyond the starting file system | true or false |
| `scanner.exclusions` | Ordered gitignore-style patterns | An array of strings |
| `model.process_memory_mib` | Whole-process memory limit | At least 128 MiB and no more than detected memory |
| `runtime.reduced_motion` | Disable nonessential transitions | true or false |
| `runtime.theme` | Built-in color theme | See `excise --help` for names |
| `runtime.ascii` | Use ASCII symbols and borders | true or false |
| `runtime.mouse` | Enable mouse selection | true or false |
| `runtime.keymap` | Movement preset | `vim`, `emacs`, or `custom` |
| `runtime.format` | Output mode | `tui`, `table`, or `json` |
| `runtime.output` | Report destination for noninteractive output | A path |

The default memory limit is 512 MiB or the detected available memory when that is lower. Excise reserves 25 percent as process headroom and limits working data to the remaining 75 percent.
By default, Excise uses one less than the detected available processor count, clamped from one through eight workers, so interactive input retains a processor when possible.

## Environment Variables

| Variable | Corresponding setting |
|---|---|
| `EXCISE_CONFIG` | Explicit configuration file |
| `EXCISE_ROOT` | Scan root |
| `EXCISE_SCAN_THREADS` | `scanner.threads` |
| `EXCISE_EVENT_BUFFER` | `scanner.event_buffer` |
| `EXCISE_APPARENT_SIZE` | `scanner.apparent_size` |
| `EXCISE_CROSS_FILESYSTEMS` | `scanner.cross_filesystems` |
| `EXCISE_EXCLUDE` | Exclusion patterns separated by semicolons |
| `EXCISE_MEMORY_MIB` | `model.process_memory_mib` |
| `EXCISE_REDUCED_MOTION` | `runtime.reduced_motion` |
| `EXCISE_THEME` | `runtime.theme` |
| `EXCISE_ASCII` | `runtime.ascii` |
| `EXCISE_MOUSE` | `runtime.mouse` |
| `EXCISE_KEYMAP` | `runtime.keymap` |
| `EXCISE_FORMAT` | `runtime.format` |
| `EXCISE_OUTPUT` | `runtime.output` |
| `NO_COLOR` | Force monochrome rendering when present |

Boolean environment values accept `true`, `false`, `yes`, `no`, `on`, `off`, `1`, and `0`, without regard to letter case.

## Deletion Confirmation

`--disable-delete-confirmation` does not remove deletion safeguards. It enables a visible, session-only reduced confirmation mode. It is intentionally unavailable in the persistent configuration file and environment.
