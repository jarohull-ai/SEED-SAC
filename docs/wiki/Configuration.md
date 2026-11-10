# Configuration

The example file is [`config.example.toml`](https://github.com/jarohull-ai/SEED-SAC/blob/main/config.example.toml). Copy it to `config/default.toml` and adjust the polling interval, resource thresholds, immune process names and L1 endpoint for the local machine.

```sh
cp config.example.toml config/default.toml
```

The tracked `config/default.toml` is a sample runtime configuration. Keep credentials out of configuration files committed to Git. The L1 endpoint should remain loopback for a local model server unless the operator deliberately configures another trusted endpoint.

## Event database

The event log defaults to:

- `$XDG_DATA_HOME/seed-sac/events.db` when `XDG_DATA_HOME` is set;
- `~/.local/share/seed-sac/events.db` otherwise.

The database directory is created when the writer starts. SQLite is built with the `bundled` feature and uses WAL mode.
